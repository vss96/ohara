pub mod find_pattern;

use crate::server::OharaServer;

pub async fn serve(_server: OharaServer) -> anyhow::Result<()> {
    // Task 18 wires this to rmcp's stdio transport with the find_pattern tool.
    // This stub returns an error so the binary fails clearly when run before Task 18 lands.
    anyhow::bail!("ohara-mcp serve: not yet implemented (Task 18)")
}
