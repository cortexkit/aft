use std::collections::HashMap;
use std::ffi::OsString;
#[cfg(any(windows, test))]
#[cfg(all(test, unix))]
use std::fs;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use portable_pty::PtySize;

use crate::sandbox_spawn::SpawnPlan;

use super::persistence::{ExitMarker, TaskArtifact, TaskIoHandles, TaskPaths};
use super::pty_runtime::{CompletionCoordinator, PtyRuntime};

/// How long PTY setup cleanup waits for a killed child to be reaped. A
/// SIGKILLed child normally exits within milliseconds; the bound only exists so
/// a child that cannot finish exiting turns into a logged error instead of
/// hanging the caller forever.
const PTY_SETUP_REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A PTY child (and, during setup, the PTY master) that setup still owns. If
/// setup fails before handing the child to its waiter thread, dropping this
/// attempts to kill and reap the child within a deadline, logging reap failures.
struct SpawnedPtyChild {
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    master: Option<Box<dyn portable_pty::MasterPty + Send>>,
}

impl SpawnedPtyChild {
    fn new(child: Box<dyn portable_pty::Child + Send + Sync>) -> Self {
        Self {
            child: Some(child),
            master: None,
        }
    }
}

impl Drop for SpawnedPtyChild {
    fn drop(&mut self) {
        // Close the master before waiting on the child. On macOS a session
        // leader's exit drains its controlling terminal: the kernel waits for
        // pending terminal output (for example the echo of the newline and EOF
        // that portable_pty's writer sends when it is dropped) to be read from
        // the master. With no reader left and the master still open, the child
        // can never finish exiting and `wait` blocks forever. Closing the
        // master hangs up the terminal and releases that drain. Linux does not
        // drain on exit, which is why the hang is macOS-only.
        drop(self.master.take());
        let Some(mut child) = self.child.take() else {
            return;
        };
        // Dropping a PTY handle is not a termination guarantee: a child can
        // ignore HUP. Setup still owns this child, so kill and reap it here.
        #[cfg(unix)]
        if let Some(pid) = child.process_id() {
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
        let _ = child.kill();
        if let Err(error) = reap_killed_child(child.as_mut(), PTY_SETUP_REAP_TIMEOUT) {
            crate::slog_warn!("PTY setup cleanup: {error}");
        }
    }
}

/// Reap a child that has already been sent SIGKILL, polling instead of
/// blocking in `wait` so a child that cannot finish exiting surfaces as an
/// error after `timeout` rather than hanging the caller.
fn reap_killed_child(
    child: &mut (dyn portable_pty::Child + Send + Sync),
    timeout: std::time::Duration,
) -> Result<(), String> {
    let pid = child.process_id();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(()),
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(format!("reaping killed PTY child {pid:?} failed: {error}"));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "killed PTY child {pid:?} was not reaped within {}ms",
                timeout.as_millis()
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_pty_for_command(
    spawn_plan: &SpawnPlan,
    task_id: &str,
    session_id: &str,
    user_command: &str,
    shell_kind: super::BashShell,
    shell_path: &Path,
    paths: &TaskPaths,
    workdir: &Path,
    env: &HashMap<String, String>,
    rows: u16,
    cols: u16,
    wake_tx: crossbeam_channel::Sender<()>,
    io_handles: &mut TaskIoHandles,
    spawned_pgid: &mut Option<i32>,
    memory_monitor: &mut Option<super::memory_limit::Monitor>,
) -> Result<PtyRuntime, String> {
    #[cfg(unix)]
    {
        let _ = shell_kind;
        let mut args = if let Some(prepared) = spawn_plan.prepared_task() {
            vec![
                OsString::from("-c"),
                OsString::from(prepared.command_text()?),
            ]
        } else {
            vec![OsString::from("-c"), OsString::from(user_command)]
        };
        let program = if memory_monitor.is_some() {
            let mut gated = vec![
                OsString::from("-c"),
                OsString::from("read -r ready || exit 125; exec \"$@\""),
                OsString::from("aft-memory-gate"),
                shell_path.as_os_str().to_owned(),
            ];
            gated.append(&mut args);
            args = gated;
            std::ffi::OsStr::new("/bin/sh")
        } else {
            shell_path.as_os_str()
        };
        try_spawn_pty(
            spawn_plan,
            task_id,
            session_id,
            program,
            &args,
            paths,
            workdir,
            env,
            rows,
            cols,
            wake_tx,
            io_handles,
            spawned_pgid,
            memory_monitor,
        )
    }
    #[cfg(windows)]
    {
        let _ = shell_path;
        use crate::windows_shell::shell_candidates;

        let candidates = if shell_kind.is_powershell() {
            vec![crate::windows_shell::WindowsShell::Pwsh]
        } else {
            shell_candidates()
        };
        let attempt = crate::windows_shell::spawn_pty_candidates(&candidates, |shell| {
            let wrapper_body = if memory_monitor.is_some() {
                shell.memory_gated_wrapper_bytes(user_command, &paths.exit)
            } else {
                shell.wrapper_script_bytes(user_command, &paths.exit)
            };
            let wrapper_path = windows_wrapper_path(paths, shell);
            if let Err(error) = crate::private_storage::write(&wrapper_path, wrapper_body) {
                return Err(crate::windows_shell::PtyAttemptError::Wrapper(format!(
                    "write wrapper {wrapper_path:?}: {error}"
                )));
            }

            let mut wrapper_env;
            let env = match shell.wrapper_environment(user_command) {
                Some((key, value)) => {
                    wrapper_env = env.clone();
                    wrapper_env.insert(key.to_string(), value.to_string());
                    &wrapper_env
                }
                None => env,
            };

            let args: Vec<OsString> = shell
                .pty_wrapper_args(&wrapper_path)
                .into_iter()
                .map(OsString::from)
                .collect();

            try_spawn_pty(
                spawn_plan,
                task_id,
                session_id,
                std::ffi::OsStr::new(shell.binary().as_ref()),
                &args,
                paths,
                workdir,
                env,
                rows,
                cols,
                wake_tx.clone(),
                io_handles,
                spawned_pgid,
                memory_monitor,
                shell.uses_memory_file_gate(),
            )
            .map_err(crate::windows_shell::PtyAttemptError::Spawn)
        });
        match attempt {
            Ok((runtime, _)) => Ok(runtime),
            Err(crate::windows_shell::CandidateFailure::Stopped { error, .. }) => Err(error),
            Err(crate::windows_shell::CandidateFailure::Exhausted { last }) => Err(last
                .map(|(_, error)| error)
                .unwrap_or_else(|| "no Windows shell candidates available".into())),
        }
    }
}

#[cfg(windows)]
fn windows_wrapper_path(
    paths: &TaskPaths,
    shell: &crate::windows_shell::WindowsShell,
) -> std::path::PathBuf {
    let extension = match shell {
        crate::windows_shell::WindowsShell::Pwsh
        | crate::windows_shell::WindowsShell::Powershell => "ps1",
        crate::windows_shell::WindowsShell::Cmd => "bat",
        crate::windows_shell::WindowsShell::Posix(_) => "sh",
    };
    let stem = paths
        .json
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("wrapper");
    paths.dir.join(format!("{stem}.{extension}"))
}

#[allow(clippy::too_many_arguments)]
fn try_spawn_pty(
    spawn_plan: &SpawnPlan,
    task_id: &str,
    session_id: &str,
    program: &std::ffi::OsStr,
    args: &[OsString],
    paths: &TaskPaths,
    workdir: &Path,
    env: &HashMap<String, String>,
    rows: u16,
    cols: u16,
    wake_tx: crossbeam_channel::Sender<()>,
    io_handles: &mut TaskIoHandles,
    spawned_pgid: &mut Option<i32>,
    memory_monitor: &mut Option<super::memory_limit::Monitor>,
    #[cfg(windows)] memory_file_gate: bool,
) -> Result<PtyRuntime, String> {
    let (command, profile_handle) = crate::sandbox_spawn::pty_command_for_plan(
        spawn_plan,
        program,
        args,
        &paths.json,
        workdir,
        env,
    )?;
    let spawn_program = command
        .get_argv()
        .first()
        .cloned()
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let spawn_workdir = command
        .get_cwd()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| workdir.to_path_buf());
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("open PTY failed: {error}"))?;
    #[cfg(target_os = "macos")]
    let child = if crate::privacy_spawn::requested(env) {
        crate::privacy_spawn::spawn_pty(command, pair.master.as_ref())?
    } else {
        pair.slave.spawn_command(command).map_err(|error| {
            crate::bash_background::format_spawn_failure(
                "spawn PTY command failed",
                &spawn_program,
                &spawn_workdir,
                error,
            )
        })?
    };
    #[cfg(not(target_os = "macos"))]
    let child = pair.slave.spawn_command(command).map_err(|error| {
        crate::bash_background::format_spawn_failure(
            "spawn PTY command failed",
            &spawn_program,
            &spawn_workdir,
            error,
        )
    })?;
    if crate::privacy_spawn::requested(env) {
        crate::privacy_spawn::note_session(session_id);
    }
    drop(profile_handle);
    // Own the master in the child guard so every later setup error closes it
    // before cleanup waits for the session leader to exit.
    let mut child = SpawnedPtyChild {
        child: Some(child),
        master: Some(pair.master),
    };
    let child_pid = child.child.as_ref().unwrap().process_id();
    // Preserve group identity for scratch cleanup even if setup returns no runtime.
    *spawned_pgid = child_pid.map(|pid| pid as i32);
    if let Some(monitor) = memory_monitor.as_mut() {
        monitor.attach(child_pid.ok_or("memory cap launch PID missing")?)?;
    }
    let killer = child.child.as_ref().unwrap().clone_killer();
    #[cfg(test)]
    fail_pty_setup_for_test(child_pid)?;
    #[cfg(test)]
    observe_pty_setup_for_test(child_pid);
    let reader = child
        .master
        .as_ref()
        .unwrap()
        .try_clone_reader()
        .map_err(|error| format!("clone PTY reader failed: {error}"))?;
    let mut writer = child
        .master
        .as_ref()
        .unwrap()
        .take_writer()
        .map_err(|error| format!("take PTY writer failed: {error}"))?;
    if memory_monitor.is_some() {
        use std::io::Write;
        #[cfg(windows)]
        if memory_file_gate {
            crate::windows_shell::release_memory_file_gate(&paths.exit)?;
        }
        #[cfg(unix)]
        let ready: &[u8] = b"\n";
        #[cfg(windows)]
        let ready: &[u8] = if memory_file_gate {
            b""
        } else {
            b"aft-memory-ready\n"
        };
        writer
            .write_all(ready)
            .map_err(|error| format!("memory cap launch gate failed: {error}"))?;
    }

