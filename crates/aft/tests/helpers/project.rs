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
