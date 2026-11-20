//! `ohara task`: run an outcome-driven harness task (plan-34, phase 1).
//!
//! `ohara task "<outcome>" --for 2h` drafts a contract in the task's own
//! worktree and asks for approval; `approve`, `list` and `show` manage tasks
//! afterwards. Supervision (`stop`, `resume`, `review`) lands in later tasks.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Args as ClapArgs, Subcommand};
use ohara_core::task::{TaskId, TaskStore};
use ohara_harness::agent::build::runner_from_config;
use ohara_harness::config::HarnessConfig;
use ohara_storage::harness::SqliteTaskStore;

/// `ohara task approve`.
pub mod approve;
/// Budget parsing (`--for`, `--until`).
pub mod clock;
/// `ohara task "<outcome>"`.
pub mod create;
/// `ohara task list` / `show`.
pub mod view;

use clock::{parse_duration, parse_until, Budget};
use create::CreateOpts;

/// Printed once per task, at the contract prompt.
const SANDBOX_WARNING: &str = "Phase 1: agents run with your permissions; no sandbox.";

/// Arguments for `ohara task`.
#[derive(ClapArgs, Debug)]
#[command(args_conflicts_with_subcommands = true)]
pub struct Args {
    /// What the task should achieve.
    pub outcome: Option<String>,
    /// Time budget counted from approval, e.g. `2h`, `90m`, `1h30m`.
    #[arg(long = "for", value_name = "DURATION", conflicts_with = "until")]
    pub for_duration: Option<String>,
    /// Absolute deadline: an RFC 3339 timestamp or `HH:MM` today.
    #[arg(long, value_name = "TIME")]
    pub until: Option<String>,
    /// Workflow to follow.
    #[arg(long, default_value = "feature")]
    pub workflow: String,
    /// Repository to work on.
    #[arg(long, default_value = ".")]
    pub repo: PathBuf,
    /// Draft the contract and exit; approve later with `ohara task approve`.
    #[arg(long)]
    pub detach: bool,
    /// Manage existing tasks.
    #[command(subcommand)]
    pub command: Option<TaskCmd>,
}

/// `ohara task <subcommand>`.
#[derive(Subcommand, Debug)]
pub enum TaskCmd {
    /// Approve a drafted contract and start the task.
    Approve {
        /// Task id.
        id: String,
        /// Edit the contract in `$EDITOR` before approving.
        #[arg(long)]
        edit: bool,
    },
    /// List tasks.
    List {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show one task.
    Show {
        /// Task id.
        id: String,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

/// Runs `f` (blocking git/process work) off the async runtime.
pub(crate) async fn blocking<T, E>(f: impl FnOnce() -> Result<T, E> + Send + 'static) -> Result<T>
where
    T: Send + 'static,
    E: Into<anyhow::Error> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .context("background task panicked")?
        .map_err(Into::into)
}

/// `$OHARA_HOME/tasks/<id>`.
pub(crate) fn task_dir(id: &TaskId) -> Result<PathBuf> {
    Ok(ohara_core::paths::ohara_home()?
        .join("tasks")
        .join(id.to_string()))
}

/// The task's git worktree.
pub(crate) fn worktree_path(id: &TaskId) -> Result<PathBuf> {
    Ok(task_dir(id)?.join("worktree"))
}

/// The drafted (not yet frozen) contract.
pub(crate) fn pending_path(id: &TaskId) -> Result<PathBuf> {
    Ok(task_dir(id)?.join("contract.pending.toml"))
}

fn resolve_budget(args: &Args) -> Result<Budget> {
    let now = Utc::now();
    match (&args.for_duration, &args.until) {
        (Some(d), None) => Ok(Budget::For(parse_duration(d)?)),
        (None, Some(u)) => parse_until(u, now),
        _ => bail!("give a time budget: --for <duration> or --until <time>"),
    }
}

fn prompt(question: &str) -> Result<String> {
    print!("{question} ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_lowercase())
}

async fn run_create(store: &SqliteTaskStore, args: Args) -> Result<()> {
    let Some(outcome) = args.outcome.clone() else {
        bail!("give an outcome: ohara task \"<outcome>\" --for 2h");
    };
    let budget = resolve_budget(&args)?;
    let runner = runner_from_config(&HarnessConfig::load_default()?)?;
    let opts = CreateOpts {
        outcome,
        budget,
        workflow: args.workflow,
        repo: args.repo,
    };
    let task = create::create(store, &runner, opts, Utc::now()).await?;
    println!("task {} drafted", task.id);
    println!("{}", std::fs::read_to_string(pending_path(&task.id)?)?);
    println!("{SANDBOX_WARNING}");
    if let (_, Some(note)) = clock::split_detail(task.detail.as_deref()) {
        println!("note: {note}");
    }
    if args.detach {
        println!("ohara task approve {}", task.id);
        return Ok(());
    }
    match prompt("Approve? [y/e(dit)/n]")?.as_str() {
        "y" => approve_and_report(store, &task.id, false).await,
        "e" => approve_and_report(store, &task.id, true).await,
        _ => {
            println!(
                "not approved; run `ohara task approve {}` when ready",
                task.id
            );
            Ok(())
        }
    }
}

async fn approve_and_report(store: &SqliteTaskStore, id: &TaskId, edit: bool) -> Result<()> {
    let mut editor = approve::run_editor;
    let ed: Option<&mut approve::Editor<'_>> = if edit { Some(&mut editor) } else { None };
    let t = approve::approve(store, id, Utc::now(), ed).await?;
    println!(
        "task {} approved; running until {}",
        t.id,
        t.deadline.0.to_rfc3339()
    );
    Ok(())
}

async fn get(store: &SqliteTaskStore, id: &str) -> Result<ohara_core::task::TaskRecord> {
    let id: TaskId = id.parse()?;
    store
        .get_task(&id)
        .await?
        .with_context(|| format!("no task {id}"))
}

/// Entry point for `ohara task`.
pub async fn run(args: Args) -> Result<()> {
    let store = SqliteTaskStore::open_default().await?;
    let now = Utc::now();
    match args.command {
        None => run_create(&store, args).await,
        Some(TaskCmd::Approve { id, edit }) => {
            let id: TaskId = id.parse()?;
            approve_and_report(&store, &id, edit).await
        }
        Some(TaskCmd::List { json }) => {
            let tasks = store.list_tasks().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
                return Ok(());
            }
            print!("{}", view::render_list(&tasks, now));
            Ok(())
        }
        Some(TaskCmd::Show { id, json }) => {
            let t = get(&store, &id).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&t)?);
                return Ok(());
            }
            print!("{}", view::render_task(&t, now));
            Ok(())
        }
    }
}
