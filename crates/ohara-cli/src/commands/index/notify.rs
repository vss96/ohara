//! Tell live `ohara serve` daemons that an index changed under them.

use std::path::{Path, PathBuf};

/// Best-effort: notify every alive daemon that `repo_path` was re-indexed
/// (or pruned),
/// except `skip` (the daemon that ran a routed index already refreshed
/// its own caches).
///
/// Failures at any step (registry missing, daemon down, IPC error) are
/// silently discarded — the next `list_alive` call prunes stale records.
pub(crate) async fn notify_daemons_of_invalidation(repo_path: &Path, skip: Option<&Path>) {
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
    let sockets = sockets_to_notify(alive.into_iter().map(|d| d.socket_path).collect(), skip);
    for socket in sockets {
        let req = Request {
            id: 1,
            repo_path: Some(repo_path.to_string_lossy().to_string()),
            method: RequestMethod::InvalidateRepo,
        };
        // Best-effort. Daemon down → next list_alive prunes it.
        let _ = Client::connect(&socket).call(req).await;
    }
}

/// Every socket in `alive` except `skip`.
fn sockets_to_notify(alive: Vec<PathBuf>, skip: Option<&Path>) -> Vec<PathBuf> {
    alive
        .into_iter()
        .filter(|s| Some(s.as_path()) != skip)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_routing_daemon_is_not_notified_again() {
        let a = PathBuf::from("/s/a.sock");
        let b = PathBuf::from("/s/b.sock");
        let all = vec![a.clone(), b.clone()];
        assert_eq!(sockets_to_notify(all.clone(), None), all);
        assert_eq!(
            sockets_to_notify(all, Some(Path::new("/s/a.sock"))),
            vec![b]
        );
        assert!(sockets_to_notify(vec![a], Some(Path::new("/s/a.sock"))).is_empty());
    }
}
