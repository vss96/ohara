//! Outcome-harness task records and the `TaskStore` seam.
//!
//! `ohara-core` defines the shapes and the trait; `ohara-storage` persists
//! them in `harness.sqlite`. Nested fields are stored as JSON, so every type
//! here derives `Serialize` / `Deserialize`.

/// Decision-trace records and their ids.
pub mod decision;

use crate::{OhraError, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use decision::DecisionRecord;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use ulid::Ulid;

/// Unique id of a harness task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TaskId(pub Ulid);

impl TaskId {
    /// Generates a new, unique task id.
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for TaskId {
    type Err = OhraError;
    fn from_str(s: &str) -> Result<Self> {
        Ulid::from_string(s)
            .map(Self)
            .map_err(|e| OhraError::InvalidArgument(format!("task id {s:?}: {e}")))
    }
}

/// Lifecycle state of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// Contract is being drafted; not yet submitted for approval.
    Drafting,
    /// Contract is waiting for a human to approve it.
    AwaitingApproval,
    /// Supervisor loop is running rounds.
    Running,
    /// Task finished with its checks satisfied.
    Succeeded,
    /// Task hit its deadline before succeeding.
    TimedOut,
    /// Task was aborted before completion.
    Aborted,
    /// Task was stopped by the user.
    Stopped,
}

impl TaskState {
    /// The snake_case name used in storage and serialization.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Drafting => "drafting",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::TimedOut => "timed_out",
            Self::Aborted => "aborted",
            Self::Stopped => "stopped",
        }
    }
}

impl FromStr for TaskState {
    type Err = OhraError;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "drafting" => Ok(Self::Drafting),
            "awaiting_approval" => Ok(Self::AwaitingApproval),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "timed_out" => Ok(Self::TimedOut),
            "aborted" => Ok(Self::Aborted),
            "stopped" => Ok(Self::Stopped),
            other => Err(OhraError::InvalidArgument(format!(
                "unknown task state {other:?}"
            ))),
        }
    }
}

/// The user's stated outcome for a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome(pub String);

/// Wall-clock deadline for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deadline(pub DateTime<Utc>);

/// Money, in cents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CostCents(pub u64);

/// Blake3 hash of the frozen contract, displayed (and serialized) as 64 hex chars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContractHash(pub [u8; 32]);

impl fmt::Display for ContractHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl FromStr for ContractHash {
    type Err = OhraError;
    fn from_str(s: &str) -> Result<Self> {
        let bytes = hex::decode(s)
            .map_err(|e| OhraError::InvalidArgument(format!("contract hash {s:?}: {e}")))?;
        let arr: [u8; 32] = bytes.try_into().map_err(|_| {
            OhraError::InvalidArgument(format!("contract hash {s:?}: not 32 bytes"))
        })?;
        Ok(Self(arr))
    }
}

impl Serialize for ContractHash {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ContractHash {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Name of a configured agent runtime.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RuntimeName(pub String);

/// One harness task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Task id.
    pub id: TaskId,
    /// The user's stated outcome.
    pub outcome: Outcome,
    /// Name of the workflow driving the task.
    pub workflow: String,
    /// Root of the repository the task runs in.
    pub repo_root: PathBuf,
    /// Commit the task branch was started from.
    pub base_sha: String,
    /// Branch the task works on.
    pub branch: String,
    /// Current lifecycle state.
    pub state: TaskState,
    /// Wall-clock deadline.
    pub deadline: Deadline,
    /// Spending limit, if one is set.
    pub cost_ceiling: Option<CostCents>,
    /// Money spent so far.
    pub cost_spent: CostCents,
    /// Hash of the frozen contract, once approved.
    pub contract_hash: Option<ContractHash>,
    /// Last heartbeat from the supervisor.
    pub heartbeat_at: Option<DateTime<Utc>>,
    /// Current round number.
    pub round: u32,
    /// When the task was created.
    pub created_at: DateTime<Utc>,
    /// When the task finished, if it has.
    pub finished_at: Option<DateTime<Utc>>,
    /// Free-form detail about the final state.
    pub detail: Option<String>,
}

/// Which part an agent plays in a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Drafts and freezes the contract.
    Contract,
    /// Plans the work for a round.
    Planner,
    /// Writes the code changes.
    Builder,
    /// Judges whether the round met the contract.
    Judge,
    /// Reviews the round's changes.
    Reviewer,
}

