//! Decision-trace records: what was chosen, why, and what evidence backs it.

use crate::task::{RuntimeName, TaskId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use ulid::Ulid;

/// Unique id of a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DecisionId(pub Ulid);

impl DecisionId {
    /// Generates a new, unique decision id.
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl Default for DecisionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Lifecycle state of a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionState {
    /// Recorded by an agent, not yet checked by the supervisor.
    Proposed,
    /// Checked and accepted by the supervisor.
    Settled,
    /// Rejected by the supervisor.
    Rejected,
    /// Reopened for reconsideration.
    Reopened,
    /// Replaced by a later decision.
    Superseded,
}

/// A belief the decision rests on; `check` is an argv that can re-verify it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assumption {
    /// Statement of the belief.
    pub text: String,
    /// Argv that re-verifies the belief, if one exists.
    pub check: Option<Vec<String>>,
}

/// Estimated cost of a decision being wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostIfWrong {
    /// Cheap to undo.
    Low,
    /// Moderate rework if wrong.
    Medium,
    /// Expensive to undo.
    High,
}

/// A human's ruling on a settled decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The human agreed with the decision.
    Upheld,
    /// The human rejected the decision.
    Overturned,
}

/// Result of one verification script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptResult {
    /// Name of the script.
    pub name: String,
    /// Whether the script passed.
    pub passed: bool,
    /// Process exit code, if it ran to completion.
    pub exit_code: Option<i32>,
    /// Number of tests the script reported running, if known.
    pub tests_run: Option<u32>,
    /// Required items the script did not produce.
    pub missing_required: Vec<String>,
    /// Path to the script's log.
    pub log: PathBuf,
}

/// Evidence behind a decision. Judge fields arrive later as an additive change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Results of the verification scripts run for the decision.
    pub script_results: Vec<ScriptResult>,
    /// Runtime that produced the decision, if recorded.
    pub runtime: Option<RuntimeName>,
}

/// One recorded decision: the choice made, its alternatives, and its evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRecord {
    /// Decision id.
    pub id: DecisionId,
    /// Task the decision belongs to.
    pub task_id: TaskId,
    /// Round the decision was made in.
    pub round: u32,
    /// Earlier decisions this one builds on.
    pub parent_ids: Vec<DecisionId>,
    /// The question being decided.
    pub question: String,
    /// Options considered.
    pub options: Vec<String>,
    /// Option that was chosen.
    pub chosen: String,
    /// Next-best option, if recorded.
    pub runner_up: Option<String>,
    /// Beliefs the decision rests on.
    pub assumptions: Vec<Assumption>,
    /// Estimated cost of being wrong.
    pub cost_if_wrong: CostIfWrong,
    /// Git checkpoint the decision was settled at, if any.
    pub checkpoint_sha: Option<String>,
    /// Evidence backing the decision.
    pub evidence: Evidence,
    /// Current lifecycle state.
    pub state: DecisionState,
    /// Why the decision was rejected, if it was.
    pub rejected_reason: Option<String>,
    /// Human ruling, if one was given.
    pub human_verdict: Option<Verdict>,
    /// Note accompanying the human ruling.
    pub verdict_note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{RuntimeName, TaskId};
    use std::path::PathBuf;

    #[test]
    fn decision_record_serde_round_trip() {
        let d = DecisionRecord {
            id: DecisionId::new(),
            task_id: TaskId::new(),
            round: 1,
            parent_ids: vec![DecisionId::new()],
            question: "which store?".into(),
            options: vec!["sqlite".into(), "files".into()],
            chosen: "sqlite".into(),
            runner_up: Some("files".into()),
            assumptions: vec![Assumption {
                text: "sqlite available".into(),
                check: Some(vec!["sqlite3".into(), "--version".into()]),
            }],
            cost_if_wrong: CostIfWrong::Medium,
            checkpoint_sha: Some("abc123".into()),
            evidence: Evidence {
                script_results: vec![ScriptResult {
                    name: "tests".into(),
                    passed: true,
                    exit_code: Some(0),
                    tests_run: Some(4),
                    missing_required: vec![],
                    log: PathBuf::from("logs/tests.log"),
                }],
                runtime: Some(RuntimeName("claude".into())),
            },
            state: DecisionState::Settled,
            rejected_reason: None,
            human_verdict: Some(Verdict::Upheld),
            verdict_note: None,
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: DecisionRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
    }
}
