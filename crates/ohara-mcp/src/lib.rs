//! ohara-mcp: stdio MCP server exposing `find_pattern` and `explain_change` tools to
//! Claude Code / Cursor / Codex.
//!
//! Wraps `ohara_engine::RetrievalEngine`.
pub mod server;
pub mod tools;
