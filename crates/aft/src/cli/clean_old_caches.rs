//! Explicit, confirmation-gated cleanup after moving rebuildable caches.
use std::ffi::OsString;
use std::io::{self, BufRead, IsTerminal, Write};

const HELP: &str = "usage: aft clean-old-caches [--yes]\n\nExplicit legacy exception: remove only views/, blobs/, callgraph/, semantic/, index/, symbols/ and inspect/ directly under the resolved, proven AFT storage root (aft.db or its owner marker). Harness folders and durable state are never selected. An active enrolled cache root on a separate device is required. Symlinks, foreign-device domains, nested mounts and hardlinks refuse cleanup. The prompt lists the exact folders and sizes and requires delete, or explicit --yes.";

pub fn run(args: Vec<OsString>) -> i32 {
    if args.len() == 1 && matches!(args[0].to_str(), Some("--help" | "-h")) {
        println!("{HELP}");
        return 0;
    }
    if args.iter().any(|arg| arg != "--yes") || args.len() > 1 {
        eprintln!("{HELP}");
        return 2;
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("{error}");
            return 1;
        }
    };
    let tiers = aft::subc_config::read_local_cortexkit_config_tiers(
        aft::subc_config::cortexkit_user_config_path().as_deref(),
        &cwd,
    );
    run_with(
        &tiers,
        &aft::bash_background::storage_dir(None),
        !args.is_empty(),
        io::stdin().is_terminal(),
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
    )
}

fn run_with(
    tiers: &[aft::config_resolve::ConfigTier],
    storage: &std::path::Path,
    yes: bool,
    interactive: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> i32 {
    let report = aft::root_cache::clean_old_caches(tiers, storage, |candidates| {
        let mut total = 0u64;
        for candidate in candidates {
            total = total.saturating_add(candidate.bytes);
            if writeln!(
                output,
                "{}\t{} bytes",
                candidate.path.display(),
                candidate.bytes
            )
            .is_err()
            {
                return false;
            }
        }
        if writeln!(
            output,
            "Total: {total} bytes in {} old cache directories",
            candidates.len()
        )
        .is_err()
        {
            return false;
        }
        if yes {
            return output.flush().is_ok();
        }
        if !interactive {
            let _ = writeln!(
                output,
                "Refused: a terminal and exact confirmation are required (or explicit --yes)."
            );
            return false;
        }
        if writeln!(output, "Type delete to remove these old caches:")
            .and_then(|_| output.flush())
            .is_err()
        {
            return false;
        }
        let mut answer = String::new();
        input.read_line(&mut answer).is_ok() && answer.trim_end_matches(['\r', '\n']) == "delete"
    });
    let failed = report.error.is_some();
    let _ = writeln!(output, "{}", serde_json::to_string(&report).unwrap());
    i32::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires AFT_TEST_CACHE_VOLUME on a real second filesystem"]
    fn old_cache_command_lists_before_exact_confirmation_and_keeps_durable_state() {
        let temp = tempfile::tempdir().unwrap();
        let storage = temp.path().join("storage");
        let volume = std::env::var_os("AFT_TEST_CACHE_VOLUME")
            .expect("set AFT_TEST_CACHE_VOLUME to a mounted second filesystem");
        let cache = tempfile::tempdir_in(volume).unwrap();
        #[cfg(unix)]
        let _process_snapshot = {
            use std::os::unix::fs::PermissionsExt;
            // Fake `ps` to report no running daemons for this real-volume deletion
            // test; a separate test verifies the upgrade-window refusal. The scan
            // asks `ps` for executables (`pid=,comm=`) and for command lines
            // (`pid=,args=`), and refuses a listing that leaves out the scanning
            // process, so both answers list it: the script's parent.
            let bin = temp.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            let ps = bin.join("ps");
            std::fs::write(
                &ps,
                "#!/bin/sh\ncase \"$2\" in\n*comm=*) echo \"$PPID /opt/aft-cli-test\" ;;\n*) echo \"$PPID /opt/aft-cli-test --ignored\" ;;\nesac\n",
            )
            .unwrap();
            std::fs::set_permissions(&ps, std::fs::Permissions::from_mode(0o700)).unwrap();
            struct RestorePath(Option<OsString>);
            impl Drop for RestorePath {
                fn drop(&mut self) {
                    match &self.0 {
                        Some(path) => unsafe { std::env::set_var("PATH", path) },
                        None => unsafe { std::env::remove_var("PATH") },
                    }
                }
            }
            let previous = std::env::var_os("PATH");
            let mut paths = vec![bin];
            if let Some(path) = &previous {
                paths.extend(std::env::split_paths(path));
            }
            unsafe { std::env::set_var("PATH", std::env::join_paths(paths).unwrap()) };
            RestorePath(previous)
        };
        let parent = cache.path().join("volume");
        std::fs::create_dir(&parent).unwrap();
        struct RestoreStorage(Option<OsString>);
        impl Drop for RestoreStorage {
            fn drop(&mut self) {
                match &self.0 {
                    Some(value) => unsafe { std::env::set_var("AFT_STORAGE_DIR", value) },
                    None => unsafe { std::env::remove_var("AFT_STORAGE_DIR") },
                }
            }
        }
        let _restore = RestoreStorage(std::env::var_os("AFT_STORAGE_DIR"));
        unsafe { std::env::set_var("AFT_STORAGE_DIR", &storage) };
        for path in [
            "index/old",
            "views/old",
            "pi/callgraph/old",
            "aft.db",
            "checkpoints/keep",
            "backups/keep",
            "bash-tasks/keep",
        ] {
            let path = storage.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "1234").unwrap();
        }
        let tiers = [aft::config_resolve::ConfigTier {
            tier: "user".into(),
            source: "test".into(),
            doc: serde_json::json!({"storage":{"cache_dir":parent}}).to_string(),
        }];
        for (interactive, answer) in [(false, "delete\n"), (true, "yes\n")] {
            let mut output = Vec::new();
            assert_eq!(
                run_with(
                    &tiers,
                    &storage,
                    false,
                    interactive,
                    &mut answer.as_bytes(),
                    &mut output
                ),
                1
            );
            let text = String::from_utf8(output).unwrap();
            assert!(text.contains(" bytes in 2 old cache directories"), "{text}");
            assert!(text.contains("Total:"));
            assert!(text.contains("old_cache_confirmation_required"));
            assert!(storage.join("index/old").is_file());
        }
        let mut output = Vec::new();
        assert_eq!(
            run_with(
                &tiers,
                &storage,
                false,
                true,
                &mut "delete\n".as_bytes(),
                &mut output
            ),
            0
        );
        let text = String::from_utf8(output).unwrap();
        assert!(text.find("Total:").unwrap() < text.find("Type delete").unwrap());
        assert!(!storage.join("index").exists());
        for path in [
            "aft.db",
            "checkpoints/keep",
            "backups/keep",
            "bash-tasks/keep",
            "pi/callgraph/old",
        ] {
            assert!(storage.join(path).is_file());
        }
    }
}
