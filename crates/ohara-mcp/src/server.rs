use anyhow::{Context, Result};
use ohara_engine::client::{
    find_or_spawn_daemon, registry_path, try_daemon_call, DaemonCallResult, DaemonHandle,
};
use ohara_engine::ipc::{Request, RequestMethod};
use ohara_engine::RetrievalEngine;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::OnceCell;

pub struct OharaServer {
    pub repo_path: PathBuf,
    /// Lazily-built in-process engine; only constructed when the daemon
    /// path is unavailable AND a tool call actually needs the engine.
    fallback: OnceCell<Arc<RetrievalEngine>>,
    use_daemon: bool,
}

impl OharaServer {
    /// Plan-29: no model loading here — boot is path canonicalisation.
    /// `OHARA_NO_DAEMON=1` pins the in-process fallback (CI, debugging).
    pub fn open<P: AsRef<Path>>(workdir: P) -> Result<Self> {
        let canonical = std::fs::canonicalize(workdir.as_ref()).context("canonicalize workdir")?;
        Ok(Self {
            repo_path: canonical,
            fallback: OnceCell::new(),
            use_daemon: std::env::var_os("OHARA_NO_DAEMON").is_none(),
        })
    }

    /// Test seam: a server that always uses `engine` in-process and never
    /// contacts a daemon (envelope-parity tests).
    pub fn with_engine(repo_path: PathBuf, engine: Arc<RetrievalEngine>) -> Self {
        Self {
            repo_path,
            fallback: OnceCell::new_with(Some(engine)),
            use_daemon: false,
        }
    }

    /// The in-process fallback engine. Lazy embedder + lazy reranker, so
    /// even building it loads no model; `explain_change` stays model-free.
    pub async fn engine(&self) -> &Arc<RetrievalEngine> {
        self.fallback
            .get_or_init(|| async {
                let embedder: Arc<dyn ohara_core::EmbeddingProvider> =
                    Arc::new(ohara_embed::LazyFastEmbedProvider::new());
                let reranker: Arc<dyn ohara_core::embed::RerankProvider> =
                    Arc::new(ohara_embed::LazyFastEmbedReranker::new());
                Arc::new(RetrievalEngine::new(embedder, reranker))
            })
            .await
    }

    /// Route one request to the shared daemon and report what happened.
    /// Callers decide via [`DaemonCallResult::route`]; anything but a
    /// served result means "use the in-process fallback" (except a
    /// `NeedsRebuild` refusal). `OHARA_DAEMON_SOCKET` skips discovery (tests).
    pub async fn daemon_call(&self, method: RequestMethod) -> DaemonCallResult {
        if !self.use_daemon {
            return DaemonCallResult::Unavailable;
        }
        let req = Request {
            id: 1,
            repo_path: Some(self.repo_path.to_string_lossy().to_string()),
            method,
        };
        if let Some(socket) = std::env::var_os("OHARA_DAEMON_SOCKET") {
            let h = DaemonHandle {
                socket_path: PathBuf::from(socket),
                pid: 0,
                spawned: false,
            };
            return try_daemon_call(move || Ok(Some(h)), req).await;
        }
        let registry = match registry_path() {
            Ok(r) => r,
            Err(e) => return DaemonCallResult::ConnectFailed(format!("registry_path: {e}")),
        };
        let current_exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => return DaemonCallResult::ConnectFailed(format!("current_exe: {e}")),
        };
        try_daemon_call(
            move || {
                find_or_spawn_daemon(
                    &current_exe,
                    env!("CARGO_PKG_VERSION"),
                    option_env!("OHARA_GIT_SHA").unwrap_or("unknown"),
                    &registry,
                    false,
                )
            },
            req,
        )
        .await
    }

    pub async fn serve_stdio(self) -> Result<()> {
        crate::tools::serve(self).await
    }
}

/// Compose a single hint string from the freshness state and the
/// compatibility verdict. Delegates to the canonical
/// `ohara_core::index_metadata::compose_hint` so the wording is kept
/// in one place.
pub fn compose_hint(
    st: &ohara_core::query::IndexStatus,
    compatibility: &ohara_core::index_metadata::CompatibilityStatus,
) -> Option<String> {
    ohara_core::index_metadata::compose_hint(st, compatibility)
}

// Tests for compose_hint and compose_hint wording are now in
// ohara-core::index_metadata::tests to avoid duplication.
// MCP-layer integration tests live in crates/ohara-mcp/tests/.
