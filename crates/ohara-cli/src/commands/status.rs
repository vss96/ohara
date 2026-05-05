use anyhow::Result;
use clap::Args as ClapArgs;
use ohara_core::index_metadata::{CompatibilityStatus, RuntimeIndexMetadata};
use ohara_core::query::compute_index_status;
use ohara_core::Storage;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[arg(default_value = ".")]
    pub path: PathBuf,
}

/// Build the runtime compatibility expectation from the constants
/// owned by `ohara-embed` / `ohara-parse`. Does NOT load the embedder
/// — status must stay cheap. Delegates to the canonical
/// `ohara_core::index_metadata::runtime_metadata_from`.
pub fn current_runtime_metadata() -> RuntimeIndexMetadata {
    ohara_core::index_metadata::runtime_metadata_from(
        ohara_embed::DEFAULT_MODEL_ID,
        ohara_embed::DEFAULT_DIM as u32,
        ohara_embed::DEFAULT_RERANKER_ID,
        ohara_parse::CHUNKER_VERSION,
        ohara_parse::parser_versions(),
    )
}

/// Render `CompatibilityStatus` as a single line for `ohara status`,
/// followed by an actionable next-step command when one applies.
/// Pulled out so unit tests can pin the wording without standing up a
/// real index dir.
pub fn render_compatibility(status: &CompatibilityStatus) -> String {
    match status {
        CompatibilityStatus::Compatible => "compatibility: compatible".to_string(),
        CompatibilityStatus::QueryCompatibleNeedsRefresh { reason } => format!(
            "compatibility: query-compatible, refresh recommended ({reason})\n  run: ohara index --force"
        ),
        CompatibilityStatus::NeedsRebuild { reason } => format!(
            "compatibility: needs rebuild ({reason})\n  run: ohara index --rebuild"
        ),
        CompatibilityStatus::Unknown { missing_components } => format!(
            "compatibility: unknown (no metadata for {})\n  run: ohara index --force",
            missing_components.join(", ")
        ),
    }
}

/// Render the one-line summary printed by `ohara status` when the
/// repo has any ignore rules active. Pulled out for unit testing.
pub fn render_ignore_summary(builtins: usize, gitattrs: usize, user: usize) -> String {
    let total = builtins + gitattrs + user;
    format!(
        "ignore_rules: {total} patterns ({builtins} built-in + {gitattrs} gitattributes + {user} user)"
    )
}

fn count_ignore_layers(repo_root: &std::path::Path) -> (usize, usize, usize) {
    let builtins = ohara_core::BUILT_IN_DEFAULTS.len();
    let gitattrs = std::fs::read_to_string(repo_root.join(".gitattributes"))
        .map(|s| s.lines().filter(|l| l.contains("linguist-")).count())
        .unwrap_or(0);
    let user = std::fs::read_to_string(repo_root.join(".oharaignore"))
        .map(|s| {
            s.lines()
                .filter(|l| {
                    let t = l.trim();
                    !t.is_empty() && !t.starts_with('#')
                })
                .count()
        })
        .unwrap_or(0);
    (builtins, gitattrs, user)
}

