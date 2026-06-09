//! `find_pattern` MCP tool — searches the project's git history for prior
//! implementations that resemble a natural-language query.
//!
//! Adapted from the Task 18 plan for the rmcp 0.1.5 API surface (see
//! `tools/mod.rs` and the report for the specific deviations).
//!
//! `OharaService` also hosts the Plan 5 `explain_change` tool. Both
//! methods live on the same `#[tool(tool_box)] impl` block — rmcp
//! 0.1.5's macro only registers the methods on the impl block it
//! decorates, so splitting them across separate impl blocks would drop
//! the second tool from the registry.

use crate::server::OharaServer;
use crate::tools::explain_change::{ExplainChangeInput, EXPLAIN_TOOL_DESCRIPTION};
use ohara_core::count_lines;
use ohara_core::index_metadata::CompatibilityStatus;
use ohara_core::query_understanding::{parse_query, RetrievalProfile};
use ohara_engine::ipc::{ErrorCode, RequestMethod, Response};
use ohara_engine::FindPatternResult;
use rmcp::{
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, ServerHandler,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

/// The refusal envelope the in-process and daemon paths both produce when
/// the index is incompatible with the current runtime. Surfaced to the MCP
/// client as `invalid_params` so the agent can run the rebuild itself.
fn rebuild_refusal(detail: &str) -> rmcp::Error {
    rmcp::Error::invalid_params(
        format!(
            "find_pattern refuses to run: {detail}. \
             Run `ohara index --rebuild` in this repo first."
        ),
        None,
    )
}

fn map_engine_error(e: ohara_engine::EngineError) -> rmcp::Error {
    match e {
        ohara_engine::EngineError::NeedsRebuild { reason } => {
            rebuild_refusal(&format!("index needs rebuild ({reason})"))
        }
        other => rmcp::Error::internal_error(other.to_string(), None),
    }
}

/// Daemon response → typed result, mapping NeedsRebuild to the same
/// refusal envelope the in-process path produces.
fn decode_find_pattern(resp: Response) -> Result<FindPatternResult, rmcp::Error> {
    if let Some(err) = resp.error {
        if err.code == ErrorCode::NeedsRebuild {
            return Err(rebuild_refusal(&err.message));
        }
        return Err(rmcp::Error::internal_error(err.message, None));
    }
    let value = resp.result.ok_or_else(|| {
        rmcp::Error::internal_error("daemon response missing result".to_string(), None)
    })?;
    serde_json::from_value(value)
        .map_err(|e| rmcp::Error::internal_error(format!("decode FindPatternResult: {e}"), None))
}

/// Successful daemon response → typed value; anything else → None
/// (caller falls back in-process).
fn decode_ok<T: serde::de::DeserializeOwned>(resp: Response) -> Option<T> {
    if resp.error.is_some() {
        return None;
    }
    serde_json::from_value(resp.result?).ok()
}

/// Shared envelope builder — both paths produce the existing wire shape.
fn find_pattern_body(
    result: FindPatternResult,
    query_text: &str,
) -> Result<CallToolResult, rmcp::Error> {
    let parsed = parse_query(query_text);
    let profile = RetrievalProfile::for_intent(parsed.intent);
    let meta = result.meta;
    let body = json!({
        "hits": result.hits,
        "_meta": {
            "index_status": meta.index_status,
            "hint": meta.hint,
            "compatibility": meta.compatibility,
            "query_profile": {
                "name": profile.name,
                "explanation": profile.explanation,
            },
        }
    });
    Ok(CallToolResult::success(vec![Content::text(
        body.to_string(),
    )]))
}

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
            query: input.query.clone(),
            k: input.k.clamp(1, 20),
            language: input.language,
            since_unix,
            no_rerank: input.no_rerank,
        };

        // Daemon-first (plan-29): one shared engine across sessions.
        if let Some(resp) = self
            .server
            .daemon_call(RequestMethod::FindPattern(q.clone()))
            .await
        {
            return find_pattern_body(decode_find_pattern(resp)?, &input.query);
        }

        // In-process fallback (daemon disabled or unreachable). The engine
        // now guards NeedsRebuild itself, so the refusal still surfaces here.
        let engine = self.server.engine().await;
        let result = engine
            .find_pattern(&self.server.repo_path, q)
            .await
            .map_err(map_engine_error)?;
        find_pattern_body(result, &input.query)
    }

    #[tool(description = EXPLAIN_TOOL_DESCRIPTION)]
    pub async fn explain_change(
        &self,
        #[tool(aggr)] input: ExplainChangeInput,
    ) -> Result<CallToolResult, rmcp::Error> {
        // Resolve the line_end sentinel ("0" → file length) by reading
        // the workdir checkout. Defense-in-depth: the orchestrator and
        // the Blamer both clamp internally, but we still pass a real
        // upper bound so the schema's intent is honored.
        let line_start = if input.line_start == 0 {
            1
        } else {
            input.line_start
        };
        let line_end = if input.line_end == 0 {
            let on_disk = self.server.repo_path.join(&input.file);
            match std::fs::read_to_string(&on_disk) {
                Ok(s) => count_lines(&s).max(line_start),
                // File missing in workdir — let the orchestrator emit
                // the limitation note. Use line_start as the upper
                // bound so blame returns an empty Vec.
                Err(_) => line_start,
            }
        } else {
            input.line_end
        };

        let q = ohara_core::explain::ExplainQuery {
            file: input.file,
            line_start,
            line_end,
            k: input.k.clamp(1, 20),
            include_diff: input.include_diff,
            // Plan 12 Task 3.2: cap MCP responses to keep payload
            // size predictable. Clients that want enrichment can opt
            // in via the schema bump (separate task); CLI defaults to
            // include_related=true.
            include_related: false,
        };

        // Daemon-first: blame result + index status both from the shared
        // engine. Any miss → full in-process fallback below.
        if let Some(resp) = self
            .server
            .daemon_call(RequestMethod::ExplainChange(q.clone()))
            .await
        {
            let explain: Option<ohara_engine::ExplainResult> = decode_ok(resp);
            let meta: Option<ohara_core::query::ResponseMeta> =
                match self.server.daemon_call(RequestMethod::IndexStatus).await {
                    Some(m) => decode_ok(m),
                    None => None,
                };
            if let (Some(explain), Some(meta)) = (explain, meta) {
                let body = json!({
                    "hits": explain.hits,
                    "_meta": {
                        "index_status": meta.index_status,
                        "hint": meta.hint,
                        "explain": explain.meta,
                    }
                });
                return Ok(CallToolResult::success(vec![Content::text(
                    body.to_string(),
                )]));
            }
        }

        // In-process fallback (daemon disabled or unreachable). The lazy
        // engine loads no model just to blame + read index status.
        let engine = self.server.engine().await;
        let explain_result = engine
            .explain_change(&self.server.repo_path, q)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;

        // Derive index_meta for the _meta.index_status fields. Re-use the
        // engine's open_repo handle; index status requires a git walk + storage
        // query, so we compute it here rather than caching in the tool.
        let handle = engine
            .open_repo(&self.server.repo_path)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;

        let behind = ohara_git::GitCommitsBehind::open(&self.server.repo_path)
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;
        let st = ohara_core::query::compute_index_status(
            handle.storage.as_ref(),
            &handle.repo_id,
            &behind,
        )
        .await
        .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;

        let stored = handle
            .storage
            .get_index_metadata(&handle.repo_id)
            .await
            .map_err(|e| rmcp::Error::internal_error(e.to_string(), None))?;
        // Issue #40: adopt stored embed_input_mode so a
        // `--embed-cache=diff` index isn't mis-flagged as NeedsRebuild.
        let mut runtime = ohara_engine::current_runtime_metadata(ohara_core::EmbedMode::default());
        if let Some(stored_mode) = stored.components.get("embed_input_mode") {
            runtime.embed_input_mode = stored_mode.clone();
        }
        let compatibility = CompatibilityStatus::assess(&runtime, &stored);
        let hint = crate::server::compose_hint(&st, &compatibility);

        // Same response envelope shape as the pre-refactor code: top-level
        // `hits` + `_meta`, with `explain` placing its blame-specific
        // diagnostics under `_meta.explain`.
        let body = json!({
            "hits": explain_result.hits,
            "_meta": {
                "index_status": st,
                "hint": hint,
                "explain": explain_result.meta,
            }
        });
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
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| {
            d.and_hms_opt(0, 0, 0)
                .expect("invariant: 0,0,0 is a valid HMS")
                .and_utc()
                .fixed_offset()
        })
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

    // The `since` field reaches the public MCP `find_pattern` handler at
    // line ~93, where any error is surfaced as `rmcp::Error::invalid_params`
    // back to the agent. These cases lock in the inputs that MUST be
    // rejected so we never regress that envelope.
    #[test]
    fn parse_since_rejects_garbage() {
        assert!(parse_since(Some("garbage")).is_err());
    }

    #[test]
    fn parse_since_rejects_malformed_days_suffix() {
        assert!(parse_since(Some("abcd")).is_err());
        assert!(parse_since(Some("1.5d")).is_err());
        assert!(parse_since(Some("d")).is_err());
    }

    #[test]
    fn parse_since_rejects_invalid_iso_date() {
        assert!(parse_since(Some("2024-13-01")).is_err());
        assert!(parse_since(Some("2024-02-30")).is_err());
        assert!(parse_since(Some("not-a-date-at-all")).is_err());
    }

    #[test]
    fn parse_since_accepts_rfc3339() {
        let out = parse_since(Some("2024-06-15T12:00:00Z")).unwrap().unwrap();
        assert!(out > 1_700_000_000 && out < 1_800_000_000);
    }

    #[test]
    fn find_pattern_input_default_k_is_five() {
        let raw = r#"{"query":"x"}"#;
        let parsed: FindPatternInput = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.k, 5);
        assert!(parsed.language.is_none());
        assert!(parsed.since.is_none());
        assert!(!parsed.no_rerank);
    }

    #[test]
    fn find_pattern_input_rejects_negative_k() {
        let raw = r#"{"query":"x","k":-1}"#;
        assert!(serde_json::from_str::<FindPatternInput>(raw).is_err());
    }

    #[test]
    fn find_pattern_input_rejects_oversized_k() {
        // k is u8, so a value > 255 fails at deserialization. The 1..=20
        // clamp inside the handler is for in-range-but-extreme values; this
        // test guards the schema-level rejection that happens before that.
        let raw = r#"{"query":"x","k":1000}"#;
        assert!(serde_json::from_str::<FindPatternInput>(raw).is_err());
    }

    #[test]
    fn find_pattern_input_requires_query() {
        let raw = r#"{"k":3}"#;
        assert!(serde_json::from_str::<FindPatternInput>(raw).is_err());
    }

    #[test]
    fn decode_find_pattern_maps_needs_rebuild_to_refusal() {
        use ohara_engine::ipc::{ErrorCode, ErrorPayload, Response};
        let resp = Response {
            id: 1,
            result: None,
            error: Some(ErrorPayload {
                code: ErrorCode::NeedsRebuild,
                message: "index needs rebuild: embedding_model mismatch".into(),
            }),
        };
        let err = decode_find_pattern(resp).expect_err("must refuse");
        let msg = err.to_string();
        assert!(msg.contains("needs rebuild"), "got: {msg}");
        assert!(msg.contains("ohara index --rebuild"), "got: {msg}");
    }
}
