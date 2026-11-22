use super::{
    emit_linux_warning, exec_target, landlock_backend, read_profile, seal_failure_fd,
    signal_scope_label, verify_worker_gh_fence, LaunchArgs, SandboxLaunchError,
};
use aft::config::SandboxMode;
use aft::sandbox_profile::{LinuxMountPolicy, SandboxProfile, WorkerGhFence};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Seek, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

mod mounts;
use mounts::{Mount, Snapshot};
mod handoff;

#[cfg(test)]
mod tests;

const BWRAP: &str = "/usr/bin/bwrap";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    Missing,
    UserNamespaceUnavailable,
}

impl Fallback {
    fn code(self) -> &'static str {
        match self {
            Self::Missing => "bwrap_missing",
            Self::UserNamespaceUnavailable => "bwrap_user_namespace_unavailable",
        }
    }
}

pub(super) fn launch(args: &LaunchArgs, profile: SandboxProfile) -> Result<(), SandboxLaunchError> {
    launch_with_probe(args, profile, probe)
}

fn launch_with_probe(
    args: &LaunchArgs,
    profile: SandboxProfile,
    probe: impl FnOnce() -> Result<Option<Fallback>, String>,
) -> Result<(), SandboxLaunchError> {
    if let Some(fd) = args.failure_fd {
        // Keep the engine's failure descriptor writable here. Bubblewrap and
        // the inner launcher use a separate descriptor to report setup status.
        seal_failure_fd(fd).map_err(SandboxLaunchError::unavailable)?;
    }
    if let Some(reason) = probe().map_err(SandboxLaunchError::unavailable)? {
        let mounts = profile.linux_mounts.as_ref().expect("local mount policy");
        for_blocked_mount_layer(&profile, mounts, reason);
        let profile = fallback_profile(profile)?;
        super::apply_sandbox(&profile)?;
        return exec_target(&args.command);
    }
    exec_bwrap(args, profile)
}

fn bwrap_command(path: &Path) -> Command {
    let mut command = Command::new(path);
    // Keep the network, PID namespace, devices and terminal the command had
    // before launch. Prevent the payload from creating a user namespace of its own.
    command.args(["--unshare-user", "--disable-userns", "--dev-bind", "/", "/"]);
    command
}

fn probe() -> Result<Option<Fallback>, String> {
    probe_at(Path::new(BWRAP))
}

fn probe_at(path: &Path) -> Result<Option<Fallback>, String> {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Some(Fallback::Missing))
        }
        Err(error) => return Err(format!("cannot inspect {BWRAP}: {error}")),
        Ok(_) => {}
    }
    let output = bwrap_command(path)
        .current_dir("/")
        .env("LC_ALL", "C")
        .args(["--", "/usr/bin/true"])
        .output()
        .map_err(|error| format!("cannot execute {BWRAP}: {error}"))?;
    if output.status.success() {
        return Ok(None);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() == Some(1)
        && stderr.starts_with("bwrap: No permissions to create a new namespace, likely because the kernel does not allow non-privileged user namespaces.")
    {
        return Ok(Some(Fallback::UserNamespaceUnavailable));
    }
    Err(format!(
        "bubblewrap namespace probe failed: {} ({})",
        stderr.trim_end(),
        output.status
    ))
}

fn fallback_profile(mut profile: SandboxProfile) -> Result<SandboxProfile, SandboxLaunchError> {
    if let Some(mounts) = profile.linux_mounts.take() {
        profile.write_deny.extend(mounts.fallback_write_deny);
    }
    if matches!(
        profile.mode,
        SandboxMode::Credentials | SandboxMode::Standard
    ) && profile.worker_gh_fence.is_some()
    {
        // Relaxed Landlock cannot hide the file the gh shim uses to tell worker
        // shells from head shells.
        profile.worker_gh_fence = Some(WorkerGhFence::Unenforced);
    }
    profile
        .canonicalize_for_launch()
        .map_err(|error| SandboxLaunchError::unavailable(error.to_string()))
}

