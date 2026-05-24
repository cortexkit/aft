#[path = "helpers/mod.rs"]
mod test_helpers;

mod helpers {
    pub use crate::test_helpers::AftProcess;
}

#[path = "integration/semantic_test.rs"]
mod semantic_test;
