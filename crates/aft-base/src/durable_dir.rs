//! Publish private durable directories before committing records inside them.

use std::path::Path;

/// Publish a new durable store directory and any missing ancestors into their
/// parents before committing records inside it. Existing directories cost no sync.
pub(crate) fn create_private_durable_dir(path: &Path) -> std::io::Result<()> {
    crate::production_storage::refuse_write(path)?;
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        // A durable child entry is insufficient when an ancestor can vanish.
        // Existing ancestors cost nothing; each new one publishes into its own
        // parent before any records can be committed under this namespace.
        create_private_durable_dir(parent)?;
    }
    let created = crate::private_storage::create_dir(path);
    match created {
        Ok(()) => {
            crate::durability::record(crate::durability::EventKind::DirectoryCreated, path);
            if let Some(parent) = path.parent() {
                crate::durability::sync_dir(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_missing_ancestors_and_accepts_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("parent").join("child");
        create_private_durable_dir(&nested).unwrap();
        assert!(nested.is_dir());
        create_private_durable_dir(&nested).unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"unchanged").unwrap();
        assert!(create_private_durable_dir(&file).is_err());
        assert_eq!(std::fs::read(file).unwrap(), b"unchanged");
    }
}
