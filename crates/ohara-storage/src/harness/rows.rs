//! Column codecs and row mappers for `harness.sqlite`.
//!
//! Enum columns hold the serde snake_case string; times are unix
//! milliseconds; nested fields are JSON text.

use chrono::{DateTime, TimeZone, Utc};
use ohara_core::task::decision::{DecisionId, DecisionRecord};
use ohara_core::task::{
    ContractHash, CostCents, Deadline, Outcome, RuntimeName, StepRecord, TaskId, TaskRecord,
};
use rusqlite::Row;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::PathBuf;

pub(super) const TASK_COLS: &str = "id, outcome, workflow, repo_root, base_sha, branch, state, \
     deadline_ms, cost_ceiling, cost_spent, contract_hash, heartbeat_ms, round, created_ms, \
     finished_ms, detail";

pub(super) const STEP_COLS: &str = "task_id, round, seq, role, runtime, model, state, started_ms, \
     finished_ms, head_sha, cost, failover_from, transcript, summary";

pub(super) const DECISION_COLS: &str =
    "id, task_id, round, parent_ids, question, options, chosen, \
     runner_up, assumptions, cost_if_wrong, checkpoint_sha, evidence, state, rejected_reason, \
     human_verdict, verdict_note";

pub(super) fn to_ms(t: DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

fn from_ms(ms: i64) -> anyhow::Result<DateTime<Utc>> {
    Utc.timestamp_millis_opt(ms)
        .single()
        .ok_or_else(|| anyhow::anyhow!("timestamp {ms} out of range"))
}

pub(super) fn cents(c: CostCents) -> anyhow::Result<i64> {
    Ok(i64::try_from(c.0)?)
}

fn from_cents(v: i64) -> anyhow::Result<CostCents> {
    Ok(CostCents(u64::try_from(v)?))
}

/// Serialize a snake_case enum (or any JSON value) to its column text.
pub(super) fn enum_str<T: Serialize>(v: &T) -> anyhow::Result<String> {
    match serde_json::to_value(v)? {
        serde_json::Value::String(s) => Ok(s),
        other => anyhow::bail!("expected string-valued enum, got {other}"),
    }
}

fn parse_enum<T: DeserializeOwned>(s: String) -> anyhow::Result<T> {
    Ok(serde_json::from_value(serde_json::Value::String(s))?)
}

pub(super) fn json<T: Serialize>(v: &T) -> anyhow::Result<String> {
    Ok(serde_json::to_string(v)?)
}

fn conv(idx: usize, e: anyhow::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, e.into())
}

/// Read column `idx` and convert it, tagging failures with the column.
fn col<T, R>(
    row: &Row<'_>,
    idx: usize,
    f: impl FnOnce(R) -> anyhow::Result<T>,
) -> rusqlite::Result<T>
where
    R: rusqlite::types::FromSql,
{
    f(row.get::<_, R>(idx)?).map_err(|e| conv(idx, e))
}

fn opt<T, R>(
    row: &Row<'_>,
    idx: usize,
    f: impl FnOnce(R) -> anyhow::Result<T>,
) -> rusqlite::Result<Option<T>>
where
    R: rusqlite::types::FromSql,
{
    match row.get::<_, Option<R>>(idx)? {
        Some(v) => f(v).map(Some).map_err(|e| conv(idx, e)),
        None => Ok(None),
    }
}

fn parse_json<T: DeserializeOwned>(s: String) -> anyhow::Result<T> {
    Ok(serde_json::from_str(&s)?)
}

fn task_id(s: String) -> anyhow::Result<TaskId> {
    Ok(s.parse()?)
}

pub(super) fn task_from_row(r: &Row<'_>) -> rusqlite::Result<TaskRecord> {
    Ok(TaskRecord {
        id: col(r, 0, task_id)?,
        outcome: Outcome(r.get(1)?),
        workflow: r.get(2)?,
        repo_root: PathBuf::from(r.get::<_, String>(3)?),
        base_sha: r.get(4)?,
        branch: r.get(5)?,
        state: col(r, 6, parse_enum)?,
        deadline: Deadline(col(r, 7, from_ms)?),
        cost_ceiling: opt(r, 8, from_cents)?,
        cost_spent: col(r, 9, from_cents)?,
        contract_hash: opt(r, 10, |s: String| Ok(s.parse::<ContractHash>()?))?,
        heartbeat_at: opt(r, 11, from_ms)?,
        round: r.get(12)?,
        created_at: col(r, 13, from_ms)?,
        finished_at: opt(r, 14, from_ms)?,
        detail: r.get(15)?,
    })
}

pub(super) fn step_from_row(r: &Row<'_>) -> rusqlite::Result<StepRecord> {
    Ok(StepRecord {
        task_id: col(r, 0, task_id)?,
        round: r.get(1)?,
        seq: r.get(2)?,
        role: col(r, 3, parse_enum)?,
        runtime: RuntimeName(r.get(4)?),
        model: r.get(5)?,
        state: col(r, 6, parse_enum)?,
        started_at: col(r, 7, from_ms)?,
        finished_at: opt(r, 8, from_ms)?,
        head_sha: r.get(9)?,
        cost: opt(r, 10, from_cents)?,
        failover_from: r.get::<_, Option<String>>(11)?.map(RuntimeName),
        transcript: PathBuf::from(r.get::<_, String>(12)?),
        summary: r.get(13)?,
    })
}

fn decision_id(s: String) -> anyhow::Result<DecisionId> {
    Ok(DecisionId(s.parse()?))
}

pub(super) fn decision_from_row(r: &Row<'_>) -> rusqlite::Result<DecisionRecord> {
    Ok(DecisionRecord {
        id: col(r, 0, decision_id)?,
        task_id: col(r, 1, task_id)?,
        round: r.get(2)?,
        parent_ids: col(r, 3, parse_json)?,
        question: r.get(4)?,
        options: col(r, 5, parse_json)?,
        chosen: r.get(6)?,
        runner_up: r.get(7)?,
        assumptions: col(r, 8, parse_json)?,
        cost_if_wrong: col(r, 9, parse_enum)?,
        checkpoint_sha: r.get(10)?,
        evidence: col(r, 11, parse_json)?,
        state: col(r, 12, parse_enum)?,
        rejected_reason: r.get(13)?,
        human_verdict: opt(r, 14, parse_enum)?,
        verdict_note: r.get(15)?,
    })
}
