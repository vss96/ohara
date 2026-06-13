use anyhow::{Context, Result};
use deadpool_sqlite::{Config, Hook, HookError, Manager, Metrics, Pool, Runtime};
use rusqlite::Connection;
use std::path::Path;
use std::sync::Once;

static VEC_AUTO_EXT_REGISTERED: Once = Once::new();
static VEC_AUTO_EXT_RC: std::sync::OnceLock<std::os::raw::c_int> = std::sync::OnceLock::new();

pub struct SqlitePoolBuilder {
    path: std::path::PathBuf,
}

impl SqlitePoolBuilder {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    pub async fn build(self) -> Result<Pool> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).context("create index dir")?;
        }
        // Register sqlite-vec as a sqlite auto-extension exactly once per process so every
        // connection (current and future, including ones the pool lazily opens) gets the
        // `vec0` virtual table and `vec_version()` SQL function.
        register_vec_auto_extension()?;
        let cfg = Config::new(&self.path);
        let manager = Manager::from_config(&cfg, Runtime::Tokio1);
        // Apply pragmas via a `post_create` hook so they run on every connection
        // the pool creates, not just the first checkout. Per-connection settings
        // like `synchronous`, `mmap_size`, `cache_size`, `temp_store`, and
        // `foreign_keys` do NOT persist on the database file — only `journal_mode=WAL`
        // does. Without this hook, lazily-created connections silently inherit
        // SQLite defaults for everything else.
        let pool = Pool::builder(manager)
            .config(cfg.get_pool_config())
            .runtime(Runtime::Tokio1)
            .post_create(Hook::async_fn(|conn, _: &Metrics| {
                Box::pin(async move {
                    conn.interact(|c| {
                        apply_pragmas(c)?;
                        // Sanity-check the auto-extension actually registered on this connection.
                        load_vec_extension(c)?;
                        install_sql_trace(c);
                        Ok::<_, anyhow::Error>(())
                    })
                    .await
                    .map_err(|e| HookError::message(format!("interact: {e}")))?
                    .map_err(|e| HookError::message(e.to_string()))?;
                    Ok(())
                })
            }))
            .build()
            .map_err(|e| anyhow::anyhow!("build pool: {e}"))?;
        Ok(pool)
    }
}

/// Install a per-connection trace callback that emits one
/// `tracing::trace!` event per executed SQL statement on target
/// `"ohara_storage::sql"`. The cost is gated by the subscriber's level
/// filter — when no subscriber listens on this target the callback is
/// effectively a no-op (one filter check per statement).
///
/// **Operational note:** rusqlite invokes the closure for every
/// statement regardless of whether tracing later discards the event,
/// so there is a constant per-statement cost (one virtual call + one
/// level-filter check) even with no subscriber. For hot indexing
/// loops this is in the noise, but if profiling ever points the
/// finger at SQL-trace overhead, gate the install behind an env var
/// (e.g. only register the callback when `RUST_LOG` mentions
/// `ohara_storage::sql`).
fn install_sql_trace(conn: &mut Connection) {
    conn.trace(Some(|sql: &str| {
        tracing::trace!(target: "ohara_storage::sql", sql);
    }));
}

pub(crate) fn apply_pragmas(c: &Connection) -> Result<()> {
    c.execute_batch(
        // busy_timeout=30000 overrides rusqlite's 5000ms default
        // (inner_connection.rs sets sqlite3_busy_timeout(5000) at open).
        // The parallel indexer runs ~num_cpus workers against one WAL
        // writer; under CoreML-fast embedding they finish together and
        // queue on the writer, and a clustered wave's tail can wait
        // several seconds. 30s sits comfortably above the worst-case
        // queue depth so writers serialize instead of dropping commits.
        "PRAGMA busy_timeout=30000;
         PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA mmap_size=268435456;
         PRAGMA cache_size=-64000;
         PRAGMA temp_store=MEMORY;
         PRAGMA foreign_keys=ON;",
    )?;
    Ok(())
}

/// Registers `sqlite3_vec_init` as a sqlite auto-extension. After this returns successfully,
/// every SQLite connection opened in the process has `vec_version()` and `vec0` available.
/// Idempotent across calls; the registration result is cached and replayed on subsequent calls.
pub(crate) fn register_vec_auto_extension() -> Result<()> {
    VEC_AUTO_EXT_REGISTERED.call_once(|| {
        let rc = unsafe {
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
                sqlite_vec::sqlite3_vec_init as *const ()
            )))
        };
        let _ = VEC_AUTO_EXT_RC.set(rc);
    });
    let rc = VEC_AUTO_EXT_RC
        .get()
        .copied()
        .unwrap_or(rusqlite::ffi::SQLITE_OK);
    if rc == rusqlite::ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(anyhow::anyhow!("sqlite3_auto_extension returned rc={rc}"))
    }
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
mod sql_trace_tests {
    use super::*;
    use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::{Context, SubscriberExt};
    use tracing_subscriber::{Layer, Registry};

    // --- global-subscriber capture infrastructure ---
    //
    // `interact` closures run in `spawn_blocking` threads that inherit the
    // global subscriber (not any thread-local `set_default`).  We must use
    // `set_global_default` so the callsite is registered with
    // `Interest::always()` for every thread in the process.

    type SqlSink = Mutex<Option<Arc<Mutex<Vec<String>>>>>;

