//! Newtypes and the persistence contract for worktree-aware indexing.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_sha_accepts_40_hex_and_rejects_everything_else() {
        assert!(CommitSha::parse("0123456789abcdef0123456789abcdef01234567").is_ok());
        assert!(CommitSha::parse("abc").is_err());
        assert!(CommitSha::parse("0123456789ABCDEF0123456789abcdef01234567").is_err());
        assert!(CommitSha::parse("z123456789abcdef0123456789abcdef01234567").is_err());
    }

    #[test]
    fn worktree_key_main_is_empty_and_linked_rejects_empty() {
        assert_eq!(WorktreeKey::main().as_str(), "");
        assert!(WorktreeKey::main().is_main());
        assert!(WorktreeKey::linked("").is_err());
        let k = WorktreeKey::linked("slot-1").unwrap();
        assert_eq!(k.as_str(), "slot-1");
        assert!(!k.is_main());
    }
}
