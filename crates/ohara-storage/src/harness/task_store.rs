//! `TaskStore` over `harness.sqlite`.

use super::migrations;
use super::rows::{
    cents, decision_from_row, enum_str, json, step_from_row, task_from_row, to_ms, DECISION_COLS,
    STEP_COLS, TASK_COLS,
};
use crate::codec::pool::SqlitePoolBuilder;
use crate::storage_impl::with_conn;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_sqlite::Pool;
use ohara_core::task::decision::DecisionRecord;
use ohara_core::task::{StepRecord, TaskId, TaskRecord, TaskStore};
use ohara_core::{OhraError, Result};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use std::path::Path;

/// SQLite-backed [`TaskStore`]. Several supervisors write the same file, so
/// every write transaction starts with `BEGIN IMMEDIATE` and relies on the
/// pool's `busy_timeout` to queue behind other writers.
pub struct SqliteTaskStore {
    pool: Pool,
}

impl SqliteTaskStore {
    /// Open (creating if needed) the database at `path` and apply
    /// `harness_migrations/`.
    pub async fn open(path: &Path) -> Result<Self> {
        let pool = SqlitePoolBuilder::new(path)
            .build()
            .await
            .map_err(|e| OhraError::Storage(format!("open {}: {e}", path.display())))?;
        with_conn(&pool, migrations::run).await?;
        Ok(Self { pool })
    }

    /// Open `$OHARA_HOME/harness.sqlite`.
    pub async fn open_default() -> Result<Self> {
        let home = ohara_core::paths::ohara_home()?;
        Self::open(&home.join("harness.sqlite")).await
    }

    async fn write<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Transaction<'_>) -> anyhow::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        with_conn(&self.pool, move |c: &mut Connection| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })
        .await
    }
}

fn expect_one(changed: usize, what: &str) -> anyhow::Result<()> {
    match changed {
        0 => anyhow::bail!("{what} not found"),
        _ => Ok(()),
    }
}

const UPSERT_TASK_SET: &str = "outcome=?2, workflow=?3, repo_root=?4, base_sha=?5, branch=?6, \
     state=?7, deadline_ms=?8, cost_ceiling=?9, cost_spent=?10, contract_hash=?11, \
     heartbeat_ms=?12, round=?13, created_ms=?14, finished_ms=?15, detail=?16";

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn write_task(tx: &Transaction<'_>, sql: &str, t: &TaskRecord) -> anyhow::Result<usize> {
    Ok(tx.execute(
        sql,
        params![
            t.id.to_string(),
            t.outcome.0,
            t.workflow,
            path_str(&t.repo_root),
            t.base_sha,
            t.branch,
            enum_str(&t.state)?,
            to_ms(t.deadline.0),
            t.cost_ceiling.map(cents).transpose()?,
            cents(t.cost_spent)?,
            t.contract_hash.map(|h| h.to_string()),
            t.heartbeat_at.map(to_ms),
            t.round,
            to_ms(t.created_at),
            t.finished_at.map(to_ms),
            t.detail,
        ],
    )?)
}

#[async_trait]
impl TaskStore for SqliteTaskStore {
    async fn create_task(&self, t: &TaskRecord) -> Result<()> {
        let t = t.clone();
        self.write(move |tx| {
            let sql = format!(
                "INSERT INTO task ({TASK_COLS}) VALUES \
                 (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)"
            );
            write_task(tx, &sql, &t)?;
            Ok(())
        })
        .await
    }

    async fn update_task(&self, t: &TaskRecord) -> Result<()> {
        let t = t.clone();
        self.write(move |tx| {
            let sql = format!("UPDATE task SET {UPSERT_TASK_SET} WHERE id=?1");
            expect_one(write_task(tx, &sql, &t)?, "task")
        })
        .await
    }

