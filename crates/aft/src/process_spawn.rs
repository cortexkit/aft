/// Add path context only after an operating-system process spawn has failed.
///
/// Spawn errors can mean that the child working directory is missing, or that
/// the executable itself cannot be found. Checking here keeps filesystem
/// probes off the successful process-launch path.
pub(crate) fn format_spawn_failure(
    context: &str,
    program: &std::path::Path,
    workdir: &std::path::Path,
    error: impl std::fmt::Display,
) -> String {
    let workdir_problem = match std::fs::metadata(workdir) {
        Ok(metadata) if !metadata.is_dir() => Some("is not a directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some("does not exist"),
        _ => None,
    };
    let program_exists = if program.is_absolute() {
        program.exists()
    } else if program.components().count() > 1 {
        workdir.join(program).exists()
    } else {
        which::which(program.as_os_str()).is_ok()
    };

    if let Some(problem) = workdir_problem {
        format!(
            "{context}: working directory {problem}: {}: {error}",
            workdir.display()
        )
    } else if !program_exists {
        format!(
            "{context}: program not found: {}: {error}",
            program.display()
        )
    } else {
        format!("{context}: {error}")
    }
}

#[cfg(test)]
mod spawn_failure_tests {
    #[test]
    fn bash_background_spawn_error_names_missing_program() {
        let workdir = tempfile::tempdir().unwrap();
        let missing_program = workdir.path().join("missing-shell");
        let error = std::process::Command::new(&missing_program)
            .current_dir(workdir.path())
            .spawn()
            .unwrap_err();

        let message = super::format_spawn_failure(
            "failed to spawn background bash command",
            &missing_program,
            workdir.path(),
            error,
        );
        assert!(
            message.contains(&format!("program not found: {}", missing_program.display())),
            "spawn error did not identify the missing program: {message}"
        );
    }
}
