//! `propose_decision` MCP tool — lets a harness agent record a decision it
//! made, with the options it weighed and the assumptions it rests on.
//!
//! The tool is only exposed when the server was started with
//! `--harness-task <id>`. It writes straight to the harness `TaskStore`
//! (never the retrieval index), so it works without the daemon. Every
//! decision lands as `Proposed` with no evidence: the supervisor settles or
//! rejects it later and attaches the evidence itself.

use ohara_core::task::decision::{
    Assumption, CostIfWrong, DecisionId, DecisionRecord, DecisionState, Evidence,
};
use ohara_core::task::{TaskId, TaskState, TaskStore};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Tool description shown to the agent.
pub const PROPOSE_TOOL_DESCRIPTION: &str = "\
Record a design decision you just made while working on this task.

USE WHEN you chose between two or more real alternatives (a library, a data
shape, an approach) and a reviewer would want to know why.

Provide the question, every option you weighed (at least 2), the one you
chose, and the assumptions the choice rests on. An assumption may carry a
`check` argv the supervisor re-runs at the end of the round; a failing check
rejects the decision.

Returns: { decision_id }. The decision is `proposed` until the supervisor
settles it.";

/// How costly it is if the decision turns out wrong.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CostInput {
    /// Cheap to undo.
    Low,
    /// Moderate rework if wrong.
    Medium,
    /// Expensive to undo.
    High,
}

impl From<CostInput> for CostIfWrong {
    fn from(c: CostInput) -> Self {
        match c {
            CostInput::Low => Self::Low,
            CostInput::Medium => Self::Medium,
            CostInput::High => Self::High,
        }
    }
}

/// One belief the decision rests on.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct AssumptionInput {
    /// Statement of the belief.
    pub text: String,
    /// Argv that re-verifies the belief; exit 0 means it still holds.
    ///
    /// Agent-controlled, so the supervisor must run it with no more
    /// authority than the agent has: argv only (never a shell), cwd fixed to
    /// the task worktree, under a deadline. Shape is bounded by
    /// [`MAX_CHECK_ARGS`] and [`MAX_CHECK_BYTES`].
    #[serde(default)]
    pub check: Option<Vec<String>>,
}

/// Input of the `propose_decision` tool.
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ProposeDecisionInput {
    /// The question being decided.
    pub question: String,
    /// Every option considered (at least 2).
    pub options: Vec<String>,
    /// The option chosen; must be one of `options`.
    pub chosen: String,
    /// Next-best option, if any.
    #[serde(default)]
    pub runner_up: Option<String>,
    /// Beliefs the decision rests on.
    #[serde(default)]
    pub assumptions: Vec<AssumptionInput>,
    /// Estimated cost of being wrong.
    pub cost_if_wrong: CostInput,
    /// Ids of earlier decisions this one builds on.
    #[serde(default)]
    pub parent_ids: Vec<String>,
}

/// Result of the `propose_decision` tool.
#[derive(Debug, Clone, Serialize)]
pub struct ProposeDecisionOutput {
    /// Id of the recorded decision.
    pub decision_id: String,
}

