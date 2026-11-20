pub mod decision;
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_round_trips_through_string() {
        let id = TaskId::new();
        let parsed: TaskId = id.to_string().parse().unwrap();
        assert_eq!(id, parsed);
        assert!("not-a-ulid".parse::<TaskId>().is_err());
    }

    #[test]
    fn task_state_round_trips_every_variant() {
        for s in [
            TaskState::Drafting,
            TaskState::AwaitingApproval,
            TaskState::Running,
            TaskState::Succeeded,
            TaskState::TimedOut,
            TaskState::Aborted,
            TaskState::Stopped,
        ] {
            assert_eq!(s.as_str().parse::<TaskState>().unwrap(), s);
        }
        assert!("bogus".parse::<TaskState>().is_err());
    }

    #[test]
    fn contract_hash_displays_as_64_hex_chars() {
        let h = ContractHash([0xab; 32]);
        let s = h.to_string();
        assert_eq!(s.len(), 64);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(s.starts_with("abab"));
    }
}