    async fn get_task(&self, id: &TaskId) -> Result<Option<TaskRecord>> {
        let id = id.to_string();
        with_conn(&self.pool, move |c| {
            let sql = format!("SELECT {TASK_COLS} FROM task WHERE id=?1");
            let mut stmt = c.prepare(&sql)?;
            let mut rows = stmt.query_map([id], task_from_row)?;
            Ok(rows.next().transpose()?)
        })
        .await
    }

    async fn list_tasks(&self) -> Result<Vec<TaskRecord>> {
        with_conn(&self.pool, |c| {
            let sql = format!("SELECT {TASK_COLS} FROM task ORDER BY created_ms DESC, id DESC");
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([], task_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    async fn heartbeat(&self, id: &TaskId, at: DateTime<Utc>) -> Result<()> {
        let id = id.to_string();
        self.write(move |tx| {
            let n = tx.execute(
                "UPDATE task SET heartbeat_ms=?2 WHERE id=?1",
                params![id, to_ms(at)],
            )?;
            expect_one(n, "task")
        })
        .await
    }

    async fn record_step(&self, s: &StepRecord) -> Result<()> {
        let s = s.clone();
        self.write(move |tx| {
            let sql = format!(
                "INSERT INTO step ({STEP_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14) \
                 ON CONFLICT(task_id, round, seq) DO UPDATE SET role=?4, runtime=?5, model=?6, \
                 state=?7, started_ms=?8, finished_ms=?9, head_sha=?10, cost=?11, \
                 failover_from=?12, transcript=?13, summary=?14"
            );
            tx.execute(
                &sql,
                params![
                    s.task_id.to_string(),
                    s.round,
                    s.seq,
                    enum_str(&s.role)?,
                    s.runtime.0,
                    s.model,
                    enum_str(&s.state)?,
                    to_ms(s.started_at),
                    s.finished_at.map(to_ms),
                    s.head_sha,
                    s.cost.map(cents).transpose()?,
                    s.failover_from.as_ref().map(|r| r.0.clone()),
                    path_str(&s.transcript),
                    s.summary,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn list_steps(&self, id: &TaskId) -> Result<Vec<StepRecord>> {
        let id = id.to_string();
        with_conn(&self.pool, move |c| {
            let sql = format!("SELECT {STEP_COLS} FROM step WHERE task_id=?1 ORDER BY round, seq");
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([id], step_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    async fn put_decision(&self, d: &DecisionRecord) -> Result<()> {
        let d = d.clone();
        self.write(move |tx| {
            let sql = format!(
                "INSERT INTO decision ({DECISION_COLS}) VALUES \
                 (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16) \
                 ON CONFLICT(id) DO UPDATE SET task_id=?2, round=?3, parent_ids=?4, question=?5, \
                 options=?6, chosen=?7, runner_up=?8, assumptions=?9, cost_if_wrong=?10, \
                 checkpoint_sha=?11, evidence=?12, state=?13, rejected_reason=?14, \
                 human_verdict=?15, verdict_note=?16"
            );
            tx.execute(
                &sql,
                params![
                    d.id.0.to_string(),
                    d.task_id.to_string(),
                    d.round,
                    json(&d.parent_ids)?,
                    d.question,
                    json(&d.options)?,
                    d.chosen,
                    d.runner_up,
                    json(&d.assumptions)?,
                    enum_str(&d.cost_if_wrong)?,
                    d.checkpoint_sha,
                    json(&d.evidence)?,
                    enum_str(&d.state)?,
                    d.rejected_reason,
                    d.human_verdict.as_ref().map(enum_str).transpose()?,
                    d.verdict_note,
                ],
            )?;
            Ok(())
        })
        .await
    }

    async fn list_decisions(&self, id: &TaskId) -> Result<Vec<DecisionRecord>> {
        let id = id.to_string();
        with_conn(&self.pool, move |c| {
            let sql = format!("SELECT {DECISION_COLS} FROM decision WHERE task_id=?1 ORDER BY id");
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([id], decision_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }
}
