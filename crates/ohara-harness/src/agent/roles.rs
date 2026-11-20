//! `RoleRunner`: run a role's configured runtimes in order with failover.

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use ohara_core::task::{Role, RuntimeName, StepRecord, StepState, TaskStore};

use super::{AgentOutcome, AgentRequest, AgentRuntime, AgentStatus};
use crate::config::{HarnessConfig, RuntimeSpec};
use crate::error::{Error, Result};

/// Runs a role's runtimes in configured order, failing over on
/// `RateLimited`, `AuthFailed` and `Crashed`.
pub struct RoleRunner {
    runtimes: BTreeMap<Role, Vec<Arc<dyn AgentRuntime>>>,
}

fn step_state(status: AgentStatus) -> StepState {
    match status {
        AgentStatus::Completed => StepState::Passed,
        AgentStatus::TimedOut => StepState::Aborted,
        AgentStatus::RateLimited | AgentStatus::AuthFailed | AgentStatus::Crashed => {
            StepState::Failed
        }
    }
}

/// A timeout means the clock ran out; a completion needs no retry.
fn should_fail_over(status: AgentStatus) -> bool {
    !matches!(status, AgentStatus::Completed | AgentStatus::TimedOut)
}

impl RoleRunner {
    pub fn new(
        config: &HarnessConfig,
        factory: impl Fn(&RuntimeName, &RuntimeSpec) -> Arc<dyn AgentRuntime>,
    ) -> Self {
        let mut runtimes = BTreeMap::new();
        for (role, names) in &config.roles.0 {
            let built = names
                .iter()
                .filter_map(|n| config.runtimes.get(n).map(|spec| factory(n, spec)))
                .collect();
            runtimes.insert(*role, built);
        }
        Self { runtimes }
    }

