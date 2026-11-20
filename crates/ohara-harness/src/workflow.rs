//! Workflows: personal task templates in `$OHARA_HOME/workflows/<name>.toml`.
//!
//! A workflow carries the default contract pieces (scripts, rubric, protected
//! paths) and budgets for one kind of task. Three starters (`bug`, `feature`,
//! `research`) are seeded on first use and never overwrite a user's file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::config::RoleMap;
use crate::contract::{Rubric, Script};
use crate::error::{Error, Result};

const STARTERS: [(&str, &str); 3] = [
    ("bug", include_str!("workflow/starters/bug.toml")),
    ("feature", include_str!("workflow/starters/feature.toml")),
    ("research", include_str!("workflow/starters/research.toml")),
];

/// The default contract pieces a workflow contributes to a task.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct ContractTemplate {
    /// Paths agents may never change.
    #[serde(default)]
    pub protected_paths: Vec<String>,
    /// Scripts that must pass.
    #[serde(default, rename = "script")]
    pub scripts: Vec<Script>,
    /// Rubric criteria.
    #[serde(default, rename = "rubric")]
    pub rubric: Vec<Rubric>,
}

/// On-disk shape of a workflow file; the name comes from the file stem.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWorkflow {
    description: String,
    default_budget_minutes: u64,
    #[serde(default)]
    diff_budget_lines: u32,
    #[serde(default)]
    roles: Option<RoleMap>,
    #[serde(default)]
    contract: ContractTemplate,
}

/// A named task template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    /// Workflow name (the file stem).
    pub name: String,
    /// One-line description.
    pub description: String,
    /// Default scripts, rubric and protected paths.
    pub contract_template: ContractTemplate,
    /// Per-workflow role to runtime override, if any.
    pub roles: Option<RoleMap>,
    /// Default time budget for a task.
    pub default_budget: Duration,
    /// Maximum diff size per increment in lines; 0 means no limit.
    pub diff_budget_lines: u32,
}

/// A workflow name is a file stem: ASCII letters, digits, `-` and `_` only,
/// so it can never escape the workflows directory.
fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        return Ok(());
    }
    Err(Error::Config(format!(
        "workflow name {name:?}: use letters, digits, '-' and '_' only"
    )))
}

/// Write any missing starter into `dir`. Existing files are never touched.
pub(crate) fn seed_starters(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, body) in STARTERS {
        let path = dir.join(format!("{name}.toml"));
        let created = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path);
        match created {
            Ok(mut f) => f.write_all(body.as_bytes())?,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Ok(())
}

impl Workflow {
    /// Parse one workflow file's text. `name` is the file stem.
    pub fn parse(name: &str, text: &str) -> Result<Self> {
        return Err(Error::Config("unimplemented".into()));
        #[allow(unreachable_code)]
        validate_name(name)?;
        let raw: RawWorkflow =
            toml::from_str(text).map_err(|e| Error::Config(format!("workflow {name}: {e}")))?;
        if raw.default_budget_minutes == 0 {
            return Err(Error::Config(format!(
                "workflow {name}: default_budget_minutes must be positive"
            )));
        }
        Ok(Self {
            name: name.to_string(),
            description: raw.description,
            contract_template: raw.contract,
            roles: raw.roles,
            default_budget: Duration::from_secs(raw.default_budget_minutes.saturating_mul(60)),
            diff_budget_lines: raw.diff_budget_lines,
        })
    }

    /// Load `name` from `$OHARA_HOME/workflows`, seeding the starters first.
    pub fn load(name: &str) -> Result<Self> {
        Self::load_from(&workflows_dir()?, name)
    }

    /// Load every workflow in `$OHARA_HOME/workflows`, sorted by name.
    pub fn list() -> Result<Vec<Self>> {
        Self::list_from(&workflows_dir()?)
    }

    /// [`Workflow::load`] against an explicit workflows directory.
    pub fn load_from(dir: &Path, name: &str) -> Result<Self> {
        validate_name(name)?;
        seed_starters(dir)?;
        let path = dir.join(format!("{name}.toml"));
        let text = std::fs::read_to_string(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Error::Config(format!("no workflow named {name:?} in {}", dir.display()));
            }
            Error::Io(e)
        })?;
        Self::parse(name, &text)
    }

    /// [`Workflow::list`] against an explicit workflows directory.
    pub fn list_from(dir: &Path) -> Result<Vec<Self>> {
        seed_starters(dir)?;
        let mut names = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let is_toml = path.extension().is_some_and(|e| e == "toml");
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
            if let (true, Some(stem)) = (is_toml, stem) {
                names.push(stem);
            }
        }
        names.sort();
        names.iter().map(|n| Self::load_from(dir, n)).collect()
    }
}

fn workflows_dir() -> Result<PathBuf> {
    Ok(ohara_core::paths::ohara_home()?.join("workflows"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starters_seed_into_empty_home() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("workflows");
        let all = Workflow::list_from(&dir).unwrap();
        let names: Vec<&str> = all.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["bug", "feature", "research"]);

        let bug = Workflow::load_from(&dir, "bug").unwrap();
        assert_eq!(bug.diff_budget_lines, 200);
        assert_eq!(bug.default_budget, Duration::from_secs(60 * 60));
        assert_eq!(bug.contract_template.scripts.len(), 1);
        assert_eq!(bug.contract_template.rubric.len(), 1);

        let feature = Workflow::load_from(&dir, "feature").unwrap();
        assert_eq!(feature.diff_budget_lines, 400);
        assert_eq!(feature.default_budget, Duration::from_secs(120 * 60));
        assert_eq!(feature.contract_template.scripts.len(), 1);
        assert_eq!(feature.contract_template.rubric.len(), 2);

        let research = Workflow::load_from(&dir, "research").unwrap();
        assert_eq!(research.default_budget, Duration::from_secs(60 * 60));
        assert!(research.contract_template.scripts.is_empty());
        assert_eq!(research.contract_template.rubric.len(), 2);
    }

    #[test]
    fn user_edit_to_starter_is_not_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        let mine = "description = \"mine\"\ndefault_budget_minutes = 5\n";
        std::fs::write(dir.join("bug.toml"), mine).unwrap();

        let bug = Workflow::load_from(&dir, "bug").unwrap();
        assert_eq!(bug.description, "mine");
        assert_eq!(std::fs::read_to_string(dir.join("bug.toml")).unwrap(), mine);
        assert!(dir.join("feature.toml").exists());
    }

    #[test]
    fn unknown_or_unsafe_names_are_clear_errors() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("workflows");
        let err = Workflow::load_from(&dir, "nope").unwrap_err();
        assert!(err.to_string().contains("no workflow named"), "{err}");
        for bad in ["", "../x", "a/b", "a.b"] {
            assert!(Workflow::load_from(&dir, bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn malformed_workflow_is_an_error_not_a_panic() {
        let err = Workflow::parse("x", "this is not toml [").unwrap_err();
        assert!(err.to_string().contains("workflow x"), "{err}");
        let zero = Workflow::parse("x", "description = \"d\"\ndefault_budget_minutes = 0\n");
        assert!(zero.is_err());
    }
}
