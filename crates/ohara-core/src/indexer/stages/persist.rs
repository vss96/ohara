//! Persist stage: consumes `Vec<EmbeddedHunk>` + commit embedding and
//! writes a single storage transaction.
//!
//! The stage itself carries no state — it is a pure function of its
//! inputs. The coordinator constructs it and calls `run` per commit.

// Implementation lives in Phase B Task B.5.
