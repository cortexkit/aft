mod tests {
    #[test]
    fn registered_artifact_consumers_do_not_reopen_paths_directly() {
        let rust_sources = [
            include_str!("../../../bash_background/buffer.rs"),
            include_str!("../../../bash_background/registry.rs"),
            include_str!("../../../bash_background/process.rs"),
            include_str!("../../../bash_background/pty_process.rs"),
            include_str!("../../../../../aft-base/src/task_watches.rs"),
            include_str!("../../../bash_background/watchdog.rs"),
            include_str!("../../bash_status.rs"),
        ];
        for source in rust_sources {
            // A Windows checkout can have CRLF line endings. Without this the
            // split marker never matches there, so the guard would scan test
            // modules on Windows but not elsewhere. Scan the same production
            // text on every platform.
            let source = source.replace("\r\n", "\n");
            let production = source
                .split("#[cfg(test)]\nmod tests")
                .next()
                .unwrap_or(&source);
            for forbidden in [
                "File::open(&task.paths",
                "fs::read(&task.paths",
                "fs::read_to_string(&task.paths",
                "File::open(path)?",
            ] {
                assert!(
                    !production.contains(forbidden),
                    "registered artifact consumer contains raw path read: {forbidden}"
                );
            }
        }
        for source in [
            include_str!("../../../../../../packages/opencode-plugin/src/tools/bash.ts"),
            include_str!("../../../../../../packages/opencode-plugin/src/tools/bash_watch.ts"),
            include_str!("../../../../../../packages/pi-plugin/src/tools/bash.ts"),
        ] {
            for forbidden in [
                "fs.readFile(outputPath)",
                "fs.readFile(details.output_path)",
                "fs.open(outputPath",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "plugin artifact consumer contains raw path read: {forbidden}"
                );
            }
        }
    }
}
