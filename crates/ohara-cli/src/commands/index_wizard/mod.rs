//! Interactive `ohara index -i` wizard.
//!
//! A pure front-end over `commands::index::run`: it collects choices
//! through the [`WizardPrompter`] trait and assembles an
//! [`crate::commands::index::Args`]. The answer→Args mapping, provider
//! availability, and command rendering live in `assemble` and are
//! TTY-free / unit-tested.

mod assemble;
pub use assemble::*;
