use aft_index::data_layer::checkout::exactness;

use crate::inspect::InspectEntryPoints;

pub fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    exactness::run(args, &InspectEntryPoints)
}
