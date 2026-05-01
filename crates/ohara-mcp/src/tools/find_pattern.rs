//! `find_pattern` MCP tool — searches the project's git history for prior
//! implementations that resemble a natural-language query.
//!
//! Adapted from the Task 18 plan for the rmcp 0.1.5 API surface (see
//! `tools/mod.rs` and the report for the specific deviations).

use crate::server::OharaServer;
use rmcp::{
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

pub const TOOL_DESCRIPTION: &str = "\
Search this project's git history for past implementations of similar logic.

USE WHEN the user:
  - asks \"how did we do X before\" / \"is there a pattern for Y\"
  - requests adding a feature similar to existing functionality
    (\"add retry like we did before\", \"make this look like the auth flow\")
  - is about to write code that likely has prior art in this repo

DO NOT USE for searching current code - use Grep/Read for that.
DO NOT USE for general programming questions.

Returns: historical commits with diffs, commit messages, file paths,
similarity score, and provenance (always INFERRED - semantic match).";

pub const SERVER_INSTRUCTIONS: &str = "\
Use this server when the user is implementing, modifying, or asking about \
code that likely has historical precedent in this repository. Lineage is \
ohara's specialty - for \"how was this done before\", \"trace this change\", \
or \"add a feature like an existing one\", prefer ohara over generic search. \
Do not use for code that has no git history (new files, fresh repos).";

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct FindPatternInput {
    /// Natural-language description of the pattern to find.
    pub query: String,
    /// Number of results to return (1..=20).
    #[serde(default = "default_k")]
    pub k: u8,
    /// Optional language filter (e.g. "rust", "python").
    #[serde(default)]
    pub language: Option<String>,
    /// Optional ISO date or relative ("30d") lower bound on commit age.
    #[serde(default)]
    pub since: Option<String>,
    /// Skip the cross-encoder rerank stage. Faster (no model invocation)
    /// at the cost of slightly lower precision on the top result.
    /// Defaults to false — rerank is on by default.
    #[serde(default)]
    pub no_rerank: bool,
}

fn default_k() -> u8 {
    5
}

#[derive(Clone)]
pub struct OharaService {
    server: Arc<OharaServer>,
}

impl OharaService {
    pub fn new(server: OharaServer) -> Self {
        Self {
            server: Arc::new(server),
        }
    }
}

#[tool(tool_box)]
impl OharaService {
    #[tool(description = TOOL_DESCRIPTION)]
    pub async fn find_pattern(
        &self,
        #[tool(aggr)] input: FindPatternInput,
    ) -> Result<CallToolResult, rmcp::Error> {
        let since_unix = parse_since(input.since.as_deref())
            .map_err(|e| rmcp::Error::invalid_params(e.to_string(), None))?;
        let q = ohara_core::query::PatternQuery {
            query: input.query,
            k: input.k.clamp(1, 20),
            language: input.language,
            since_unix,
            no_rerank: input.no_rerank,
        };
        let now = chrono::Utc::now().timestamp();
        let hits = self
            .server
            .retriever
            .find_pattern(&self.server.repo_id, &q, now)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;
        let meta = self
            .server
            .index_status_meta()
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;

        let body = json!({ "hits": hits, "_meta": meta });
        Ok(CallToolResult::success(vec![Content::text(
            body.to_string(),
        )]))
    }
}

#[tool(tool_box)]
impl ServerHandler for OharaService {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            instructions: Some(SERVER_INSTRUCTIONS.into()),
            ..Default::default()
        }
    }
}

fn parse_since(s: Option<&str>) -> anyhow::Result<Option<i64>> {
    let Some(s) = s else {
        return Ok(None);
    };
    if s.is_empty() {
        return Ok(None);
    }
    if let Some(stripped) = s.strip_suffix('d') {
        let n: i64 = stripped.parse()?;
        return Ok(Some(chrono::Utc::now().timestamp() - n * 86400));
    }
    let dt = chrono::DateTime::parse_from_rfc3339(s).or_else(|_| {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().fixed_offset())
    })?;
    Ok(Some(dt.timestamp()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_since_relative_days() {
        let out = parse_since(Some("30d")).unwrap().unwrap();
        let now = chrono::Utc::now().timestamp();
        assert!((now - 30 * 86400 - out).abs() < 5);
    }
    #[test]
    fn parse_since_iso_date() {
        let out = parse_since(Some("2024-01-01")).unwrap().unwrap();
        assert!(out > 1_700_000_000 && out < 1_800_000_000);
    }
    #[test]
    fn parse_since_none() {
        assert!(parse_since(None).unwrap().is_none());
        assert!(parse_since(Some("")).unwrap().is_none());
    }
}
