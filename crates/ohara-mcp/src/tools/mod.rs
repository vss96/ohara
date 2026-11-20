pub mod explain_change;
pub mod find_pattern;
pub mod propose_decision;

use crate::server::OharaServer;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

/// Serve MCP over stdio. `harness` is `Some` only when the server was
/// started with `--harness-task`; it registers `propose_decision`.
pub async fn serve(
    server: OharaServer,
    harness: Option<propose_decision::HarnessCtx>,
) -> anyhow::Result<()> {
    let mut svc = find_pattern::OharaService::new(server);
    if let Some(ctx) = harness {
        svc = svc.with_harness(ctx);
    }
    svc.serve(stdio()).await?.waiting().await?;
    Ok(())
}