pub async fn run(args: Args) -> Result<()> {
    let (repo_id, canonical, _) = super::resolve_repo_id(&args.path)?;
    let db_path = super::index_db_path(&repo_id)?;
    let storage = Arc::new(ohara_storage::SqliteStorage::open(&db_path).await?);
    let behind = ohara_git::GitCommitsBehind::open(&canonical)?;
    let st = compute_index_status(storage.as_ref(), &repo_id, &behind).await?;

    let runtime = current_runtime_metadata();
    let stored = storage.get_index_metadata(&repo_id).await?;
    let compatibility = CompatibilityStatus::assess(&runtime, &stored);

    println!(
        "repo: {}\nid: {}\nlast_indexed_commit: {}\nindexed_at: {}\ncommits_behind_head: {}\n{}",
        canonical.display(),
        repo_id.as_str(),
        st.last_indexed_commit.unwrap_or_else(|| "<none>".into()),
        st.indexed_at.unwrap_or_else(|| "<none>".into()),
        st.commits_behind_head,
        render_compatibility(&compatibility),
    );
    let (b, g, u) = count_ignore_layers(&canonical);
    println!("{}", render_ignore_summary(b, g, u));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ohara_core::index_metadata::StoredIndexMetadata;
    use std::collections::BTreeMap;

    fn stored_complete_for(runtime: &RuntimeIndexMetadata) -> StoredIndexMetadata {
        let mut components = BTreeMap::new();
        for (k, v) in runtime.to_storage_components() {
            components.insert(k, v);
        }
        StoredIndexMetadata { components }
    }

    #[test]
    fn render_compatibility_compatible_is_one_line_no_action() {
        let s = render_compatibility(&CompatibilityStatus::Compatible);
        assert_eq!(s, "compatibility: compatible");
    }

    #[test]
    fn render_compatibility_unknown_lists_missing_and_recommends_force() {
        let s = render_compatibility(&CompatibilityStatus::Unknown {
            missing_components: vec!["embedding_model".into(), "chunker_version".into()],
        });
        assert!(
            s.contains("compatibility: unknown")
                && s.contains("embedding_model")
                && s.contains("chunker_version")
                && s.contains("ohara index --force"),
            "render output: {s}"
        );
    }

    #[test]
    fn render_compatibility_needs_rebuild_recommends_rebuild_and_names_reason() {
        let s = render_compatibility(&CompatibilityStatus::NeedsRebuild {
            reason: "embedding_dimension mismatch".into(),
        });
        assert!(
            s.contains("compatibility: needs rebuild")
                && s.contains("embedding_dimension")
                && s.contains("ohara index --rebuild"),
            "render output: {s}"
        );
    }

    #[test]
    fn render_compatibility_refresh_recommends_force() {
        let s = render_compatibility(&CompatibilityStatus::QueryCompatibleNeedsRefresh {
            reason: "chunker_version mismatch".into(),
        });
        assert!(
            s.contains("query-compatible")
                && s.contains("chunker_version")
                && s.contains("ohara index --force"),
            "render output: {s}"
        );
    }

    #[test]
    fn current_runtime_metadata_matches_constants_no_embedder_load() {
        // Plan 13 Task 3.1 Step 3: status MUST be able to compute its
        // expectation without loading the embedder model. This test
        // pins that the helper sources every value from a constant
        // (no I/O, no model download).
        let m = current_runtime_metadata();
        assert_eq!(m.schema_version, ohara_core::index_metadata::SCHEMA_VERSION);
        assert_eq!(m.embedding_model, ohara_embed::DEFAULT_MODEL_ID);
        assert_eq!(m.embedding_dimension, ohara_embed::DEFAULT_DIM as u32);
        assert_eq!(m.reranker_model, ohara_embed::DEFAULT_RERANKER_ID);
        assert_eq!(m.chunker_version, ohara_parse::CHUNKER_VERSION);
        assert_eq!(
            m.semantic_text_version,
            ohara_core::index_metadata::SEMANTIC_TEXT_VERSION
        );
        // Every language ohara-parse can index must appear in the map.
        for lang in ["rust", "python", "java", "kotlin"] {
            assert!(
                m.parser_versions.contains_key(lang),
                "parser_versions missing language {lang}"
            );
        }
    }

    #[test]
    fn assess_against_complete_stored_metadata_is_compatible() {
        let runtime = current_runtime_metadata();
        let stored = stored_complete_for(&runtime);
        assert_eq!(
            CompatibilityStatus::assess(&runtime, &stored),
            CompatibilityStatus::Compatible
        );
    }

    #[test]
    fn assess_with_dimension_mismatch_is_needs_rebuild() {
        let runtime = current_runtime_metadata();
        let mut stored = stored_complete_for(&runtime);
        stored
            .components
            .insert("embedding_dimension".into(), "768".into());
        let assessment = CompatibilityStatus::assess(&runtime, &stored);
        assert!(matches!(
            assessment,
            CompatibilityStatus::NeedsRebuild { .. }
        ));
    }

    #[test]
    fn assess_with_empty_stored_is_unknown() {
        let runtime = current_runtime_metadata();
        let stored = StoredIndexMetadata::default();
        assert!(matches!(
            CompatibilityStatus::assess(&runtime, &stored),
            CompatibilityStatus::Unknown { .. }
        ));
    }

    #[test]
    fn render_ignore_summary_counts_by_layer() {
        // Plan 26 Task E.1: a one-line summary of the active filter.
        let s = render_ignore_summary(
            /* builtins */ 18, /* gitattrs */ 0, /* user */ 5,
        );
        assert_eq!(
            s,
            "ignore_rules: 23 patterns (18 built-in + 0 gitattributes + 5 user)"
        );
    }

    #[test]
    fn render_ignore_summary_zero_user_no_gitattrs_still_prints() {
        let s = render_ignore_summary(18, 0, 0);
        assert!(s.contains("18 patterns"), "got: {s}");
    }
}
