//! Freezing the contract under a side ref and loading it back.

use ohara_core::task::{ContractHash, TaskId};

use super::Contract;
use crate::error::{Error, Result};
use crate::git::Git;

/// Freeze `c` as `refs/ohara/tasks/<id>/contract`, returning its hash.
pub fn freeze(_git: &Git, _task: &TaskId, _c: &Contract) -> Result<ContractHash> {
    Err(Error::Contract("unimplemented".into()))
}

/// Load the frozen contract for `task` with its hash.
pub fn load_frozen(_git: &Git, _task: &TaskId) -> Result<(Contract, ContractHash)> {
    Err(Error::Contract("unimplemented".into()))
}
