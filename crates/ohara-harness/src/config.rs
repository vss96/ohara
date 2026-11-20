//! Harness configuration: agent runtimes and per-role runtime lists.

use std::collections::BTreeMap;
use std::path::Path;

use ohara_core::task::{Role, RuntimeName};
use serde::Deserialize;

use crate::error::{Error, Result};

/// Which adapter drives a runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum RuntimeKind {
    /// The Claude Code CLI.
    #[serde(rename = "claude-code")]
    ClaudeCode,
    /// The Codex CLI.
    #[serde(rename = "codex")]
    Codex,
    /// The Antigravity CLI.
    #[serde(rename = "agy")]
    Agy,
    /// A user-defined command described by `command`, `prompt` and `done`.
    #[serde(rename = "generic")]
    Generic,
    /// Scripted test runtime; absent from release builds.
    #[cfg(any(test, feature = "test-util"))]
    #[serde(rename = "fake")]
    Fake,
}

/// How a generic runtime receives its prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMode {
    /// Substituted into the `{prompt}` argv placeholder.
    Arg,
    /// Written to the child process stdin.
    Stdin,
}

/// When a generic runtime run counts as completed.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DoneRule {
    /// Exit code that signals success.
    pub exit_code: i32,
    /// Optional text the output must also contain.
    #[serde(default)]
    pub output_contains: Option<String>,
}

/// Configuration for one named runtime.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RuntimeSpec {
    /// Adapter used to run this runtime.
    pub kind: RuntimeKind,
    /// Model override passed to the agent, if any.
    #[serde(default)]
    pub model: Option<String>,
    /// Argv for a generic runtime.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// Prompt delivery mode for a generic runtime.
    #[serde(default)]
    pub prompt: Option<PromptMode>,
    /// Completion rule for a generic runtime.
    #[serde(default)]
    pub done: Option<DoneRule>,
}

/// Ordered runtime list per role: first is primary, the rest are failover.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct RoleMap(pub BTreeMap<Role, Vec<RuntimeName>>);

/// Validated harness configuration: runtimes and role assignments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessConfig {
    /// Runtimes by name.
    pub runtimes: BTreeMap<RuntimeName, RuntimeSpec>,
    /// Runtime order per role.
    pub roles: RoleMap,
}

/// On-disk shape. Unknown top-level tables (e.g. a future `[poc]`) are
/// ignored: serde skips unrecognised fields by default.
#[derive(Debug, Deserialize)]
struct RawConfig {
    #[serde(default)]
    runtimes: BTreeMap<RuntimeName, RuntimeSpec>,
    #[serde(default)]
    roles: RoleMap,
}

const ALL_ROLES: [Role; 5] = [
    Role::Contract,
    Role::Planner,
    Role::Builder,
    Role::Judge,
    Role::Reviewer,
];

fn default_runtime_name() -> RuntimeName {
    RuntimeName("claude".to_string())
}

impl HarnessConfig {
    /// Built-in config: one `claude` runtime serving every role.
    pub fn builtin_default() -> Self {
        let claude = default_runtime_name();
        let spec = RuntimeSpec {
            kind: RuntimeKind::ClaudeCode,
            model: None,
            command: None,
            prompt: None,
            done: None,
        };
        let roles = ALL_ROLES
            .iter()
            .map(|r| (*r, vec![claude.clone()]))
            .collect();
        Self {
            runtimes: BTreeMap::from([(claude, spec)]),
            roles: RoleMap(roles),
        }
    }

