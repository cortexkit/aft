use std::path::Path;

use super::CheckoutView;
use crate::data_layer::dead_code_snapshot::EntryPoints;

/// The exactness harness uses the real inspect projection without owning it.
#[doc(hidden)]
pub trait ExactnessDeadCode: EntryPoints {
    fn findings(&self, view: &CheckoutView, storage: &Path) -> Result<serde_json::Value, String>;
}