fn for_blocked_mount_layer(profile: &SandboxProfile, policy: &LinuxMountPolicy, reason: Fallback) {
    let mut unenforced = vec![reason.code().to_string()];
    for path in policy.credential_masks.iter().chain(&policy.read_masks) {
        unenforced.push(format!("mask:{}", path.display()));
    }
    for path in policy.read_only.iter().chain(&policy.deferred_read_only) {
        unenforced.push(format!("read_only_bind:{}", path.display()));
    }
    for path in &profile.data_policy.deny {
        unenforced.push(format!("mask:{}", path.display()));
    }
    for path in &profile.socket_deny {
        unenforced.push(format!("socket_deny:{}", path.display()));
    }
    if profile.mode != SandboxMode::Strict && !profile.loopback_tcp_connect_deny.is_empty() {
        unenforced.push("sccache_server_unblockable_on_landlock".to_string());
    }
    for path in &policy.cargo_homes {
        unenforced.push(format!("mask:{}*", path.join("credentials").display()));
    }
    unenforced.sort_unstable();
    unenforced.dedup();
    eprintln!("sandbox-launch: profile={} unenforced=[{}] (bubblewrap unavailable; using direct Landlock)", profile.mode, unenforced.join(","));
}

#[derive(Deserialize, Serialize)]
struct InnerProfile {
    profile: SandboxProfile,
    unenforced: Vec<String>,
}

fn prepare_inner(mut profile: SandboxProfile, snapshot: &Snapshot) -> InnerProfile {
    let protected = |path: &Path| {
        path.ancestors().any(|ancestor| {
            snapshot
                .mounts
                .get(ancestor)
                .is_some_and(|mount| *mount != Mount::Guard)
        })
    };
    let mounts = profile.linux_mounts.take().expect("local mount policy");
    profile.write_deny_nested.retain(|path| !protected(path));
    profile.socket_deny.retain(|path| !protected(path));
    // The mount report names the prefix rather than describing Landlock's
    // inability to enforce it. Landlock itself does not use prefix rules.
    profile.deny_prefixes.clear();
    let mut unenforced = snapshot.unenforced.clone();
    if profile.mode == SandboxMode::Standard {
        for path in profile
            .data_policy
            .deny
            .iter()
            .filter(|path| !protected(path))
        {
            unenforced.push(format!(
                "private_store_mask_unenforced_on_bwrap:{}",
                path.display()
            ));
        }
        for path in mounts
            .deferred_read_only
            .iter()
            .filter(|path| !protected(path))
        {
            unenforced.push(format!(
                "nested_write_deny_unenforced_on_bwrap:{}",
                path.display()
            ));
        }
    }
    if profile.worker_gh_fence == Some(WorkerGhFence::Unenforced) {
        unenforced.push(format!(
            "operator gh fence: unenforced ({} profile)",
            profile.mode
        ));
    }
    unenforced.sort_unstable();
    unenforced.dedup();
    InnerProfile {
        profile,
        unenforced,
    }
}

fn exec_bwrap(args: &LaunchArgs, profile: SandboxProfile) -> Result<(), SandboxLaunchError> {
    let unavailable = |error: io::Error| SandboxLaunchError::unavailable(error.to_string());
    let snapshot = Snapshot::take(profile.linux_mounts.as_ref().expect("local mount policy"))
        .map_err(SandboxLaunchError::unavailable)?;
    let inner = prepare_inner(profile, &snapshot);
    let executable = std::env::current_exe().map_err(unavailable)?;
    let handoff = handoff::Handoff::new().map_err(unavailable)?;
    let mut profile_fd = memfd(c"aft-bwrap-profile").map_err(unavailable)?;
    serde_json::to_writer(&mut profile_fd, &inner)
        .map_err(|error| SandboxLaunchError::unavailable(error.to_string()))?;
    profile_fd.rewind().map_err(unavailable)?;

    let mut command = bwrap_command(Path::new(BWRAP));
    let (_mount_args, _empty_files) = append_mounts(&mut command, snapshot).map_err(unavailable)?;
    command
        .arg("--")
        .arg(executable)
        .args(["sandbox-launch", "--bwrap-inner", "--profile-fd"])
        .arg(profile_fd.as_raw_fd().to_string());
    if let Some(path) = &args.failure_marker {
        command.arg("--failure-marker").arg(path);
    }
    command
        .arg("--failure-fd")
        .arg(handoff::INNER_FD.to_string())
        .arg("--")
        .args(&args.command);
    let status = handoff.supervise(&mut command)?;
    handoff::exit_with_status(status)
}

