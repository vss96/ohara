pub mod find_pattern;

use crate::server::OharaServer;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

pub async fn serve(server: OharaServer) -> anyhow::Result<()> {
    let svc = find_pattern::OharaService::new(server);
    svc.serve(stdio()).await?.waiting().await?;
    Ok(())
}