    fn sql_sink() -> &'static SqlSink {
        static SINK: OnceLock<SqlSink> = OnceLock::new();
        SINK.get_or_init(|| Mutex::new(None))
    }

    struct SqlTraceLayer;

    impl<S: tracing::Subscriber> Layer<S> for SqlTraceLayer {
        fn on_event(&self, ev: &tracing::Event<'_>, _: Context<'_, S>) {
            if ev.metadata().target() != "ohara_storage::sql" {
                return;
            }
            struct V<'a>(&'a mut String);
            impl<'a> Visit for V<'a> {
                fn record_str(&mut self, f: &Field, v: &str) {
                    if f.name() == "sql" {
                        *self.0 = v.to_string();
                    }
                }
                fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
                    if f.name() == "sql" {
                        *self.0 = format!("{:?}", v);
                    }
                }
            }
            let mut sql = String::new();
            ev.record(&mut V(&mut sql));
            if let Some(sink) = sql_sink().lock().unwrap().as_ref() {
                sink.lock().unwrap().push(sql);
            }
        }
    }

    struct SqlGuard(#[allow(dead_code)] MutexGuard<'static, ()>);
    impl Drop for SqlGuard {
        fn drop(&mut self) {
            *sql_sink().lock().unwrap() = None;
        }
    }

    fn acquire_sql_collector() -> (Arc<Mutex<Vec<String>>>, SqlGuard) {
        static INSTALLED: OnceLock<()> = OnceLock::new();
        INSTALLED.get_or_init(|| {
            tracing::subscriber::set_global_default(Registry::default().with(SqlTraceLayer))
                .expect("global tracing subscriber set once");
        });

        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let lock_guard = LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let events: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        *sql_sink().lock().unwrap() = Some(Arc::clone(&events));
        (events, SqlGuard(lock_guard))
    }

    // ---

    #[tokio::test]
    async fn sql_trace_emits_events_when_target_is_enabled() {
        let (events, _guard) = acquire_sql_collector();

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.sqlite");
        let pool = SqlitePoolBuilder::new(&db).build().await.unwrap();
        let conn = pool.get().await.unwrap();
        conn.interact(|c| c.execute_batch("CREATE TABLE probe (id INTEGER); SELECT 1;"))
            .await
            .unwrap()
            .unwrap();

        let captured = events.lock().unwrap();
        assert!(
            captured.iter().any(|s| s.contains("CREATE TABLE probe")),
            "expected SQL trace event for CREATE TABLE; got {:?}",
            *captured
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pool_opens_and_pragmas_apply() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePoolBuilder::new(dir.path().join("idx.sqlite"))
            .build()
            .await
            .unwrap();
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
    async fn busy_timeout_overrides_rusqlite_default() {
        // Plan 31: rusqlite opens connections with a 5000ms busy_timeout
        // by default. Under CoreML-fast indexing, ~num_cpus workers
        // finish embedding together and queue on the single WAL writer;
        // each commit's write transaction (incl. vec0 inserts) can take
        // hundreds of ms, so the tail of a clustered wave exceeds 5s and
        // drops the commit. apply_pragmas must raise the timeout well
        // above the worst-case queue wait.
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePoolBuilder::new(dir.path().join("idx.sqlite"))
            .build()
            .await
            .unwrap();
        let conn = pool.get().await.unwrap();
        let timeout_ms: i64 = conn
            .interact(|c| {
                c.query_row("PRAGMA busy_timeout", [], |r| r.get(0))
                    .map_err(anyhow::Error::from)
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            timeout_ms, 30_000,
            "busy_timeout must be raised to 30s (was rusqlite's 5s default), got {timeout_ms}"
        );
    }

    #[tokio::test]
    async fn concurrent_writers_do_not_get_database_locked() {
        // Plan 31 regression: K overlapping write transactions against
        // one pooled DB must all commit. Pre-fix (busy_timeout=0) the
        // losers returned SQLITE_BUSY ("database is locked"); with the
        // pragma they serialize on SQLite's busy handler instead.
        let dir = tempfile::tempdir().unwrap();
        let pool = std::sync::Arc::new(
            SqlitePoolBuilder::new(dir.path().join("idx.sqlite"))
                .build()
                .await
                .unwrap(),
        );
        pool.get()
            .await
            .unwrap()
            .interact(|c| {
                c.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);")
                    .map_err(anyhow::Error::from)
            })
            .await
            .unwrap()
            .unwrap();

        let mut handles = Vec::new();
        for i in 0..8i64 {
            let pool = pool.clone();
            handles.push(tokio::spawn(async move {
                let conn = pool.get().await.unwrap();
                conn.interact(move |c| {
                    // BEGIN IMMEDIATE grabs the write lock up front, so
                    // overlapping tasks contend exactly the way indexer
                    // workers do.
                    c.execute_batch("BEGIN IMMEDIATE;")
                        .map_err(anyhow::Error::from)?;
                    c.execute(
                        "INSERT INTO t (id, v) VALUES (?, ?)",
                        (i, format!("row-{i}")),
                    )
                    .map_err(anyhow::Error::from)?;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    c.execute_batch("COMMIT;").map_err(anyhow::Error::from)?;
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .unwrap()
            }));
        }
        for h in handles {
            h.await
                .unwrap()
                .expect("every concurrent write must commit");
        }
        let count: i64 = pool
            .get()
            .await
            .unwrap()
            .interact(|c| {
                c.query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
                    .map_err(anyhow::Error::from)
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(count, 8, "all 8 writers must persist, none dropped");
    }

    #[tokio::test]
    async fn vec_extension_is_callable() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SqlitePoolBuilder::new(dir.path().join("idx.sqlite"))
            .build()
            .await
            .unwrap();
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
        let pool = SqlitePoolBuilder::new(dir.path().join("idx.sqlite"))
            .build()
            .await
            .unwrap();
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
        assert_eq!(
            sync_mode, 1,
            "synchronous must be NORMAL (1) on every pool connection"
        );
        assert!(
            !vec_v.is_empty(),
            "vec extension must be available on every pool connection"
        );
    }
}
