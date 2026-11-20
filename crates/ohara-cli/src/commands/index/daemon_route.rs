#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn routes_to_daemon_only_for_plain_incremental_with_a_live_daemon() {
        let sock = Some(PathBuf::from("/tmp/s"));
        assert!(matches!(
            choose_route(true, false, false, false, false, sock.clone()),
            Route::Daemon(_)
        ));
        // full index
        assert!(matches!(
            choose_route(false, false, false, false, false, sock.clone()),
            Route::InProcess
        ));
        // --force
        assert!(matches!(
            choose_route(true, true, false, false, false, sock.clone()),
            Route::InProcess
        ));
        // --rebuild
        assert!(matches!(
            choose_route(true, false, true, false, false, sock.clone()),
            Route::InProcess
        ));
        // --no-daemon
        assert!(matches!(
            choose_route(true, false, false, true, false, sock.clone()),
            Route::InProcess
        ));
        // tuned
        assert!(matches!(
            choose_route(true, false, false, false, true, sock),
            Route::InProcess
        ));
        // none alive
        assert!(matches!(
            choose_route(true, false, false, false, false, None),
            Route::InProcess
        ));
    }
}