    let reader_done = Arc::new(AtomicBool::new(false));
    let reader_eof = Arc::new(AtomicBool::new(false));
    let exit_observed = Arc::new(AtomicBool::new(false));
    let was_killed = Arc::new(AtomicBool::new(false));
    let coordinator = Arc::new(CompletionCoordinator::new(
        task_id.to_string(),
        session_id.to_string(),
        wake_tx,
    ));

    let writer = Arc::new(Mutex::new(writer));
    let spill = io_handles
        .clone_file(TaskArtifact::Pty)
        .map_err(|error| format!("failed to clone PTY spill handle: {error}"))?;
    let exit = io_handles
        .clone_file(TaskArtifact::Exit)
        .map_err(|error| format!("failed to clone PTY exit handle: {error}"))?;
    spawn_reader(
        reader,
        spill,
        Arc::clone(&reader_done),
        Arc::clone(&reader_eof),
        Arc::clone(&coordinator),
        Some(Arc::clone(&writer)),
    )
    .map_err(|error| format!("failed to start PTY reader: {error}"))?;
    spawn_waiter(
        child.child.take().unwrap(),
        exit,
        Arc::clone(&was_killed),
        Arc::clone(&exit_observed),
        Arc::clone(&coordinator),
    )
    .map_err(|error| format!("failed to start PTY waiter: {error}"))?;

