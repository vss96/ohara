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
