impl ExactnessDeadCode for InspectEntryPoints {
    fn findings(&self, view: &CheckoutView, storage: &Path) -> Result<serde_json::Value, String> {
        crate::inspect::dead_code_gate::exactness_rows(view, storage)
    }
}
