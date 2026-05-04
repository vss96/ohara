pub mod discover;
pub mod spawn;
pub mod transport;
pub mod try_call;
pub use discover::{find_or_spawn_daemon, registry_path, DaemonHandle};
pub use spawn::{runtime_dir, spawn_daemon, SpawnedDaemon};
pub use transport::Client;
pub use try_call::try_daemon_call;
