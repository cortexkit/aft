//! Process-wide registry of LSP child PIDs spawned by `LspClient::spawn`.
//!
//! Mirrors the `BgTaskRegistry` pattern: `Arc`-cloneable handle that the
//! signal handler thread can use to SIGKILL all child language servers
//! before the aft process exits. Without this registry, LSP children get
//! orphaned to PID 1 when aft is SIGTERM'd by its parent (e.g., during
//! plugin bridge.shutdown() or e2e test cleanup), accumulating across runs.
//!
//! The registry intentionally does NOT do graceful shutdown — that takes
//! up to 5 seconds per server (shutdown request + exit notification +
//! poll). Signal handlers must finish quickly. Graceful shutdown still
//! happens on the natural stdin-closed exit path via `LspManager::shutdown_all`.

use std::collections::HashSet;
use std::io;
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
pub struct LspChildRegistry {
    inner: Arc<Mutex<HashSet<u32>>>,
}

impl LspChildRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Track a newly-spawned LSP child PID.
    pub fn track(&self, pid: u32) {
        if let Ok(mut set) = self.inner.lock() {
            set.insert(pid);
        }
    }

    /// Spawn a child while holding the same mutex used by signal cleanup, then
    /// insert its PID before releasing that mutex. This closes the SIGINT /
    /// SIGTERM spawn→track race: if cleanup starts concurrently, it blocks
    /// until the just-spawned child is present in the tracked set.
    pub fn spawn_tracked(&self, command: &mut Command) -> io::Result<Child> {
        let mut set = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("LSP child registry mutex poisoned"))?;
        let child = command.spawn()?;
        set.insert(child.id());
        Ok(child)
    }

    /// Forget a PID (called when the client is dropped or shut down gracefully).
    pub fn untrack(&self, pid: u32) {
        if let Ok(mut set) = self.inner.lock() {
            set.remove(&pid);
        }
    }

    /// Snapshot of currently-tracked PIDs.
    pub fn pids(&self) -> Vec<u32> {
        self.inner
            .lock()
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Force-kill every tracked child synchronously. Used by the signal
    /// handler to prevent orphaned LSP processes when aft is SIGTERM'd.
    /// Returns the number of process groups that were sent SIGKILL.
    ///
    /// On Unix, kills the entire process group (via `killpg`) rather than
    /// just the wrapper PID. Necessary because npm-wrapped LSP servers like
    /// biome ship as `node biome lsp-proxy` shims that spawn the real
    /// `cli-darwin-arm64 biome lsp-proxy` as a child; killing only the
    /// wrapper leaves the real server orphaned to PID 1.
    ///
    /// `LspClient::spawn` puts each child in its own session via `setsid()`
    /// so `pgid == child.id()`.
    #[cfg(unix)]
    pub fn kill_all(&self) -> usize {
        use std::os::raw::c_int;
        let pids = self.pids();
        let mut killed = 0;
        for pid in pids {
            // SIGKILL = 9. We use the raw libc call rather than crossbeam
            // because we're inside a signal-handler context where allocator
            // and channel use is risky.
            // SAFETY: killpg(2) is async-signal-safe.
            unsafe {
                let pgid = pid as libc::pid_t;
                let rc = libc::killpg(pgid, 9 as c_int);
                if rc == 0 {
                    killed += 1;
                }
            }
        }
        killed
    }

    /// Windows fallback: best-effort kill via `taskkill /F /T`. The `/T`
    /// flag kills the entire process tree (Windows analogue of process
    /// groups). Not technically async-signal-safe but Windows doesn't
    /// deliver signals the same way.
    #[cfg(not(unix))]
    pub fn kill_all(&self) -> usize {
        let pids = self.pids();
        let mut killed = 0;
        for pid in pids {
            if std::process::Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .status()
                .is_ok()
            {
                killed += 1;
            }
        }
        killed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_untrack_pids_round_trip() {
        let reg = LspChildRegistry::new();
        reg.track(100);
        reg.track(200);
        let mut pids = reg.pids();
        pids.sort();
        assert_eq!(pids, vec![100, 200]);
        reg.untrack(100);
        assert_eq!(reg.pids(), vec![200]);
    }

    #[test]
    fn clones_share_state() {
        let a = LspChildRegistry::new();
        let b = a.clone();
        a.track(42);
        assert_eq!(b.pids(), vec![42]);
        b.untrack(42);
        assert!(a.pids().is_empty());
    }

    #[test]
    fn untracking_unknown_pid_is_safe() {
        let reg = LspChildRegistry::new();
        reg.untrack(999); // no-op, no panic
        assert!(reg.pids().is_empty());
    }

    #[test]
    fn kill_all_with_no_pids_returns_zero() {
        let reg = LspChildRegistry::new();
        assert_eq!(reg.kill_all(), 0);
    }

    #[test]
    fn spawn_tracked_records_pid_before_returning() {
        let reg = LspChildRegistry::new();
        let mut command = if cfg!(windows) {
            let mut command = std::process::Command::new("cmd");
            command.args(["/C", "exit", "0"]);
            command
        } else {
            let mut command = std::process::Command::new("sh");
            command.args(["-c", "exit 0"]);
            command
        };

        let mut child = reg.spawn_tracked(&mut command).expect("spawn tracked");
        let pid = child.id();
        assert!(reg.pids().contains(&pid));
        let _ = child.wait();
        reg.untrack(pid);
    }

    // Regression for the npm-wrapper orphan bug: biome ships as `node
    // biome lsp-proxy` (the wrapper) that spawns
    // `cli-darwin-arm64 biome lsp-proxy` (the actual server) as a child.
    // Killing just the wrapper PID via `kill(2)` leaves the real server
    // orphaned to PID 1. `killpg(2)` kills the whole group.
    //
    // This test simulates that two-process structure with a shell pipeline:
    // a parent shell that forks a child `sleep`. The parent stays attached
    // (via wait), so both die when the group is killed.
    #[cfg(unix)]
    #[test]
    fn kill_all_kills_process_group_not_just_wrapper_pid() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::Duration;

        // Spawn a wrapper that forks a child and waits for it. Print the
        // child PID to stdout so we can verify it's killed too.
        let mut child = unsafe {
            let mut cmd = Command::new("sh");
            cmd.arg("-c")
                .arg("sleep 60 & echo $! ; wait")
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            // setsid() so wrapper becomes its own process-group leader,
            // matching what LspClient::spawn does.
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
            cmd.spawn().expect("spawn wrapper")
        };

        // Read the child PID from stdout.
        let mut stdout = child.stdout.take().expect("stdout pipe");
        let mut buf = String::new();
        use std::io::Read;
        // Give the shell a moment to print the PID.
        let mut byte = [0u8; 1];
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match stdout.read(&mut byte) {
                Ok(0) => break,
                Ok(_) => {
                    if byte[0] == b'\n' {
                        break;
                    }
                    buf.push(byte[0] as char);
                }
                Err(_) => break,
            }
        }
        let grandchild_pid: u32 = buf.trim().parse().expect("parse grandchild PID");

        // Verify both are alive before kill.
        let wrapper_pid = child.id();
        assert!(
            crate::bash_background::process::is_process_alive(wrapper_pid),
            "wrapper should be alive"
        );
        assert!(
            crate::bash_background::process::is_process_alive(grandchild_pid),
            "grandchild should be alive"
        );

        // Track wrapper PID, kill the group.
        let reg = LspChildRegistry::new();
        reg.track(wrapper_pid);
        let killed = reg.kill_all();
        assert_eq!(killed, 1, "should report 1 group killed");

        // Reap the wrapper so we don't leave a zombie.
        let _ = child.wait();

        // Give the kernel a moment to propagate SIGKILL through the group.
        thread::sleep(Duration::from_millis(100));

        // Both must be dead. This is the actual regression assertion:
        // without killpg() the grandchild would survive as an orphan.
        assert!(
            !crate::bash_background::process::is_process_alive(wrapper_pid),
            "wrapper must be dead after killpg"
        );
        assert!(
            !crate::bash_background::process::is_process_alive(grandchild_pid),
            "grandchild must be dead after killpg (this was the npm-wrapper orphan bug)"
        );
    }
}
