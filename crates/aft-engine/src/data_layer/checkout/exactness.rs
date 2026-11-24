use std::path::Path;

use aft_index::data_layer::checkout::{exactness, exactness_port::ExactnessDeadCode, CheckoutView};

use crate::inspect::InspectEntryPoints;

impl ExactnessDeadCode for InspectEntryPoints {
    fn findings(&self, view: &CheckoutView, storage: &Path) -> Result<serde_json::Value, String> {
        crate::inspect::dead_code_gate::exactness_rows(view, storage)
    }
}

pub fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    exactness::run(args, &InspectEntryPoints)
}
