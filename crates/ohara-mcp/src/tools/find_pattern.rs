//! find_pattern MCP tool — stub. Task 18 replaces this with the real implementation.

use crate::server::OharaServer;

#[allow(dead_code)]
pub struct OharaService {
    pub(crate) server: OharaServer,
}

#[allow(dead_code)]
impl OharaService {
    pub fn new(server: OharaServer) -> Self {
        Self { server }
    }
}