/// Outcome of a single step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// Step is in progress.
    Running,
    /// Step completed successfully.
    Passed,
    /// Step completed with a failure.
    Failed,
    /// Step was aborted before completion.
    Aborted,
}

/// One agent invocation within a task round.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepRecord {
    /// Task this step belongs to.
    pub task_id: TaskId,
    /// Round the step ran in.
    pub round: u32,
    /// Position of the step within its round.
    pub seq: u32,
    /// Role the agent played.
    pub role: Role,
    /// Runtime that executed the step.
    pub runtime: RuntimeName,
    /// Model used, if reported.
    pub model: Option<String>,
    /// Current state of the step.
    pub state: StepState,
    /// When the step started.
    pub started_at: DateTime<Utc>,
    /// When the step finished, if it has.
    pub finished_at: Option<DateTime<Utc>>,
    /// Git HEAD after the step, if recorded.
    pub head_sha: Option<String>,
    /// Cost of the step, if known.
    pub cost: Option<CostCents>,
    /// Runtime this attempt failed over from, if any.
    pub failover_from: Option<RuntimeName>,
    /// Path to the step's transcript.
    pub transcript: PathBuf,
    /// Short summary of the step's result.
    pub summary: Option<String>,
}

/// Persistence seam for harness tasks, steps and decisions.
#[async_trait]
pub trait TaskStore: Send + Sync {
    async fn create_task(&self, t: &TaskRecord) -> Result<()>;
    async fn update_task(&self, t: &TaskRecord) -> Result<()>;
    async fn get_task(&self, id: &TaskId) -> Result<Option<TaskRecord>>;
    /// Newest first.
    async fn list_tasks(&self) -> Result<Vec<TaskRecord>>;
    async fn heartbeat(&self, id: &TaskId, at: DateTime<Utc>) -> Result<()>;
    /// Upsert on `(task_id, round, seq)`.
    async fn record_step(&self, s: &StepRecord) -> Result<()>;
    async fn list_steps(&self, id: &TaskId) -> Result<Vec<StepRecord>>;
    /// Upsert on `id`.
    async fn put_decision(&self, d: &DecisionRecord) -> Result<()>;
    async fn list_decisions(&self, id: &TaskId) -> Result<Vec<DecisionRecord>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_round_trips_through_string() {
        let id = TaskId::new();
        let parsed: TaskId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
        assert!("not-a-ulid".parse::<TaskId>().is_err());
    }

    #[test]
    fn task_state_round_trips_every_variant() {
        for s in [
            TaskState::Drafting,
            TaskState::AwaitingApproval,
            TaskState::Running,
            TaskState::Succeeded,
            TaskState::TimedOut,
            TaskState::Aborted,
            TaskState::Stopped,
        ] {
            assert_eq!(s.as_str().parse::<TaskState>().unwrap(), s);
        }
        assert!("bogus".parse::<TaskState>().is_err());
    }

    #[test]
    fn contract_hash_displays_as_64_hex_chars() {
        let h = ContractHash([0xab; 32]);
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(s.starts_with("abab"));
    }

    #[test]
    fn contract_hash_json_round_trips_and_rejects_bad_strings() {
        let h = ContractHash([0x1f; 32]);
        let json = serde_json::to_string(&h).unwrap();
        assert_eq!(json, format!("\"{}\"", "1f".repeat(32)));
        assert_eq!(serde_json::from_str::<ContractHash>(&json).unwrap(), h);
        assert!(serde_json::from_str::<ContractHash>("\"zz\"").is_err());
        assert!(serde_json::from_str::<ContractHash>("\"abcd\"").is_err());
    }

    #[test]
    fn enums_serialize_snake_case_matching_as_str() {
        let json = serde_json::to_string(&TaskState::AwaitingApproval).unwrap();
        assert_eq!(json, "\"awaiting_approval\"");
        assert_eq!(
            serde_json::to_string(&TaskState::TimedOut).unwrap(),
            "\"timed_out\""
        );
        assert_eq!(
            serde_json::to_string(&Role::Builder).unwrap(),
            "\"builder\""
        );
        assert_eq!(
            serde_json::to_string(&StepState::Passed).unwrap(),
            "\"passed\""
        );
    }
}
