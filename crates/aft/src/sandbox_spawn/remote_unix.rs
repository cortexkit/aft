use super::{super as native, *};
use crate::{bash_background::output::FINAL_OUTPUT_CAP_BYTES, sandbox_profile::SandboxProfile};
use std::{
    ffi::OsString,
    io,
    os::{fd::AsRawFd, unix::net::UnixStream},
    process::{ExitStatus, Stdio},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};

static TOOL_DIRS: &[&str] = &[
    "/bin",
    "/usr/bin",
    "/lib",
    "/lib64",
    "/usr/lib",
    "/usr/lib64",
    "/usr/libexec",
    "/usr/share",
];

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(error) = native::remove_provider_temp_layout(&self.0) {
            log::warn!("remote shell scratch cleanup: {error}");
        }
    }
}

pub(crate) struct Start {
    launcher: PathBuf,
    scratch: Scratch,
    workdir: PathBuf,
    shell: PathBuf,
    command: String,
    profile: OsString,
    path: OsString,
}

pub(crate) fn prepare(
    ctx: &AppContext,
    folder: &Path,
    workdir: PathBuf,
    command: String,
) -> Result<Start, String> {
    let launcher = crate::pinned_executable::path().map_err(|error| error.to_string())?;
    prepare_with_launcher(ctx, folder, workdir, command, launcher)
}

pub(super) fn prepare_with_launcher(
    ctx: &AppContext,
    folder: &Path,
    workdir: PathBuf,
    command: String,
    launcher: PathBuf,
) -> Result<Start, String> {
    let _config = ctx.pin_config();
    let os_error = |error: io::Error| error.to_string();
    let home = crate::production_storage::account_home().map_err(os_error)?;
    let env_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or(home);
    let launcher = launcher.canonicalize().map_err(os_error)?;
    let scratch = Scratch(native::create_provider_temp_layout().map_err(os_error)?);
    let shell = Path::new("/bin/bash").canonicalize().map_err(os_error)?;
    let mut reads = vec![folder.to_path_buf(), scratch.0.clone(), shell.clone()];
    reads.extend(
        TOOL_DIRS
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.is_dir()),
    );
    reads.extend(
        [
            "/dev/null",
            "/dev/urandom",
            "/etc/ld.so.cache",
            "/etc/passwd",
            "/etc/group",
            "/etc/nsswitch.conf",
        ]
        .into_iter()
        .map(PathBuf::from)
        .filter(|path| path.is_file() || path.starts_with("/dev") && path.exists()),
    );
    let policy = ctx
        .access_policy("")
        .map_err(|error| error.data.to_string())?;
    let mut writable_deny = policy.absolute_floor.clone();
    let mut private = policy.private_stores;
    private.deny.extend([
        ctx.storage_dir(),
        env_home.join(".local/share/cortexkit"),
        env_home.join(".local/state/cortexkit"),
    ]);
    private.read_allow.clear();
    private.write_allow.clear();
    private.read_files.clear();
    private.read_metadata_allow.clear();
    private.session_task_read_dir = None;
    let mut path = vec![PathBuf::from("/usr/bin"), PathBuf::from("/bin")];
    if ctx.config().github.shim {
        let dir = crate::pinned_executable::shims_dir(&ctx.storage_dir())
            .canonicalize()
            .map_err(os_error)?;
        let binary = dir.join("gh").canonicalize().map_err(os_error)?;
        if binary != launcher {
            return Err("remote shell needs the shim from the running AFT build".into());
        }
        // Only the installed shim is an exception to the private data floor.
        private.read_allow.push(dir.clone());
        private.read_allow.push(binary.clone());
        writable_deny.extend([dir.clone(), binary.clone()]);
        reads.extend([dir.clone(), binary]);
        path.insert(0, dir);
    }
    let mut profile = SandboxProfile::build(
        vec![folder.to_path_buf()],
        writable_deny,
        Vec::new(),
        reads,
        policy.absolute_floor,
        Vec::new(),
        Vec::new(),
        scratch.0.clone(),
    )
    .map_err(|error| error.to_string())?;
    profile.data_policy = private;
    native::add_credential_floor(&mut profile, &env_home, &[folder.to_path_buf()])?;
    let profile = profile
        .canonicalize_for_launch()
        .map_err(|error| error.to_string())?;
    Ok(Start {
        launcher,
        scratch,
        workdir,
        shell,
        command,
        profile: serde_json::to_string(&profile)
            .map_err(|error| error.to_string())?
            .into(),
        path: std::env::join_paths(path).map_err(|error| error.to_string())?,
    })
}

pub(crate) struct Running {
    child: Child,
    scratch: Scratch,
    group: i32,
    failure: tokio::net::UnixStream,
}