    /// Load and validate a config file. A missing file yields the built-in
    /// default.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::builtin_default())
            }
            Err(e) => return Err(Error::Io(e)),
        };
        let raw: RawConfig =
            toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        Self::from_raw(raw)
    }

    /// Load `$OHARA_HOME/config.toml`.
    pub fn load_default() -> Result<Self> {
        let home = ohara_core::paths::ohara_home()?;
        Self::load(&home.join("config.toml"))
    }

    fn from_raw(raw: RawConfig) -> Result<Self> {
        let mut runtimes = raw.runtimes;
        if runtimes.is_empty() {
            runtimes = Self::builtin_default().runtimes;
        }
        let mut roles = raw.roles.0;
        let claude = default_runtime_name();
        for role in ALL_ROLES {
            if !roles.contains_key(&role) && runtimes.contains_key(&claude) {
                roles.insert(role, vec![claude.clone()]);
            }
        }
        let cfg = Self {
            runtimes,
            roles: RoleMap(roles),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        for (name, spec) in &self.runtimes {
            let missing_command = spec.kind == RuntimeKind::Generic
                && spec.command.as_ref().is_none_or(|c| c.is_empty());
            if missing_command {
                return Err(Error::Config(format!(
                    "runtimes.{}: kind \"generic\" requires `command`",
                    name.0
                )));
            }
        }
        for role in ALL_ROLES {
            if !self.roles.0.contains_key(&role) {
                return Err(Error::Config(format!(
                    "roles.{}: no runtime configured",
                    role.as_str()
                )));
            }
        }
        for (role, list) in &self.roles.0 {
            let key = role.as_str();
            if list.is_empty() {
                return Err(Error::Config(format!("roles.{key}: runtime list is empty")));
            }
            if let Some(unknown) = list.iter().find(|n| !self.runtimes.contains_key(*n)) {
                return Err(Error::Config(format!(
                    "roles.{key}: unknown runtime \"{}\"",
                    unknown.0
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Spec §8a block; the one-line `[runtimes.x]  kind = ...` headers are
    // split onto two lines because TOML forbids a key after a header.
    const SPEC_EXAMPLE: &str = r#"
[runtimes.claude]
kind = "claude-code"
[runtimes.codex]
kind = "codex"
[runtimes.agy]
kind = "agy"        # model = "..." optional
[runtimes.local]
kind = "generic"
command = ["my-agent", "--yes", "{prompt}"]
prompt = "arg"
done = { exit_code = 0 }

[roles]
planner  = ["claude", "codex"]     # first is primary; the rest are the failover order
builder  = ["codex", "claude"]
judge    = ["agy", "claude"]       # prefer a different model family from the builder
reviewer = ["claude"]
contract = ["claude"]

[poc]
builders = ["codex", "claude"]     # optional: POCs of one decision rotate across these
"#;

    fn name(s: &str) -> RuntimeName {
        RuntimeName(s.to_string())
    }

    fn load_str(toml: &str) -> Result<HarnessConfig> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, toml).unwrap();
        HarnessConfig::load(&p)
    }

    #[test]
    fn parses_spec_example_config() {
        let cfg = load_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(cfg.runtimes.len(), 4);
        assert_eq!(cfg.runtimes[&name("codex")].kind, RuntimeKind::Codex);
        let local = &cfg.runtimes[&name("local")];
        assert_eq!(local.kind, RuntimeKind::Generic);
        assert_eq!(local.prompt, Some(PromptMode::Arg));
        assert_eq!(
            local.done,
            Some(DoneRule {
                exit_code: 0,
                output_contains: None
            })
        );
        assert_eq!(
            cfg.roles.0[&Role::Planner],
            vec![name("claude"), name("codex")]
        );
        assert_eq!(cfg.roles.0[&Role::Judge], vec![name("agy"), name("claude")]);
    }

    #[test]
    fn missing_file_yields_claude_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = HarnessConfig::load(&dir.path().join("nope.toml")).unwrap();
        assert_eq!(cfg.runtimes.len(), 1);
        assert_eq!(cfg.runtimes[&name("claude")].kind, RuntimeKind::ClaudeCode);
        for role in [
            Role::Contract,
            Role::Planner,
            Role::Builder,
            Role::Judge,
            Role::Reviewer,
        ] {
            assert_eq!(cfg.roles.0[&role], vec![name("claude")]);
        }
    }

    #[test]
    fn role_referencing_unknown_runtime_is_error_naming_it() {
        let err =
            load_str("[runtimes.claude]\nkind = \"claude-code\"\n[roles]\nbuilder = [\"ghost\"]\n")
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ghost"), "{msg}");
        assert!(msg.contains("builder"), "{msg}");
    }

    #[test]
    fn generic_without_command_is_error() {
        let err = load_str("[runtimes.local]\nkind = \"generic\"\n").unwrap_err();
        assert!(err.to_string().contains("runtimes.local"), "{err}");
    }

    #[test]
    fn empty_role_list_is_error_naming_role() {
        let err = load_str("[runtimes.claude]\nkind = \"claude-code\"\n[roles]\njudge = []\n")
            .unwrap_err();
        assert!(err.to_string().contains("judge"), "{err}");
    }

    #[test]
    fn roles_without_claude_runtime_must_be_listed_explicitly() {
        let err = load_str("[runtimes.codex]\nkind = \"codex\"\n").unwrap_err();
        assert!(err.to_string().contains("roles.contract"), "{err}");
        let err = load_str("[runtimes.codex]\nkind = \"codex\"\n[roles]\nbuilder = [\"codex\"]\n")
            .unwrap_err();
        assert!(err.to_string().contains("no runtime configured"), "{err}");
    }
}