/// Why a proposal was refused.
#[derive(Debug, thiserror::Error)]
pub enum ProposeError {
    /// The input or the task state makes the proposal invalid.
    #[error("{0}")]
    Invalid(String),
    /// The task store failed.
    #[error("task store: {0}")]
    Store(#[from] ohara_core::OhraError),
}

impl From<ProposeError> for rmcp::Error {
    fn from(e: ProposeError) -> Self {
        match e {
            ProposeError::Invalid(m) => rmcp::Error::invalid_params(m, None),
            ProposeError::Store(s) => rmcp::Error::internal_error(s.to_string(), None),
        }
    }
}

/// The task this server records decisions for, and where to record them.
#[derive(Clone)]
pub struct HarnessCtx {
    /// Task the server was started for (`--harness-task`).
    pub task_id: TaskId,
    /// Harness task log.
    pub store: Arc<dyn TaskStore>,
}

impl HarnessCtx {
    /// Context for `task_id` writing to `store`.
    pub fn new(task_id: TaskId, store: Arc<dyn TaskStore>) -> Self {
        Self { task_id, store }
    }
}

fn parse_parent(raw: &str) -> Result<DecisionId, ProposeError> {
    serde_json::from_value(serde_json::Value::String(raw.to_string())).map_err(|e| {
        ProposeError::Invalid(format!("parent_ids: {raw:?} is not a decision id: {e}"))
    })
}

/// Most arguments an assumption `check` argv may have.
pub const MAX_CHECK_ARGS: usize = 64;

/// Most total bytes an assumption `check` argv may have.
pub const MAX_CHECK_BYTES: usize = 8 * 1024;

fn validate_check(text: &str, argv: &[String]) -> Result<(), ProposeError> {
    let bad = |why: &str| ProposeError::Invalid(format!("assumption {text:?} check: {why}"));
    match argv.first() {
        None => return Err(bad("argv is empty")),
        Some(p) if p.is_empty() => return Err(bad("program name is empty")),
        Some(_) => {}
    }
    if argv.len() > MAX_CHECK_ARGS {
        return Err(bad(&format!("more than {MAX_CHECK_ARGS} arguments")));
    }
    if argv.iter().map(String::len).sum::<usize>() > MAX_CHECK_BYTES {
        return Err(bad(&format!("more than {MAX_CHECK_BYTES} bytes in total")));
    }
    Ok(())
}

fn validate(input: &ProposeDecisionInput) -> Result<(), ProposeError> {
    if input.options.len() < 2 {
        return Err(ProposeError::Invalid(
            "options: a decision needs at least 2 options".into(),
        ));
    }
    if !input.options.contains(&input.chosen) {
        return Err(ProposeError::Invalid(format!(
            "chosen: {:?} is not one of the options",
            input.chosen
        )));
    }
    for a in &input.assumptions {
        if let Some(argv) = &a.check {
            validate_check(&a.text, argv)?;
        }
    }
    Ok(())
}

/// Validate `input` and record it as a `Proposed` decision of the task's
/// current round.
pub async fn propose(
    ctx: &HarnessCtx,
    input: ProposeDecisionInput,
) -> Result<ProposeDecisionOutput, ProposeError> {
    validate(&input)?;
    let parent_ids = input
        .parent_ids
        .iter()
        .map(|p| parse_parent(p))
        .collect::<Result<Vec<_>, _>>()?;
    let task = ctx
        .store
        .get_task(&ctx.task_id)
        .await?
        .ok_or_else(|| ProposeError::Invalid(format!("task {} not found", ctx.task_id)))?;
    if task.state != TaskState::Running {
        return Err(ProposeError::Invalid(format!(
            "task {} is {}, not running; decisions can only be proposed while it runs",
            ctx.task_id,
            task.state.as_str()
        )));
    }
    let id = DecisionId::new();
    let record = DecisionRecord {
        id,
        task_id: ctx.task_id,
        round: task.round,
        parent_ids,
        question: input.question,
        options: input.options,
        chosen: input.chosen,
        runner_up: input.runner_up,
        assumptions: input
            .assumptions
            .into_iter()
            .map(|a| Assumption {
                text: a.text,
                check: a.check,
            })
            .collect(),
        cost_if_wrong: input.cost_if_wrong.into(),
        checkpoint_sha: None,
        evidence: Evidence {
            script_results: Vec::new(),
            runtime: None,
        },
        state: DecisionState::Proposed,
        rejected_reason: None,
        human_verdict: None,
        verdict_note: None,
    };
    ctx.store.put_decision(&record).await?;
    Ok(ProposeDecisionOutput {
        decision_id: id.0.to_string(),
    })
}
