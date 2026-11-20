//! Decision-trace records: what was chosen, why, and what evidence backs it.

use crate::task::{RuntimeName, TaskId};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use ulid::Ulid;

/// Unique id of a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DecisionId(pub Ulid);

impl DecisionId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl Default for DecisionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionState {
    Proposed,
    Settled,
    Rejected,
    Reopened,
    Superseded,
}

/// A belief the decision rests on; `check` is an argv that can re-verify it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assumption {
    pub text: String,
    pub check: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostIfWrong {
    Low,
    Medium,
    High,
}

/// A human's ruling on a settled decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Upheld,
    Overturned,
}

/// Result of one verification script.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptResult {
    pub name: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub tests_run: Option<u32>,
    pub missing_required: Vec<String>,
    pub log: PathBuf,
}

/// Evidence behind a decision. Judge fields arrive later as an additive change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub script_results: Vec<ScriptResult>,
    pub runtime: Option<RuntimeName>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub id: DecisionId,
    pub task_id: TaskId,
    pub round: u32,
    pub parent_ids: Vec<DecisionId>,
    pub question: String,
    pub options: Vec<String>,
    pub chosen: String,
    pub runner_up: Option<String>,
    pub assumptions: Vec<Assumption>,
    pub cost_if_wrong: CostIfWrong,
    pub checkpoint_sha: Option<String>,
    pub evidence: Evidence,
    pub state: DecisionState,
    pub rejected_reason: Option<String>,
    pub human_verdict: Option<Verdict>,
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