    Ok(PtyRuntime {
        master: child.master.take(),
        writer,
        killer,
        child_pid,
        reader_done,
        reader_eof,
        exit_observed,
        was_killed,
        coordinator,
        #[cfg(any(windows, test))]
        output_drain: cfg!(windows).then(super::pty_runtime::PtyOutputDrain::default),
    })
}

#[cfg(test)]
thread_local! {
    static SETUP_FAILURE: std::cell::RefCell<Option<(std::path::PathBuf, Arc<std::sync::atomic::AtomicU32>)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
thread_local! {
    static SETUP_OBSERVATION: std::cell::RefCell<Option<(std::path::PathBuf, Arc<std::sync::atomic::AtomicU32>)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn observe_pty_setup_for_test(pid: Option<u32>) {
    SETUP_OBSERVATION.with(|observation| {
        if let Some((ready, observed)) = observation.borrow_mut().take() {
            observed.store(pid.unwrap(), Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert!(ready.exists(), "PTY child never armed its HUP handler");
        }
    });
}

#[cfg(test)]
fn fail_pty_setup_for_test(pid: Option<u32>) -> Result<(), String> {
    SETUP_FAILURE.with(|failure| {
        if let Some((ready, observed)) = failure.borrow_mut().take() {
            observed.store(pid.unwrap(), Ordering::SeqCst);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::yield_now();
            }
            assert!(ready.exists(), "PTY child never armed its HUP handler");
            Err("clone PTY reader failed: injected setup failure".into())
        } else {
            Ok(())
        }
    })
}

/// DSR escape sequence `\x1b[6n` is 4 bytes, so a carry of the last 3 bytes of
/// the running stream is enough to detect a needle straddling any read boundary
/// (see `DsrScanner`).
const DSR_CARRY_OVER: usize = 3;

pub(crate) fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    mut file: std::fs::File,
    reader_done: Arc<AtomicBool>,
    reader_eof: Arc<AtomicBool>,
    coordinator: Arc<CompletionCoordinator>,
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
) -> io::Result<()> {
    super::worker_thread::spawn(super::worker_thread::Worker::PtyReader, move || {
        let result = (|| -> io::Result<()> {
            let mut buf = [0_u8; 8192];
            let mut dsr = DsrScanner::default();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        file.write_all(&buf[..n])?;
                        file.flush()?;
                        if dsr.scan(&buf[..n]) {
                            // Some Windows console hosts/apps query the
                            // terminal cursor position with DSR (ESC[6n)
                            // before accepting input. A real terminal answers
                            // with ESC[row;colR; without that response the
                            // process can sit forever after emitting only the
                            // query. We own both ends of the PTY, so provide a
                            // conservative 1;1 response.
                            if let Some(writer) = writer.as_ref() {
                                if let Ok(mut writer) = writer.lock() {
                                    let _ = writer.write_all(b"\x1b[1;1R");
                                    let _ = writer.flush();
                                }
                            }
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })();
        drop(file);
        if let Err(ref error) = result {
            crate::slog_warn!(
                "PTY reader for {}:{} stopped with error: {error}",
                coordinator.session_id,
                coordinator.task_id
            );
        }
        #[cfg(test)]
        wait_on_pty_reader_gate_for_test(&coordinator.task_id);
        reader_eof.store(result.is_ok(), Ordering::SeqCst);
        reader_done.store(true, Ordering::SeqCst);
        coordinator.signal_one_done();
    })
}

/// Test gates that hold a PTY reader after it reached end-of-file and before
/// it reports itself done (and so before the watchdog wake), keyed by task id
/// so parallel tests do not collide.
#[cfg(test)]
static PTY_READER_GATES: std::sync::OnceLock<
    Mutex<HashMap<String, crossbeam_channel::Receiver<()>>>,
> = std::sync::OnceLock::new();

/// Hold the reader of `task_id` at end-of-file until the returned sender is
/// used (or dropped).
#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn install_pty_reader_gate_for_test(task_id: &str) -> crossbeam_channel::Sender<()> {
    let (release_tx, release_rx) = crossbeam_channel::bounded(1);
    PTY_READER_GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .insert(task_id.to_string(), release_rx);
    release_tx
}

#[cfg(test)]
fn wait_on_pty_reader_gate_for_test(task_id: &str) {
    let gate = PTY_READER_GATES
        .get()
        .and_then(|gates| gates.lock().unwrap().remove(task_id));
    if let Some(release) = gate {
        let _ = release.recv_timeout(std::time::Duration::from_secs(30));
    }
}

/// Detects the DSR cursor-position query `\x1b[6n` (4 bytes) in a byte stream
/// delivered in arbitrary chunks. A `read()` may return as little as one byte,
/// so the 4-byte needle can split across ANY number of reads. We keep a rolling
/// carry of the last 3 bytes seen and prepend it to each new chunk before
/// scanning. Crucially the carry is taken from the COMBINED (carry + chunk)
/// buffer, not just the chunk's own tail — that is what lets detection survive
/// more than two reads (e.g. `\x1b`, `[`, `6`, `n` arriving as four reads).
#[derive(Default)]
struct DsrScanner {
    carry: Vec<u8>,
}

impl DsrScanner {
    fn scan(&mut self, chunk: &[u8]) -> bool {
        let mut combined = Vec::with_capacity(self.carry.len() + chunk.len());
        combined.extend_from_slice(&self.carry);
        combined.extend_from_slice(chunk);
        let detected = combined.windows(4).any(|w| w == b"\x1b[6n");
        // Carry the last 3 bytes of the combined stream forward: a 4-byte
        // needle straddling this boundary keeps at most 3 bytes on this side.
        let start = combined.len().saturating_sub(DSR_CARRY_OVER);
        self.carry.clear();
        self.carry.extend_from_slice(&combined[start..]);
        detected
    }
}

pub(crate) fn spawn_waiter(
    child: Box<dyn portable_pty::Child + Send + Sync>,
    mut exit_file: std::fs::File,
    was_killed: Arc<AtomicBool>,
    exit_observed: Arc<AtomicBool>,
    coordinator: Arc<CompletionCoordinator>,
) -> io::Result<()> {
    let mut owned_child = SpawnedPtyChild::new(child);
    super::worker_thread::spawn(super::worker_thread::Worker::PtyWaiter, move || {
        let mut child = owned_child.child.take().unwrap();
        let marker = loop {
            match child.wait() {
                Ok(status) => {
                    if was_killed.load(Ordering::SeqCst) {
                        break ExitMarker::Killed;
                    }
                    let code = i32::try_from(status.exit_code()).unwrap_or(i32::MAX);
                    break ExitMarker::Code(code);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    crate::slog_warn!(
                        "PTY waiter for {}:{} failed: {error}",
                        coordinator.session_id,
                        coordinator.task_id
                    );
                    break ExitMarker::Killed;
                }
            }
        };

        if let Err(error) = write_exit_marker(&mut exit_file, &marker) {
            crate::slog_warn!(
                "PTY waiter for {}:{} failed to write exit marker: {error}",
                coordinator.session_id,
                coordinator.task_id
            );
        }
        drop(exit_file);
        exit_observed.store(true, Ordering::SeqCst);
        coordinator.signal_one_done();
    })
}

fn write_exit_marker(file: &mut File, marker: &ExitMarker) -> io::Result<()> {
    if file.metadata()?.len() > 0 {
        return Ok(());
    }
    let content = match marker {
        ExitMarker::Code(code) => code.to_string(),
        ExitMarker::Killed => "killed".to_string(),
    };
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(content.as_bytes())?;
    Ok(())
}

// Every test in this module exercises Unix-only PTY paths (`#[cfg(unix)]`
// shell resolution + the spawn_waiter), so gate the whole module on `unix` to
// avoid unused-import / dead-code warnings when cross-compiling for Windows.
#[cfg(all(test, unix))]
mod tests {
    use std::io;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use portable_pty::{Child, ChildKiller, ExitStatus};

    use super::*;

    #[derive(Debug)]
    struct FakeKiller;

    impl ChildKiller for FakeKiller {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(FakeKiller)
        }
    }

    #[derive(Debug)]
    struct InterruptedOnceChild {
        waits: usize,
    }

    impl ChildKiller for InterruptedOnceChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(FakeKiller)
        }
    }

    impl Child for InterruptedOnceChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            self.waits += 1;
            if self.waits == 1 {
                Err(io::Error::from(io::ErrorKind::Interrupted))
            } else {
                Ok(ExitStatus::with_exit_code(0))
            }
        }

        fn process_id(&self) -> Option<u32> {
            None
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    #[test]
    fn pty_waiter_retries_wait_on_interrupted() {
        let temp = tempfile::tempdir().unwrap();
        let exit_path = temp.path().join("task.exit");
        let exit_file = File::create(&exit_path).unwrap();
        let (wake_tx, wake_rx) = crossbeam_channel::bounded(1);
        let coordinator = Arc::new(CompletionCoordinator::new(
            "task".to_string(),
            "session".to_string(),
            wake_tx,
        ));
        let was_killed = Arc::new(AtomicBool::new(false));
        let exit_observed = Arc::new(AtomicBool::new(false));

        spawn_waiter(
            Box::new(InterruptedOnceChild { waits: 0 }),
            exit_file,
            was_killed,
            Arc::clone(&exit_observed),
            Arc::clone(&coordinator),
        )
        .unwrap();
        coordinator.signal_one_done();

        let started = Instant::now();
        while !exit_observed.load(Ordering::SeqCst) {
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(10));
        }
        wake_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(fs::read_to_string(exit_path).unwrap(), "0");
    }

    #[derive(Debug)]
    struct NeverExitsChild;

    #[cfg(unix)]
    #[derive(Debug)]
    struct MasterCloseCheckedChild(std::os::fd::RawFd);

    #[cfg(unix)]
    impl ChildKiller for MasterCloseCheckedChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }
        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(FakeKiller)
        }
    }

    #[cfg(unix)]
    impl Child for MasterCloseCheckedChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            // No allocation or other descriptor opens occur between closing the
            // master and this check, so its descriptor cannot have been reused.
            assert_eq!(
                unsafe { libc::fcntl(self.0, libc::F_GETFD) },
                -1,
                "PTY master must close before reaping a setup-failure child"
            );
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
            Ok(Some(ExitStatus::with_exit_code(0)))
        }
        fn wait(&mut self) -> io::Result<ExitStatus> {
            panic!("cleanup must use bounded polling")
        }
        fn process_id(&self) -> Option<u32> {
            None
        }
    }

    #[cfg(unix)]
    #[test]
    fn setup_cleanup_closes_master_before_reaping_child() {
        const CHILD: &str = "AFT_TEST_PTY_MASTER_CLOSE_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Isolate descriptor allocation from parallel tests: a closed
            // descriptor could otherwise be reused before the child checks it.
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "bash_background::pty_process::tests::setup_cleanup_closes_master_before_reaping_child", "--nocapture"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize::default())
            .unwrap();
        let fd = pair.master.as_raw_fd().unwrap();
        let guard = SpawnedPtyChild {
            master: Some(pair.master),
            child: Some(Box::new(MasterCloseCheckedChild(fd))),
        };
        drop(guard);
    }

    impl ChildKiller for NeverExitsChild {
        fn kill(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(FakeKiller)
        }
    }

    impl Child for NeverExitsChild {
        fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            Ok(None)
        }

        fn wait(&mut self) -> io::Result<ExitStatus> {
            panic!("setup cleanup must poll with try_wait, never block in wait");
        }

        fn process_id(&self) -> Option<u32> {
            None
        }

        #[cfg(windows)]
        fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
            None
        }
    }

    #[test]
    fn setup_cleanup_reap_gives_up_with_named_error_after_its_bound() {
        let mut child = NeverExitsChild;
        let started = Instant::now();
        let error = reap_killed_child(&mut child, Duration::from_millis(100))
            .expect_err("a child that never exits must not count as reaped");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "reap overran its bound: {:?}",
            started.elapsed()
        );
        assert!(
            error.contains("was not reaped within 100ms"),
            "unexpected error: {error}"
        );
    }

    /// Feed `chunks` to a fresh scanner and return how many chunks reported a
    /// detection. The needle appears exactly once across all chunks, so a
    /// correct scanner returns exactly 1 (detect once, never double-fire).
    fn scan_chunks(chunks: &[&[u8]]) -> usize {
        let mut scanner = DsrScanner::default();
        chunks.iter().filter(|chunk| scanner.scan(chunk)).count()
    }

    #[test]
    fn dsr_detected_within_single_read() {
        assert_eq!(scan_chunks(&[b"\x1b[6n"]), 1);
    }

    #[test]
    fn dsr_detected_two_read_splits() {
        assert_eq!(scan_chunks(&[b"\x1b[6", b"n"]), 1); // 3/1
        assert_eq!(scan_chunks(&[b"\x1b[", b"6n"]), 1); // 2/2
        assert_eq!(scan_chunks(&[b"\x1b", b"[6n"]), 1); // 1/3
    }

    #[test]
    fn dsr_detected_across_more_than_two_reads() {
        // The original carry-over (keep only the chunk's own last 3 bytes)
        // missed these because by the time `n` arrives the `\x1b` had aged out.
        assert_eq!(scan_chunks(&[b"\x1b", b"[", b"6", b"n"]), 1); // 1/1/1/1
        assert_eq!(scan_chunks(&[b"\x1b", b"[", b"6n"]), 1); // 1/1/2
        assert_eq!(scan_chunks(&[b"\x1b[", b"6", b"n"]), 1); // 2/1/1
                                                             // With unrelated leading noise that pushes the needle across reads.
        assert_eq!(scan_chunks(&[b"junk\x1b", b"[6", b"n more"]), 1);
    }

    #[test]
    fn dsr_detected_once_with_surrounding_output() {
        // Single sequence embedded in a larger single read fires exactly once.
        assert_eq!(scan_chunks(&[b"hello\x1b[6nworld"]), 1);
    }

    #[test]
    fn dsr_not_detected_no_match() {
        assert_eq!(scan_chunks(&[b"abc", b"def"]), 0);
        assert_eq!(scan_chunks(&[b"hello"]), 0);
        // Partial-but-never-completed sequence must not fire.
        assert_eq!(scan_chunks(&[b"\x1b[6", b"x"]), 0);
    }
}
#[cfg(unix)]
#[test]
fn lifecycle_pty_setup_failure_terminates_and_reaps_child() {
    use super::persistence::{create_task_layout, BgMode};
    let dir = tempfile::tempdir().unwrap();
    let layout = create_task_layout(dir.path(), "session", "bash-0000000000000901").unwrap();
    let mut handles = TaskIoHandles::create(&layout, BgMode::Pty, false).unwrap();
    let pid = Arc::new(std::sync::atomic::AtomicU32::new(0));
    SETUP_FAILURE
        .with(|failure| *failure.borrow_mut() = Some((dir.path().join("ready"), pid.clone())));
    let mut spawned_pgid = None;
    let result = try_spawn_pty(
        &SpawnPlan::Unsandboxed,
        "bash-0000000000000901",
        "session",
        std::ffi::OsStr::new("/bin/sh"),
        &["-c".into(), "trap '' HUP; : > ready; sleep 30".into()],
        &layout.paths,
        dir.path(),
        &HashMap::new(),
        24,
        80,
        crossbeam_channel::unbounded().0,
        &mut handles,
        &mut spawned_pgid,
        &mut None,
    );
    let pid = pid.load(Ordering::SeqCst);
    assert_ne!(pid, 0);
    assert_eq!(spawned_pgid, Some(pid as i32));
    let alive = super::process::is_process_alive(pid);
    if alive {
        super::process::terminate_pgid(pid as i32, None);
    }
    assert!(result.is_err());
    assert!(!alive, "fallible PTY setup abandoned a live child");
}

