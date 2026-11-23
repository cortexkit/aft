pub use aft_db::persisted_format::*;

#[cfg(test)]
mod tests {
    #[test]
    fn backup_and_bash_writer_versions_stay_compatible() {
        assert_eq!(crate::backup::SCHEMA_VERSION, 6);
        assert_eq!(crate::backup::ORDINARY_SCHEMA_VERSION, 5);
        assert_eq!(crate::bash_background::persistence::SCHEMA_VERSION, 7);
    }
}
