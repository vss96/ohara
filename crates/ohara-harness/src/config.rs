//! Harness configuration: agent runtimes and per-role runtime lists.

use std::collections::BTreeMap;
use std::path::Path;

use ohara_core::task::{Role, RuntimeName};
use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum RuntimeKind {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "codex")]
    Codex,
    #[serde(rename = "agy")]
    Agy,
    #[serde(rename = "generic")]
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptMode {
    Arg,
    Stdin,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DoneRule {
    pub exit_code: i32,
    #[serde(default)]
    pub output_contains: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RuntimeSpec {
    pub kind: RuntimeKind,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default)]
    pub prompt: Option<PromptMode>,
    #[serde(default)]
    pub done: Option<DoneRule>,
}

/// Ordered runtime list per role: first is primary, the rest are failover.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct RoleMap(pub BTreeMap<Role, Vec<RuntimeName>>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessConfig {
    pub runtimes: BTreeMap<RuntimeName, RuntimeSpec>,
    pub roles: RoleMap,
}

impl HarnessConfig {
    pub fn load(_path: &Path) -> Result<Self> {
        Err(Error::Config("unimplemented".into()))
    }

    pub fn load_default() -> Result<Self> {
        Err(Error::Config("unimplemented".into()))
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
        let err = load_str("[runtimes.claude]\nkind = \"claude-code\"\n[roles]\nbuilder = [\"ghost\"]\n")
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
}