#[cfg(all(test, unix))]
fn approved_pty_worker_admission_failure(worker: super::worker_thread::Worker) {
    use super::persistence::{create_task_layout, BgMode};
    let dir = tempfile::tempdir().unwrap();
    let layout = create_task_layout(dir.path(), "session", "bash-0000000000000010").unwrap();
    let mut handles = TaskIoHandles::create(&layout, BgMode::Pty, false).unwrap();
    let pid = Arc::new(std::sync::atomic::AtomicU32::new(0));
    SETUP_OBSERVATION.with(|observation| {
        *observation.borrow_mut() = Some((dir.path().join("ready"), pid.clone()))
    });
    let mut spawned_pgid = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        super::worker_thread::with_refusal(worker, || {
            try_spawn_pty(
                &SpawnPlan::Unsandboxed,
                "bash-0000000000000010",
                "session",
                std::ffi::OsStr::new("/bin/sh"),
                &["-c".into(), "trap '' HUP; : > ready; sleep 30".into()],
                &layout.paths,
                dir.path(),
                &HashMap::new(),
                24,
                80,
                crossbeam_channel::unbounded().0,
                &mut handles,
                &mut spawned_pgid,
                &mut None,
            )
        })
    }));
    let pid = pid.load(Ordering::SeqCst);
    assert_ne!(pid, 0, "refusal did not reach a spawned child");
    let alive = super::process::is_process_alive(pid);
    if alive {
        super::process::terminate_pgid(pid as i32, None);
    }
    assert!(!alive, "worker refusal abandoned a live child");
    assert!(
        result.is_ok(),
        "worker admission panicked instead of returning an error"
    );
    let error = result
        .unwrap()
        .err()
        .expect("worker refusal must reject the spawn");
    assert!(
        error.contains("injected worker admission failure"),
        "admission failure lost its cause: {error}"
    );
}

#[cfg(all(test, unix))]
#[test]
fn approved_pty_reader_admission_failure_returns_error_and_reaps_child() {
    approved_pty_worker_admission_failure(super::worker_thread::Worker::PtyReader);
}

#[cfg(all(test, unix))]
#[test]
fn approved_pty_waiter_admission_failure_returns_error_and_reaps_child() {
    approved_pty_worker_admission_failure(super::worker_thread::Worker::PtyWaiter);
}
