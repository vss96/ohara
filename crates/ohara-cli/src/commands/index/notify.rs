//! Tell live `ohara serve` daemons that an index changed under them.

/// Best-effort: notify every alive daemon that `repo_path` was re-indexed.
///
/// Failures at any step (registry missing, daemon down, IPC error) are
/// silently discarded — the next `list_alive` call prunes stale records.
pub(super) async fn notify_daemons_of_invalidation(repo_path: &std::path::Path) {
    use ohara_engine::client::{registry_path, Client};
    use ohara_engine::ipc::{Request, RequestMethod};
    use ohara_engine::registry::Registry;

    let Ok(reg_path) = registry_path() else {
        return;
    };
    let Ok(reg) = Registry::open(&reg_path) else {
        return;
    };
    let Ok(alive) = reg.list_alive() else {
        return;
    };
    for d in alive {
        let req = Request {
            id: 1,
            repo_path: Some(repo_path.to_string_lossy().to_string()),
            method: RequestMethod::InvalidateRepo,
        };
        // Best-effort. Daemon down → next list_alive prunes it.
        let _ = Client::connect(&d.socket_path).call(req).await;
    }
}
