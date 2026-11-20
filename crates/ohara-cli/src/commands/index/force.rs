//! `ohara index --force` and the shared HEAD-symbol snapshot (plan-33).

use ohara_core::{IndexedTip, WorktreeKey};

/// Printed by `--force` in a worktree whose run leaves the shared
/// HEAD-symbol snapshot alone.
pub(super) const NOT_OWNER_NOTE: &str =
    "HEAD symbols follow the main worktree; not clearing them here";

/// The note `--force` prints for `worktree`, given the store's tips:
/// `None` when this worktree's run writes the snapshot itself.
pub(super) fn force_note(worktree: &WorktreeKey, _tips: &[IndexedTip]) -> Option<&'static str> {
    match worktree.is_main() {
        true => None,
        false => Some(NOT_OWNER_NOTE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ohara_core::CommitSha;

    fn tip(worktree: WorktreeKey) -> IndexedTip {
        IndexedTip {
            worktree,
            sha: CommitSha::parse(&"a".repeat(40)).unwrap(),
        }
    }

    #[test]
    fn linked_worktree_without_a_main_tip_owns_the_snapshot() {
        let slot = WorktreeKey::linked("slot-1").unwrap();
        assert_eq!(force_note(&slot, &[]), None);
        assert_eq!(force_note(&slot, &[tip(slot.clone())]), None);
    }

    #[test]
    fn linked_worktree_with_a_main_tip_gets_the_note() {
        let slot = WorktreeKey::linked("slot-1").unwrap();
        let tips = [tip(WorktreeKey::main()), tip(slot.clone())];
        assert_eq!(force_note(&slot, &tips), Some(NOT_OWNER_NOTE));
    }

    #[test]
    fn main_never_gets_the_note() {
        assert_eq!(force_note(&WorktreeKey::main(), &[]), None);
        assert_eq!(
            force_note(&WorktreeKey::main(), &[tip(WorktreeKey::main())]),
            None
        );
    }
}
