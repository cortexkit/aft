#![allow(dead_code)]

use std::io;
use std::path::Path;

/// Give a tool-behavior fixture project evidence without turning it into a Git
/// repository or adding source files to its symbol and search results.
pub fn mark_project(root: &Path) -> io::Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join("Makefile"))
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn project_tempdir() -> tempfile::TempDir {
    let directory = tempfile::tempdir().expect("create test project directory");
    mark_project(directory.path()).expect("mark test project");
    directory
}

/// Preserve existing Git or workspace evidence; only bare tool fixtures need
/// the extra marker. In particular, checked-in source fixtures stay untouched.
pub fn ensure_project(root: &Path) -> io::Result<()> {
    let decision = aft::folder_admission::probe(
        root,
        &Default::default(),
        aft::folder_admission::ProbeBudgets {
            time: std::time::Duration::from_secs(5),
            ..Default::default()
        },
    );
    if decision.classification == aft::folder_admission::FolderClassification::NotProject {
        mark_project(root)?;
    }
    Ok(())
}