    /// Try each runtime for `role` in order. Each attempt is recorded as its
    /// own step with `seq = step.seq + attempt_index`; retries carry
    /// `failover_from` and are preceded by `reset`. An `Err` from a runtime
    /// is recorded and treated as `Crashed`. Returns the final attempt's
    /// outcome and step record.
    pub async fn run(
        &self,
        role: Role,
        req: AgentRequest,
        step: StepRecord,
        store: &dyn TaskStore,
        reset: impl Fn() -> Result<()>,
    ) -> Result<(AgentOutcome, StepRecord)> {
        let runtimes = self
            .runtimes
            .get(&role)
            .filter(|r| !r.is_empty())
            .ok_or_else(|| Error::Config(format!("no runtimes configured for role {role:?}")))?;
        let mut previous: Option<RuntimeName> = None;
        let mut last: Option<(AgentOutcome, StepRecord)> = None;
        for (attempt, rt) in (0u32..).zip(runtimes) {
            if previous.is_some() {
                reset()?;
            }
            let started_at = Utc::now();
            let outcome = match rt.run(req.clone()).await {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(runtime = %rt.name().0, error = %e, "runtime errored; treating as crashed");
                    AgentOutcome {
                        status: AgentStatus::Crashed,
                        final_message: e.to_string(),
                        cost: None,
                        exit_code: None,
                    }
                }
            };
            let record = StepRecord {
                seq: step.seq + attempt,
                runtime: rt.name().clone(),
                state: step_state(outcome.status),
                started_at,
                finished_at: Some(Utc::now()),
                cost: outcome.cost,
                failover_from: previous.take(),
                ..step.clone()
            };
            store.record_step(&record).await?;
            previous = Some(rt.name().clone());
            let done = !should_fail_over(outcome.status);
            last = Some((outcome, record));
            if done {
                break;
            }
        }
        last.ok_or_else(|| Error::Runtime("role runner made no attempt".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::fake::FakeRuntime;
    use crate::config::{RoleMap, RuntimeKind};
    use chrono::Utc;
    use ohara_core::task::decision::DecisionRecord;
    use ohara_core::task::{Deadline, StepState, TaskId, TaskRecord, TaskStore};
    use ohara_core::Result as CoreResult;
    use std::cell::Cell;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemStore {
        steps: Mutex<Vec<StepRecord>>,
    }

    #[async_trait::async_trait]
    impl TaskStore for MemStore {
        async fn create_task(&self, _t: &TaskRecord) -> CoreResult<()> {
            Ok(())
        }
        async fn update_task(&self, _t: &TaskRecord) -> CoreResult<()> {
            Ok(())
        }
        async fn get_task(&self, _id: &TaskId) -> CoreResult<Option<TaskRecord>> {
            Ok(None)
        }
        async fn list_tasks(&self) -> CoreResult<Vec<TaskRecord>> {
            Ok(vec![])
        }
        async fn heartbeat(&self, _id: &TaskId, _at: chrono::DateTime<Utc>) -> CoreResult<()> {
            Ok(())
        }
        async fn record_step(&self, s: &StepRecord) -> CoreResult<()> {
            let mut steps = self.steps.lock().unwrap();
            steps.retain(|x| (x.round, x.seq) != (s.round, s.seq));
            steps.push(s.clone());
            Ok(())
        }
        async fn list_steps(&self, _id: &TaskId) -> CoreResult<Vec<StepRecord>> {
            Ok(self.steps.lock().unwrap().clone())
        }
        async fn put_decision(&self, _d: &DecisionRecord) -> CoreResult<()> {
            Ok(())
        }
        async fn list_decisions(&self, _id: &TaskId) -> CoreResult<Vec<DecisionRecord>> {
            Ok(vec![])
        }
    }

    fn name(s: &str) -> RuntimeName {
        RuntimeName(s.into())
    }

    fn outcome(status: AgentStatus) -> AgentOutcome {
        AgentOutcome {
            status,
            final_message: format!("{status:?}"),
            cost: None,
            exit_code: None,
        }
    }

    fn config(names: &[&str]) -> HarnessConfig {
        let spec = RuntimeSpec {
            kind: RuntimeKind::Fake,
            model: None,
            command: None,
            prompt: None,
            done: None,
        };
        HarnessConfig {
            runtimes: names.iter().map(|n| (name(n), spec.clone())).collect(),
            roles: RoleMap(BTreeMap::from([(
                Role::Builder,
                names.iter().map(|n| name(n)).collect(),
            )])),
        }
    }

    fn request() -> AgentRequest {
        AgentRequest {
            cwd: "/tmp".into(),
            prompt: "p".into(),
            mcp_config: None,
            deadline: Deadline(Utc::now()),
            transcript: "/tmp/t".into(),
            model: None,
        }
    }

    fn step() -> StepRecord {
        StepRecord {
            task_id: TaskId::new(),
            round: 1,
            seq: 10,
            role: Role::Builder,
            runtime: name("placeholder"),
            model: None,
            state: StepState::Running,
            started_at: Utc::now(),
            finished_at: None,
            head_sha: Some("abc123".into()),
            cost: None,
            failover_from: None,
            transcript: "/tmp/t".into(),
            summary: None,
        }
    }

    /// Runner over fakes scripted per runtime name.
    fn runner(scripts: Vec<(&str, Vec<AgentOutcome>)>) -> (RoleRunner, Vec<Arc<FakeRuntime>>) {
        let names: Vec<&str> = scripts.iter().map(|(n, _)| *n).collect();
        let fakes: Vec<Arc<FakeRuntime>> = scripts
            .into_iter()
            .map(|(n, o)| Arc::new(FakeRuntime::new(name(n), o)))
            .collect();
        let by_name = fakes.clone();
        let r = RoleRunner::new(&config(&names), move |n, _| {
            by_name
                .iter()
                .find(|f| f.name() == n)
                .cloned()
                .map(|f| f as Arc<dyn AgentRuntime>)
                .expect("test runtime")
        });
        (r, fakes)
    }

    #[tokio::test]
    async fn rate_limited_primary_fails_over_and_records_it() {
        let (r, fakes) = runner(vec![
            ("claude", vec![outcome(AgentStatus::RateLimited)]),
            ("codex", vec![outcome(AgentStatus::Completed)]),
        ]);
        let store = MemStore::default();
        let s = step();
        let resets = Cell::new(0);
        let (out, last) = r
            .run(Role::Builder, request(), s.clone(), &store, || {
                resets.set(resets.get() + 1);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(out.status, AgentStatus::Completed);
        assert_eq!(resets.get(), 1);
        assert_eq!(fakes[1].calls().len(), 1);
        let steps = store.list_steps(&s.task_id).await.unwrap();
        assert_eq!(steps.len(), 2);
        let mut steps = steps;
        steps.sort_by_key(|x| x.seq);
        assert_eq!(steps[0].runtime, name("claude"));
        assert_eq!(steps[0].state, StepState::Failed);
        assert_eq!(steps[0].failover_from, None);
        assert_eq!(steps[1].runtime, name("codex"));
        assert_eq!(steps[1].state, StepState::Passed);
        assert_eq!(steps[1].failover_from, Some(name("claude")));
        assert_eq!(steps[1].seq, s.seq + 1);
        assert_eq!(last, steps[1]);
    }

    #[tokio::test]
    async fn timed_out_does_not_fail_over() {
        let (r, fakes) = runner(vec![
            ("claude", vec![outcome(AgentStatus::TimedOut)]),
            ("codex", vec![outcome(AgentStatus::Completed)]),
        ]);
        let store = MemStore::default();
        let s = step();
        let (out, last) = r
            .run(Role::Builder, request(), s.clone(), &store, || Ok(()))
            .await
            .unwrap();
        assert_eq!(out.status, AgentStatus::TimedOut);
        assert_eq!(last.state, StepState::Aborted);
        assert!(fakes[1].calls().is_empty());
        assert_eq!(store.list_steps(&s.task_id).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn all_fail_returns_last_outcome() {
        let (r, _) = runner(vec![
            ("claude", vec![outcome(AgentStatus::RateLimited)]),
            ("codex", vec![outcome(AgentStatus::AuthFailed)]),
            ("agy", vec![outcome(AgentStatus::Crashed)]),
        ]);
        let store = MemStore::default();
        let s = step();
        let (out, last) = r
            .run(Role::Builder, request(), s.clone(), &store, || Ok(()))
            .await
            .unwrap();
        assert_eq!(out.status, AgentStatus::Crashed);
        assert_eq!(last.runtime, name("agy"));
        assert_eq!(last.failover_from, Some(name("codex")));
        assert_eq!(store.list_steps(&s.task_id).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn runtime_error_counts_as_crash_and_fails_over() {
        // First fake has an empty script, so `run` returns Err.
        let (r, _) = runner(vec![
            ("claude", vec![]),
            ("codex", vec![outcome(AgentStatus::Completed)]),
        ]);
        let store = MemStore::default();
        let s = step();
        let (out, _) = r
            .run(Role::Builder, request(), s.clone(), &store, || Ok(()))
            .await
            .unwrap();
        assert_eq!(out.status, AgentStatus::Completed);
        let steps = store.list_steps(&s.task_id).await.unwrap();
        assert_eq!(steps.len(), 2);
        assert!(steps.iter().any(|x| x.state == StepState::Failed));
    }
}
