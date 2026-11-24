use std::path::Path;

use aft_index::data_layer::checkout::{exactness, exactness_port::ExactnessDeadCode, CheckoutView};

use crate::inspect::InspectEntryPoints;

pub fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    exactness::run(args, &InspectEntryPoints)
}
