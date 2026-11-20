//! `ohara task list` / `show`: read-only views over the `TaskStore`.

use chrono::{DateTime, Duration, Utc};
use ohara_core::task::{TaskRecord, TaskState};

/// A running task silent for longer than this is shown as `interrupted`.
const STALE_AFTER: Duration = Duration::seconds(60);

/// State as shown to users: a `Running` task with a stale (or missing)
/// heartbeat is `interrupted`.
pub fn display_state(t: &TaskRecord, now: DateTime<Utc>) -> &'static str {
    if t.state != TaskState::Running {
        return t.state.as_str();
    }
    let last = t.heartbeat_at.unwrap_or(t.created_at);
    if now - last > STALE_AFTER {
        return "interrupted";
    }
    t.state.as_str()
}

fn one_line(s: &str, max: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// One line per task: id, state, workflow, deadline, outcome.
pub fn render_list(tasks: &[TaskRecord], now: DateTime<Utc>) -> String {
    if tasks.is_empty() {
        return "no tasks\n".to_string();
    }
    let mut out = String::new();
    for t in tasks {
        out.push_str(&format!(
            "{}  {:<17} {:<9} {}  {}\n",
            t.id,
            display_state(t, now),
            t.workflow,
            t.deadline.0.format("%Y-%m-%d %H:%M"),
            one_line(&t.outcome.0, 60),
        ));
    }
    out
}

/// Multi-line detail for one task.
pub fn render_task(t: &TaskRecord, now: DateTime<Utc>) -> String {
    let mut out = format!(
        "task:      {}\nstate:     {}\noutcome:   {}\nworkflow:  {}\nrepo:      {}\nbranch:    {}\nbase:      {}\ndeadline:  {}\nround:     {}\ncost:      {} cents\n",
        t.id,
        display_state(t, now),
        t.outcome.0,
        t.workflow,
        t.repo_root.display(),
        t.branch,
        t.base_sha,
        t.deadline.0.to_rfc3339(),
        t.round,
        t.cost_spent.0,
    );
    if let Some(h) = &t.contract_hash {
        out.push_str(&format!("contract:  {h}\n"));
    }
    if let Some(hb) = t.heartbeat_at {
        out.push_str(&format!("heartbeat: {}\n", hb.to_rfc3339()));
    }
    if let Some(d) = &t.detail {
        out.push_str(&format!("detail:    {d}\n"));
    }
    out
}
