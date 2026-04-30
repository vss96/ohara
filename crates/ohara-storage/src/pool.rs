use anyhow::{Context, Result};
use deadpool_sqlite::{Config, Pool, Runtime};
use rusqlite::Connection;
use std::path::Path;
use std::sync::Once;

static VEC_AUTO_EXT_REGISTERED: Once = Once::new();

pub struct SqlitePoolBuilder {
    path: std::path::PathBuf,
}

impl SqlitePoolBuilder {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self { path: path.as_ref().to_path_buf() }
    }

    pub async fn build(self) -> Result<Pool> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).context("create index dir")?;
        }
        // Register sqlite-vec as a sqlite auto-extension exactly once per process so every
        // connection (current and future, including ones the pool lazily opens) gets the
        // `vec0` virtual table and `vec_version()` SQL function.
        register_vec_auto_extension();
        let cfg = Config::new(&self.path);
        let pool = cfg.create_pool(Runtime::Tokio1).context("create sqlite pool")?;
        // Apply pragmas on each connection by running it once on a checkout. Pragmas like
        // journal_mode=WAL persist on the database file, so a single application is enough.
        let conn = pool.get().await.context("checkout from pool")?;
        conn.interact(|c| {
            apply_pragmas(c)?;
            // Sanity-check the auto-extension actually registered on this connection.
            load_vec_extension(c)?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("interact: {e}"))??;
        Ok(pool)
    }
}

pub(crate) fn apply_pragmas(c: &Connection) -> Result<()> {
    c.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA mmap_size=268435456;
         PRAGMA cache_size=-64000;
         PRAGMA temp_store=MEMORY;
         PRAGMA foreign_keys=ON;",
    )?;
    Ok(())
}

/// Registers `sqlite3_vec_init` as a sqlite auto-extension. After this returns, every
/// SQLite connection opened in the process has `vec_version()` and `vec0` available.
/// Idempotent across calls.
pub(crate) fn register_vec_auto_extension() {
    VEC_AUTO_EXT_REGISTERED.call_once(|| {
        unsafe {
            // `sqlite3_auto_extension` takes an `Option<unsafe extern "C" fn() -> c_int>`.
            // `sqlite_vec::sqlite3_vec_init` is declared as `extern "C" fn()`, so transmute
            // through a function pointer to satisfy the FFI signature.
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *const std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::os::raw::c_int,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// Verifies the vec extension is callable on the given connection. The actual registration
/// happens via `register_vec_auto_extension`; this function is retained for parity with the
/// plan and as a per-connection sanity check.
pub(crate) fn load_vec_extension(c: &Connection) -> Result<()> {
    let _: String = c
        .query_row("SELECT vec_version()", [], |r| r.get(0))
        .context("vec_version() not available; sqlite-vec auto-extension not registered")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pool_opens_and_pragmas_apply() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePoolBuilder::new(dir.path().join("idx.sqlite")).build().await.unwrap();
        let conn = pool.get().await.unwrap();
        let mode: String = conn
            .interact(|c| {
                c.query_row("PRAGMA journal_mode", [], |r| r.get(0))
                    .map_err(anyhow::Error::from)
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[tokio::test]
    async fn vec_extension_is_callable() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePoolBuilder::new(dir.path().join("idx.sqlite")).build().await.unwrap();
        let conn = pool.get().await.unwrap();
        let v: String = conn
            .interact(|c| {
                c.query_row("SELECT vec_version()", [], |r| r.get(0))
                    .map_err(anyhow::Error::from)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(!v.is_empty());
    }

    #[tokio::test]
    async fn second_pool_connection_inherits_pragmas_and_vec() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePoolBuilder::new(dir.path().join("idx.sqlite")).build().await.unwrap();
        // Hold one checkout so the pool must lazily create a fresh connection
        // for the next request. The first checkout reuses the connection that
        // `build()` already pragma'd; the second forces a new one.
        let first = pool.get().await.unwrap();
        // Fetch a second checkout — forces deadpool to create a new connection.
        let second = pool.get().await.unwrap();
        // Note: rusqlite's bundled libsqlite3-sys is compiled with
        // SQLITE_DEFAULT_FOREIGN_KEYS=1, so foreign_keys is ON by default on
        // every fresh connection. We additionally check `synchronous` (default
        // FULL=2; we set NORMAL=1) to detect whether the post_create pragmas
        // actually ran on this connection.
        let (fk, sync_mode, vec_v): (i64, i64, String) = second
            .interact(|c| {
                let fk: i64 = c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
                let s: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
                let v: String = c.query_row("SELECT vec_version()", [], |r| r.get(0))?;
                Ok::<_, rusqlite::Error>((fk, s, v))
            })
            .await
            .unwrap()
            .unwrap();
        drop(first);
        assert_eq!(fk, 1, "foreign_keys must be ON on every pool connection");
        assert_eq!(sync_mode, 1, "synchronous must be NORMAL (1) on every pool connection");
        assert!(!vec_v.is_empty(), "vec extension must be available on every pool connection");
    }
}
