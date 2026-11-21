pub fn with_test_reader<T>(reader: Option<&str>, run: impl FnOnce() -> T) -> T {
    crate::late_result_bus::with_test_reader(reader, run)
}
