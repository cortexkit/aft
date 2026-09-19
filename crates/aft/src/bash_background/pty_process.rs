use std::collections::HashMap;
use std::ffi::OsString;
#[cfg(any(windows, test))]
use std::fs;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use portable_pty::PtySize;

use crate::sandbox_spawn::SpawnPlan;

use super::persistence::{ExitMarker, TaskArtifact, TaskIoHandles, TaskPaths};
use super::pty_runtime::{CompletionCoordinator, PtyRuntime};

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
) -> Result<PtyRuntime, String> {
    #[cfg(unix)]
    {
        let _ = shell_kind;
        let args = if let Some(prepared) = spawn_plan.prepared_task() {
            vec![
                OsString::from("-c"),
                OsString::from(prepared.command_text()?),
            ]
        } else {
            vec![OsString::from("-c"), OsString::from(user_command)]
        };
        try_spawn_pty(
            spawn_plan,
            task_id,
            session_id,
            shell_path.as_os_str(),
            &args,
            paths,
            workdir,
            env,
            rows,
            cols,
            wake_tx,
            io_handles,
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
        let mut last_err = String::from("no Windows shell candidates available");

        for shell in candidates {
            let wrapper_body = shell.wrapper_script_bytes(user_command, &paths.exit);
            let wrapper_path = windows_wrapper_path(paths, &shell);
            if let Err(error) = fs::write(&wrapper_path, wrapper_body) {
                last_err = format!("write wrapper {wrapper_path:?}: {error}");
                continue;
            }

            let args: Vec<OsString> = shell
                .pty_wrapper_args(&wrapper_path)
                .into_iter()
                .map(OsString::from)
                .collect();

            match try_spawn_pty(
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
            ) {
                Ok(runtime) => return Ok(runtime),
                Err(error) => {
                    let msg = format!("{shell:?}: {error}");
                    if msg.contains("NotFound") || msg.contains("not recognized") {
                        last_err = msg;
                        continue;
                    }
                    return Err(msg);
                }
            }
        }

        Err(last_err)
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
) -> Result<PtyRuntime, String> {
    let (command, profile_handle) = crate::sandbox_spawn::pty_command_for_plan(
        spawn_plan,
        program,
        args,
        &paths.json,
        workdir,
        env,
    )?;
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| format!("open PTY failed: {error}"))?;
    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("spawn PTY command failed: {error}"))?;
    drop(profile_handle);
    let child_pid = child.process_id();
    let killer = child.clone_killer();
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("clone PTY reader failed: {error}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| format!("take PTY writer failed: {error}"))?;

    let reader_done = Arc::new(AtomicBool::new(false));
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
        Arc::clone(&coordinator),
        Some(Arc::clone(&writer)),
    );
    spawn_waiter(
        child,
        exit,
        Arc::clone(&was_killed),
        Arc::clone(&exit_observed),
        Arc::clone(&coordinator),
    );

    Ok(PtyRuntime {
        master: Some(pair.master),
        writer,
        killer,
        child_pid,
        reader_done,
        exit_observed,
        was_killed,
        coordinator,
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
    coordinator: Arc<CompletionCoordinator>,
    writer: Option<Arc<Mutex<Box<dyn Write + Send>>>>,
) {
    thread::spawn(move || {
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
        if let Err(error) = result {
            crate::slog_warn!(
                "PTY reader for {}:{} stopped with error: {error}",
                coordinator.session_id,
                coordinator.task_id
            );
        }
        reader_done.store(true, Ordering::SeqCst);
        coordinator.signal_one_done();
    });
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
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    mut exit_file: std::fs::File,
    was_killed: Arc<AtomicBool>,
    exit_observed: Arc<AtomicBool>,
    coordinator: Arc<CompletionCoordinator>,
) {
    thread::spawn(move || {
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
        exit_observed.store(true, Ordering::SeqCst);
        coordinator.signal_one_done();
    });
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
    file.sync_all()
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
        );
        coordinator.signal_one_done();

        let started = Instant::now();
        while !exit_observed.load(Ordering::SeqCst) {
            assert!(started.elapsed() < Duration::from_secs(2));
            std::thread::sleep(Duration::from_millis(10));
        }
        wake_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(fs::read_to_string(exit_path).unwrap(), "0");
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