fn append_mounts(command: &mut Command, snapshot: Snapshot) -> io::Result<(File, Vec<File>)> {
    // Each ro-bind-data operation consumes and closes its own source FD. The
    // kernel keeps the sources alive across exec without putting them on disk.
    let mut files = Vec::new();
    let mut argv = Vec::new();
    for (path, mount) in snapshot.mounts {
        let path = path.as_os_str().as_bytes();
        let mut append = |args: &[&[u8]]| {
            for arg in args {
                argv.extend_from_slice(arg);
                argv.push(0);
            }
        };
        match mount {
            Mount::Guard => append(&[b"--dev-bind", path, path]),
            Mount::ReadOnly => append(&[b"--ro-bind", path, path]),
            Mount::EmptyDir => {
                append(&[b"--perms", b"0555", b"--tmpfs", path, b"--remount-ro", path])
            }
            Mount::EmptyFile => {
                let file = memfd(c"aft-bwrap-mask")?;
                let fd = file.as_raw_fd().to_string();
                append(&[b"--perms", b"0444", b"--ro-bind-data", fd.as_bytes(), path]);
                files.push(file);
            }
        }
    }
    // The inventory can outgrow execve's argument-vector limit on large homes.
    let mut mount_args = memfd(c"aft-bwrap-args")?;
    mount_args.write_all(&argv)?;
    mount_args.rewind()?;
    command
        .arg("--args")
        .arg(mount_args.as_raw_fd().to_string());
    Ok((mount_args, files))
}

fn memfd(name: &std::ffi::CStr) -> io::Result<File> {
    // These descriptors must survive the immediately following bubblewrap exec.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(super) fn apply_inner(args: &LaunchArgs) -> Result<(), SandboxLaunchError> {
    let fd = args
        .profile_fd
        .ok_or_else(|| SandboxLaunchError::unavailable("--bwrap-inner needs --profile-fd"))?;
    let failure_fd = args
        .failure_fd
        .filter(|&failure_fd| failure_fd == handoff::INNER_FD && failure_fd != fd)
        .ok_or_else(|| {
            SandboxLaunchError::unavailable("--bwrap-inner needs its setup descriptor")
        })?;
    let InnerProfile {
        profile,
        unenforced,
    } = read_profile(fd).map_err(|error| SandboxLaunchError::unavailable(error.to_string()))?;
    let applied = landlock_backend::apply(&profile).map_err(|error| {
        SandboxLaunchError::unavailable(format!("failed to apply Landlock ruleset: {error}"))
    })?;
    verify_worker_gh_fence(&profile)?;
    let backend = if profile.mode == SandboxMode::Strict {
        emit_linux_warning(&profile, applied);
        "bwrap+landlock"
    } else {
        if profile.worker_jail {
            eprintln!(
                "sandbox-launch: signal_scope={}",
                signal_scope_label(applied.signal_scope_enforced)
            );
        }
        "bwrap"
    };
    eprintln!(
        "sandbox-launch: profile={} backend={backend} unenforced=[{}]",
        profile.mode,
        unenforced.join(",")
    );
    super::write_marker_fd(failure_fd, handoff::COMPLETE).map_err(|error| {
        SandboxLaunchError::unavailable(format!("cannot acknowledge bubblewrap setup: {error}"))
    })?;
    seal_failure_fd(failure_fd).map_err(SandboxLaunchError::unavailable)
}

pub(super) fn mark_inner_failure(fd: i32) -> io::Result<()> {
    super::write_marker_fd(fd, handoff::FAILED)
}
