pub use aft_base::os_metadata::*;

#[cfg(test)]
#[allow(unused_imports)]
use std::ffi::OsStr;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::Path;

#[cfg(test)]
#[path = "os_metadata_tests.rs"]
mod tests;
