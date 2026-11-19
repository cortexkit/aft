#![cfg_attr(test, allow(hidden_glob_reexports))]
pub use aft_db::access_policy::*;

#[cfg(test)]
pub(crate) use aft_base::policy_path::path_probe;
#[cfg(test)]
#[allow(unused_imports)]
use crate::config::Config;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use crate::error::aft_storage_denial_message;
#[cfg(test)]
#[allow(unused_imports)]
use crate::policy_path::policy_path;
#[cfg(test)]
#[allow(unused_imports)]
use crate::sandbox_profile::{SandboxDataPolicy, SandboxProfile, SandboxProfileError};
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use directory_scope::directory_scope;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[path = "access_policy_audit2_tests.rs"]
mod audit2_tests;
#[cfg(test)]
#[path = "access_policy_outside_roots_tests.rs"]
mod outside_roots_tests;
