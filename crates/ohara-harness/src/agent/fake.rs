//! Scripted runtime for tests. Compiled only with `cfg(test)` or the
//! `test-util` feature.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use ohara_core::task::RuntimeName;

use super::{AgentOutcome, AgentRequest, AgentRuntime};
use crate::error::{Error, Result};

type EditFn = Box<dyn Fn(&AgentRequest) + Send + Sync>;

/// Pops one scripted outcome per `run`, after optionally editing the cwd.
pub struct FakeRuntime {
    name: RuntimeName,
    outcomes: Mutex<VecDeque<AgentOutcome>>,
    edit: Option<EditFn>,
    calls: Mutex<Vec<AgentRequest>>,
}

impl FakeRuntime {
    pub fn new(name: RuntimeName, outcomes: Vec<AgentOutcome>) -> Self {
        Self {
            name,
            outcomes: Mutex::new(outcomes.into()),
            edit: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Run `edit` on every call before the outcome is returned (e.g. to write
    /// files into `req.cwd`).
    pub fn with_edit(mut self, edit: impl Fn(&AgentRequest) + Send + Sync + 'static) -> Self {
        self.edit = Some(Box::new(edit));
        self
    }

    /// Requests received so far, in order.
    pub fn calls(&self) -> Vec<AgentRequest> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl AgentRuntime for FakeRuntime {
    fn name(&self) -> &RuntimeName {
        &self.name
    }

    async fn run(&self, req: AgentRequest) -> Result<AgentOutcome> {
        if let Some(edit) = &self.edit {
            edit(&req);
        }
        let poisoned = || Error::Runtime("fake runtime lock poisoned".to_string());
        self.calls.lock().map_err(|_| poisoned())?.push(req);
        self.outcomes
            .lock()
            .map_err(|_| poisoned())?
            .pop_front()
            .ok_or_else(|| {
                Error::Runtime(format!("fake runtime {}: script exhausted", self.name.0))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentStatus;
    use chrono::Utc;
    use ohara_core::task::Deadline;

    fn outcome(status: AgentStatus) -> AgentOutcome {
        AgentOutcome {
            status,
            final_message: String::new(),
            cost: None,
            exit_code: None,
        }
    }

    #[tokio::test]
    async fn scripted_outcomes_in_order_then_exhausted_and_edit_runs() {
        let dir = tempfile::tempdir().unwrap();
        let rt = FakeRuntime::new(
            RuntimeName("f".into()),
            vec![
                outcome(AgentStatus::RateLimited),
                outcome(AgentStatus::Completed),
            ],
        )
        .with_edit(|r| std::fs::write(r.cwd.join("edited"), "x").unwrap());
        let req = || AgentRequest {
            cwd: dir.path().to_path_buf(),
            prompt: "p".into(),
            mcp_config: None,
            deadline: Deadline(Utc::now()),
            transcript: dir.path().join("t"),
            model: None,
        };
        assert_eq!(
            rt.run(req()).await.unwrap().status,
            AgentStatus::RateLimited
        );
        assert!(dir.path().join("edited").exists());
        assert_eq!(rt.run(req()).await.unwrap().status, AgentStatus::Completed);
        assert!(rt.run(req()).await.is_err());
        assert_eq!(rt.calls().len(), 3);
    }
}
