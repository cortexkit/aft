//! Shell gates used by tests that hold a process until a file appears.
#![allow(dead_code)]

/// Render a POSIX shell loop. `file` is a shell word and `sleep` is the
/// existing sleep command and interval, for example `sleep 0.05`.
pub(crate) fn gate(file: &str, sleep: &str) -> String {
    gate_with_limit(file, sleep, 600, "-f")
}

pub(crate) fn gate_exists(file: &str, sleep: &str) -> String {
    gate_with_limit(file, sleep, 600, "-e")
}

pub(crate) fn gate_socket(file: &str, sleep: &str) -> String {
    gate_with_limit(file, sleep, 600, "-S")
}

pub(crate) fn powershell_gate(file: &str, millis: u32) -> String {
    let file = file.replace('\'', "''");
    format!("$aftGateFile = '{file}'; $aftGateParent = Split-Path -Parent $aftGateFile; $aftGateDeadline = [DateTime]::UtcNow.AddSeconds(600); while (-not (Test-Path -LiteralPath $aftGateFile) -and (Test-Path -LiteralPath $aftGateParent -PathType Container) -and [DateTime]::UtcNow -lt $aftGateDeadline) {{ Start-Sleep -Milliseconds {millis} }}")
}

fn gate_with_limit(file: &str, sleep: &str, seconds: u64, predicate: &str) -> String {
    format!(
        "(aft_gate_file={file}; \
         aft_gate_parent=$(cd \"$(dirname \"$aft_gate_file\")\" 2>/dev/null && pwd -P) || exit 0; \
         aft_gate_deadline=$(($(date +%s) + {seconds})); \
         while [ ! {predicate} \"$aft_gate_file\" ] && [ -d \"$aft_gate_parent\" ] && \
         [ \"$(date +%s)\" -lt \"$aft_gate_deadline\" ]; do {sleep}; done)"
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::gate_with_limit;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    struct Shell(std::process::Child);

    impl Drop for Shell {
        fn drop(&mut self) {
            if matches!(self.0.try_wait(), Ok(None)) {
                unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
                let _ = self.0.wait();
            }
        }
    }

    #[test]
    fn gate_ends_when_parent_disappears_or_limit_passes() {
        fn wait(mut child: Shell) -> Duration {
            let started = Instant::now();
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    assert!(status.success());
                    return started.elapsed();
                }
                if started.elapsed() > Duration::from_secs(5) {
                    panic!("gate did not end");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let file = format!("'{}'", dir.path().join("release").display());
        let marker = dir.path().join("started");
        let script = format!(
            ": > '{}'; {}",
            marker.display(),
            gate_with_limit(&file, "sleep 0.02", 60, "-f")
        );
        let mut child = Shell(
            Command::new("/bin/sh")
                .args(["-c", &script])
                .stdout(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let started = Instant::now();
        while !marker.exists() {
            assert!(started.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(10));
        }
        // Let the shell resolve the parent before removing it.
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "gate exited before removal"
        );
        dir.close().unwrap();
        assert!(wait(child) < Duration::from_secs(5));

        let dir = tempfile::tempdir().unwrap();
        let file = format!("'{}'", dir.path().join("release").display());
        let child = Shell(
            Command::new("/bin/sh")
                .args(["-c", &gate_with_limit(&file, "sleep 0.02", 2, "-f")])
                .stdout(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let elapsed = wait(child);
        assert!(elapsed >= Duration::from_millis(500));
        assert!(elapsed < Duration::from_secs(5));
    }
}
