#[allow(unused_imports)]
pub use aft_base::test_gate::*;

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
