#[path = "helpers/context_storage.rs"]
mod context_storage;

#[path = "helpers/mod.rs"]
mod test_helpers;

mod helpers {
    pub use crate::test_helpers::{user_config, AftProcess};
}

#[path = "integration/semantic_test.rs"]
mod semantic_test;
