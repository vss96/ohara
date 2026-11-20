//! Build a [`RoleRunner`] from a [`HarnessConfig`] using the real adapters.

use std::collections::BTreeMap;
use std::sync::Arc;

use ohara_core::task::RuntimeName;

use super::agy::AgyRuntime;
use super::claude::ClaudeCodeRuntime;
use super::codex::CodexRuntime;
use super::generic::GenericRuntime;
use super::roles::RoleRunner;
use super::AgentRuntime;
use crate::config::{HarnessConfig, RuntimeKind, RuntimeSpec};
use crate::error::Result;

fn build_runtime(name: &RuntimeName, spec: &RuntimeSpec) -> Result<Arc<dyn AgentRuntime>> {
    let model = spec.model.clone();
    match spec.kind {
        RuntimeKind::ClaudeCode => Ok(Arc::new(ClaudeCodeRuntime::new(name.clone(), model))),
        RuntimeKind::Codex => Ok(Arc::new(CodexRuntime::new(name.clone(), model))),
        RuntimeKind::Agy => Ok(Arc::new(AgyRuntime::new(name.clone(), model))),
        RuntimeKind::Generic => Ok(Arc::new(GenericRuntime::new(name.clone(), spec)?)),
        #[cfg(any(test, feature = "test-util"))]
        RuntimeKind::Fake => Err(crate::error::Error::Config(format!(
            "runtime {}: the fake kind cannot be built from config",
            name.0
        ))),
    }
}

/// Build a [`RoleRunner`] over the real runtimes named in `config`. Every
/// configured runtime is constructed up front so a bad spec fails here, not
/// midway through a task.
pub fn runner_from_config(config: &HarnessConfig) -> Result<RoleRunner> {
    let mut built: BTreeMap<RuntimeName, Arc<dyn AgentRuntime>> = BTreeMap::new();
    for (name, spec) in &config.runtimes {
        built.insert(name.clone(), build_runtime(name, spec)?);
    }
    RoleRunner::new(config, |name, _| {
        built
            .get(name)
            .cloned()
            .expect("invariant: every configured runtime was built above")
    })
}