impl Start {
    pub(crate) fn start(self) -> Result<Running, String> {
        let result = (|| -> io::Result<Running> {
            let (reader, writer) = UnixStream::pair()?;
            reader.set_nonblocking(true)?;
            let failure = tokio::net::UnixStream::from_std(reader)?;
            let fd = writer.as_raw_fd();
            let mut command = Command::new(&self.launcher);
            command
                .args(["sandbox-launch", "--remote-strict", "--profile-json"])
                .arg(self.profile)
                .args(["--failure-fd", "3", "--"])
                .arg(self.shell)
                .args(["--noprofile", "--norc", "-c"])
                .arg(self.command)
                .current_dir(self.workdir)
                .env_clear()
                .env("PATH", self.path)
                .env("HOME", &self.scratch.0)
                .env("TMPDIR", &self.scratch.0)
                .env("LC_ALL", "C")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            // SAFETY: this closure calls only async-signal-safe descriptor and process syscalls.
            unsafe {
                command.pre_exec(move || {
                    if libc::setsid() < 0
                        || libc::dup2(fd, 3) < 0
                        || libc::fcntl(3, libc::F_SETFD, 0) < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = command.spawn()?;
            let group = child.id().expect("newly spawned child has a PID") as i32;
            Ok(Running {
                child,
                scratch: self.scratch,
                group,
                failure,
            })
        })();
        result.map_err(|error| error.to_string())
    }
}

impl Running {
    fn kill(&self) {
        // SAFETY: a negative PID addresses only the child's session leader's group.
        unsafe { libc::kill(-self.group, libc::SIGKILL) };
    }

    pub(crate) async fn wait(
        mut self,
        timeout: Duration,
        cancel: CancellationToken,
    ) -> Result<Output, String> {
        let stdout = self.child.stdout.take().expect("piped stdout");
        let stderr = self.child.stderr.take().expect("piped stderr");
        let mut out = Captured::default();
        let mut err = Captured::default();
        let (status, timed_out) = {
            let drain =
                async { tokio::try_join!(out.drain(stdout), err.drain(stderr)).map(|_| ()) };
            tokio::pin!(drain);
            let deadline = tokio::time::Instant::now() + timeout;
            let mut drained = false;
            let (status, timed_out) = loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break (None, false),
                    _ = tokio::time::sleep_until(deadline) => break (None, true),
                    status = self.child.wait() => break (Some(status), false),
                    result = &mut drain, if !drained => {
                        result.map_err(|error| error.to_string())?;
                        drained = true;
                    }
                }
            };
            self.kill();
            let status = match status {
                Some(status) => status,
                None => self.child.wait().await,
            }
            .map_err(|error| error.to_string())?;
            if !drained {
                tokio::time::timeout(Duration::from_secs(2), &mut drain)
                    .await
                    .map_err(|_| "remote shell output descriptors did not close".to_owned())?
                    .map_err(|error| error.to_string())?;
            }
            (status, timed_out)
        };
        let until = tokio::time::Instant::now() + Duration::from_secs(2);
        while !session_is_clean(self.group).map_err(|error| error.to_string())? {
            if tokio::time::Instant::now() >= until {
                return Err("remote shell process group did not stop".into());
            }
            self.kill();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut marker = [0; 1];
        if tokio::time::timeout(Duration::from_secs(2), self.failure.read(&mut marker))
            .await
            .map_err(|_| "remote shell failure channel did not close".to_owned())?
            .map_err(|error| error.to_string())?
            != 0
        {
            return Err("the native launcher refused to apply remote confinement".into());
        }
        self.group = 0;
        Ok(self.output(status, timed_out, out, err))
    }

    fn output(
        &self,
        status: ExitStatus,
        timed_out: bool,
        stdout: Captured,
        stderr: Captured,
    ) -> Output {
        let _scratch = &self.scratch;
        use std::os::unix::process::ExitStatusExt;
        Output {
            text: crate::bash_background::buffer::combine_streams(&stdout.text(), &stderr.text()),
            code: status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)),
            timed_out,
        }
    }
}

fn session_is_clean(session: i32) -> io::Result<bool> {
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let stat = match std::fs::read(entry.path().join("stat")) {
            Ok(stat) => stat,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                continue
            }
            Err(error) => return Err(error),
        };
        // A process name may itself contain spaces or parentheses.
        let end = stat
            .iter()
            .rposition(|byte| *byte == b')')
            .ok_or_else(|| io::Error::other("invalid /proc process stat"))?;
        let mut fields = stat[end + 1..]
            .split(|byte| byte.is_ascii_whitespace())
            .filter(|field| !field.is_empty());
        let state = fields.next();
        let id = fields
            .nth(2)
            .and_then(|field| std::str::from_utf8(field).ok())
            .and_then(|field| field.parse::<i32>().ok())
            .ok_or_else(|| io::Error::other("invalid /proc session id"))?;
        // An exited child may wait for its new parent to reap it, but no
        // instructions from the command can run in a zombie.
        if id == session && !matches!(state, Some(b"Z" | b"X")) {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.group > 0 {
            self.kill();
        }
    }
}

#[derive(Default)]
struct Captured {
    bytes: Vec<u8>,
    omitted: usize,
}
impl Captured {
    async fn drain(&mut self, mut source: impl AsyncRead + Unpin) -> io::Result<()> {
        let mut chunk = [0; 4096];
        loop {
            let read = source.read(&mut chunk).await?;
            if read == 0 {
                return Ok(());
            }
            let retained = read.min(FINAL_OUTPUT_CAP_BYTES - self.bytes.len());
            self.bytes.extend_from_slice(&chunk[..retained]);
            self.omitted = self.omitted.saturating_add(read - retained);
        }
    }
    fn text(self) -> String {
        let text = String::from_utf8_lossy(&self.bytes);
        if self.omitted == 0 {
            text.into_owned()
        } else {
            format!("{text}\n[{} more output bytes omitted]", self.omitted)
        }
    }
}
