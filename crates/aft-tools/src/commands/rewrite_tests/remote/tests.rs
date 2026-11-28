use super::*;
use crate::tools_test_support::{handle_with_policy, rendered_status};

include!("orchestration_tests.rs");
include!("recovery_tests.rs");

#[path = "read_only.rs"]
mod read_only;

#[path = "runon_capture_tests.rs"]
mod runon_capture_tests;

#[path = "return_paths_tests.rs"]
mod return_paths_tests;
