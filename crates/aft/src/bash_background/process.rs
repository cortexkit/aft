#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(unix)]
use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
/// Shared process-termination helpers for both foreground bash and background
/// bash tasks. Extracted to avoid duplication between `commands/bash.rs` and
/// `bash_background/registry.rs`.
///
/// Termination is graceful-first: SIGTERM + 2-second grace period, then
/// SIGKILL on Unix. On Windows, `taskkill /T /F` kills the entire process tree.
use std::process::Child;
#[cfg(windows)]
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::thread;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

pub const TERMINATE_GRACE: Duration = Duration::from_secs(2);
pub const LIVE_DESCENDANT_CAP: usize = 16;

/// Longest executable name kept for one process-group member, in characters.
/// The kernel's own limit is shorter (15 on Linux, 32 on macOS); this guards
/// the fallback paths.
pub const LIVE_DESCENDANT_NAME_CAP: usize = 64;

/// Appended to a member's shown command when it had arguments AFT left out.
pub const OMITTED_ARGUMENTS_MARKER: &str = "…";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LiveDescendant {
    pub pid: u32,
    pub comm: String,
    /// The first word of the process's first argument (normally the program
    /// path it was started as), or its command name when that argument cannot
    /// be read. Never any later argument.
    pub argv0: String,
    /// Name of the user that owns the process (its real user ID), or the
    /// numeric ID when the account has no name. Absent when it could not be
    /// read, and on records written before AFT recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// The process's executable name, followed by `…` when it was started
    /// with arguments. The arguments themselves are never kept: they can hold
    /// a credential (`--token "$DEPLOY_TOKEN"`, a value read from a file) that
    /// no pattern can recognise, and this field reaches the task record, the
    /// status reply and the completion notice. Absent on older records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// Why a task has no list of surviving process-group members. A list, even an
/// empty one, always means AFT checked; this says why it could not.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LiveDescendantsUnavailable {
    /// This platform has no process-group listing AFT can use (Windows, and
    /// Unix systems other than Linux and macOS).
    UnsupportedPlatform,
    /// The task has no recorded process group.
    NoProcessGroup,
    /// Listing the group's members failed, for example because `/proc`
    /// could not be read.
    ListingFailed,
    /// The group still has members, but the listing could not account for
    /// all of them: some were hidden or unreadable, for example because
    /// `/proc` is mounted with `hidepid`. Survivors may exist.
    ListingIncomplete,
    /// AFT did not check this task's group: a remote job, a pseudoterminal
    /// task that ended on its own, or a record written by an older AFT.
    NotChecked,
}

impl LiveDescendantsUnavailable {
    /// The reason to record when a task has no process group to list.
    pub fn without_group() -> Self {
        if cfg!(any(target_os = "linux", target_os = "macos")) {
            Self::NoProcessGroup
        } else {
            Self::UnsupportedPlatform
        }
    }
}

/// Enumerate surviving members of a task's process group, at most
/// [`LIVE_DESCENDANT_CAP`] of them plus the count left out. An empty list means
/// the group was checked and has no live member; an error says why it could
/// not be checked.
pub fn live_process_group_members(
    pgid: i32,
) -> Result<(Vec<LiveDescendant>, usize), LiveDescendantsUnavailable> {
    #[cfg(test)]
    if test_hooks::listing_fails(pgid) {
        return Err(LiveDescendantsUnavailable::ListingFailed);
    }
    #[cfg(target_os = "linux")]
    return linux_process_group_members(pgid);
    #[cfg(target_os = "macos")]
    return macos_process_group_members(pgid);
    #[cfg(windows)]
    {
        let _ = pgid;
        return Err(LiveDescendantsUnavailable::UnsupportedPlatform);
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
        let _ = pgid;
        return Err(LiveDescendantsUnavailable::UnsupportedPlatform);
    }
}

/// Fault injection for tests, keyed by process group so parallel tests do not
/// collide.
#[cfg(test)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) mod test_hooks {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};

    static LISTING_FAILS: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    static SIGKILL_DENIED: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    static MEMBERS_INVISIBLE: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    static MEMBERS_UNREADABLE: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();

    fn insert(set: &OnceLock<Mutex<HashSet<i32>>>, pgid: i32) {
        set.get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(pgid);
    }

    fn contains(set: &OnceLock<Mutex<HashSet<i32>>>, pgid: i32) -> bool {
        set.get()
            .is_some_and(|groups| groups.lock().unwrap().contains(&pgid))
    }

    /// Leave this group's members out of the process listing while they stay
    /// alive, as `/proc` mounted with `hidepid=2` does for another user's
    /// processes.
    pub(crate) fn hide_members(pgid: i32) {
        insert(&MEMBERS_INVISIBLE, pgid);
    }

    pub(super) fn members_invisible(pgid: i32) -> bool {
        contains(&MEMBERS_INVISIBLE, pgid)
    }

    /// Make this group's members' `/proc/<pid>/stat` unreadable, as `/proc`
    /// mounted with `hidepid=1` does for another user's processes.
    pub(crate) fn make_members_unreadable(pgid: i32) {
        insert(&MEMBERS_UNREADABLE, pgid);
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) fn members_unreadable(pgid: i32) -> bool {
        contains(&MEMBERS_UNREADABLE, pgid)
    }

    /// Make every listing of this group fail, as an unreadable `/proc` would.
    pub(crate) fn fail_listing(pgid: i32) {
        LISTING_FAILS
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(pgid);
    }

    pub(super) fn listing_fails(pgid: i32) -> bool {
        LISTING_FAILS
            .get()
            .is_some_and(|groups| groups.lock().unwrap().contains(&pgid))
    }

    /// Make the group SIGKILL reach no member of this group, as it does for
    /// members owned by another user (the kernel answers EPERM for them).
    /// Members that ignore SIGTERM then survive the kill.
    pub(crate) fn deny_sigkill(pgid: i32) {
        SIGKILL_DENIED
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .insert(pgid);
    }

    #[cfg(unix)]
    pub(super) fn sigkill_denied(pgid: i32) -> bool {
        SIGKILL_DENIED
            .get()
            .is_some_and(|groups| groups.lock().unwrap().contains(&pgid))
    }
}

/// Cut a name to [`LIVE_DESCENDANT_NAME_CAP`] characters and mask anything in
/// it that looks like a credential.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn safe_name(name: &str) -> String {
    let name = name.trim();
    let name = if name.chars().count() <= LIVE_DESCENDANT_NAME_CAP {
        name.to_string()
    } else {
        let mut cut = name
            .chars()
            .take(LIVE_DESCENDANT_NAME_CAP - 1)
            .collect::<String>();
        cut.push('…');
        cut
    };
    crate::log_redact::aft_redactor(&name).into_owned()
}

/// The command shown for a group member: its executable name, plus
/// [`OMITTED_ARGUMENTS_MARKER`] when it had arguments. See
/// [`LiveDescendant::command`] for why the arguments are left out.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn shown_command(name: &str, has_arguments: bool) -> Option<String> {
    let name = safe_name(name);
    if name.is_empty() {
        return None;
    }
    Some(if has_arguments {
        format!("{name} {OMITTED_ARGUMENTS_MARKER}")
    } else {
        name
    })
}

/// Whether a process may have had arguments beyond its program name. An empty
/// argument list means they could not be read (macOS hides another user's), so
/// it counts as "may have".
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn arguments_may_be_present(args: &[String]) -> bool {
    args.len() != 1
}

/// The first word of a process's first argument, or its command name when that
/// argument cannot be read. A process can rewrite its
/// arguments into one title string (`setproctitle`), so everything after the
/// first whitespace may be argument text and is dropped.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn safe_argv0(first_argument: Option<&String>, comm: &str) -> String {
    let word = first_argument
        .and_then(|argument| argument.split_whitespace().next())
        .unwrap_or(comm);
    safe_name(word)
}

/// The account name for a user ID, or the ID itself when it has no name.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn user_name(uid: u32) -> String {
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let mut buffer = vec![0 as libc::c_char; 4096];
    // SAFETY: entry, buffer and result outlive the call, and the buffer
    // length passed is the buffer's real length.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut entry,
            buffer.as_mut_ptr(),
            buffer.len(),
            &mut result,
        )
    };
    if status == 0 && !result.is_null() && !entry.pw_name.is_null() {
        // SAFETY: a successful lookup points pw_name at a NUL-terminated
        // string inside `buffer`, which is still alive.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.pw_name) }.to_string_lossy();
        if !name.is_empty() {
            return name.into_owned();
        }
    }
    uid.to_string()
}

#[cfg(target_os = "linux")]
pub(crate) fn systemd_scope_argv(
    systemd_run: &Path,
    executable: &Path,
    args: &[OsString],
) -> (PathBuf, Vec<OsString>) {
    let mut wrapped = ["--user", "--scope", "--collect", "--quiet"]
        .into_iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    wrapped.push(executable.as_os_str().to_os_string());
    wrapped.extend_from_slice(args);
    (systemd_run.to_path_buf(), wrapped)
}

#[cfg(target_os = "linux")]
fn select_systemd_scope_launcher_with(
    find: impl FnOnce() -> Option<PathBuf>,
    probe: impl FnOnce(&Path) -> bool,
) -> Option<PathBuf> {
    let launcher = find()?;
    probe(&launcher).then_some(launcher)
}

#[cfg(target_os = "linux")]
pub(crate) fn select_systemd_scope_launcher() -> Option<PathBuf> {
    select_systemd_scope_launcher_with(
        || which::which("systemd-run").ok(),
        |launcher| probe_systemd_scope(launcher, Duration::from_secs(1)),
    )
}

#[cfg(target_os = "linux")]
fn probe_systemd_scope(launcher: &Path, limit: Duration) -> bool {
    let mut command = std::process::Command::new(launcher);
    command
        .args(["--user", "--scope", "--collect", "--quiet", "/bin/true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    start_new_session(&mut command);
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if Instant::now() < deadline => return status.success(),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            _ => {
                // The availability probe must not retain a launch indefinitely.
                // Kill its session as well as the client, then reap the client.
                unsafe {
                    libc::killpg(child.id() as i32, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Describe at most [`LIVE_DESCENDANT_CAP`] group members, lowest pid first,
/// and count the rest without describing them. Listing a member is cheap (its
/// pid and group); describing it reads its command line and owner, so that work
/// stops at the cap however large the group is.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn bounded_members<C>(
    mut candidates: Vec<(u32, C)>,
    mut describe: impl FnMut(u32, C) -> Option<LiveDescendant>,
) -> (Vec<LiveDescendant>, usize) {
    candidates.sort_by_key(|(pid, _)| *pid);
    let mut rest = candidates.into_iter();
    let members = rest
        .by_ref()
        .filter_map(|(pid, candidate)| describe(pid, candidate))
        .take(LIVE_DESCENDANT_CAP)
        .collect();
    (members, rest.count())
}

#[cfg(target_os = "linux")]
fn linux_process_group_members(
    pgid: i32,
) -> Result<(Vec<LiveDescendant>, usize), LiveDescendantsUnavailable> {
    // An empty group needs no /proc walk. EPERM still means the group exists
    // and must be enumerated, including reparented descendants.
    if pgid > 0 && process_group_gone(pgid) {
        return Ok((Vec::new(), 0));
    }
    #[cfg(test)]
    PROC_SCAN_COUNT.with(|count| count.set(count.get() + 1));
    // A group that exists but cannot be listed is not an empty group.
    let entries =
        std::fs::read_dir("/proc").map_err(|_| LiveDescendantsUnavailable::ListingFailed)?;
    let mut candidates = Vec::new();
    let mut zombies = 0_usize;
    let mut unreadable = 0_usize;
    for pid in entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
    {
        match linux_proc_entry(pid, pgid) {
            ProcEntry::Member(comm) => candidates.push((pid, comm)),
            ProcEntry::Zombie => zombies += 1,
            ProcEntry::Unreadable => unreadable += 1,
            ProcEntry::Elsewhere => {}
        }
    }
    // An unreadable entry may be a member of this group. With none of them,
    // a group the probe found alive must show at least one member or zombie.
    if unreadable > 0 || (candidates.is_empty() && zombies == 0) {
        return incomplete_unless_gone(pgid);
    }
    Ok(bounded_members(candidates, |pid, comm| {
        Some(linux_describe_member(pid, comm))
    }))
}

/// The verdict for a listing that could not account for the whole group: a
/// clean empty list if the group has since exited, otherwise
/// [`LiveDescendantsUnavailable::ListingIncomplete`]. Never an empty list for a
/// group that is still alive, which would read as a clean kill.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn incomplete_unless_gone(
    pgid: i32,
) -> Result<(Vec<LiveDescendant>, usize), LiveDescendantsUnavailable> {
    if process_group_gone(pgid) {
        Ok((Vec::new(), 0))
    } else {
        Err(LiveDescendantsUnavailable::ListingIncomplete)
    }
}

/// Whether the group has no process left, zombies included. Signal 0 that
/// succeeds, or fails with EPERM (a member AFT may not signal), means it
/// still exists.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn process_group_gone(pgid: i32) -> bool {
    (unsafe { libc::kill(-pgid, 0) } == -1)
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(all(test, target_os = "linux"))]
thread_local! { static PROC_SCAN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

/// What one `/proc/<pid>` entry says about a process group.
#[cfg(target_os = "linux")]
enum ProcEntry {
    /// A live member, with its command name.
    Member(String),
    /// A member that has exited but is not yet reaped.
    Zombie,
    /// In another group, or gone since the directory was listed.
    Elsewhere,
    /// Its `stat` could not be read, so it may or may not be a member.
    Unreadable,
}

#[cfg(target_os = "linux")]
fn linux_proc_entry(pid: u32, expected_pgid: i32) -> ProcEntry {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        // The process exited after its directory was listed.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return ProcEntry::Elsewhere;
        }
        Err(_) => return ProcEntry::Unreadable,
    };
    let Some((comm, state, pgrp)) = parse_proc_stat(&stat) else {
        return ProcEntry::Unreadable;
    };
    if pgrp != expected_pgid {
        return ProcEntry::Elsewhere;
    }
    #[cfg(test)]
    if test_hooks::members_invisible(pgrp) {
        return ProcEntry::Elsewhere;
    }
    #[cfg(test)]
    if test_hooks::members_unreadable(pgrp) {
        return ProcEntry::Unreadable;
    }
    if state == "Z" {
        ProcEntry::Zombie
    } else {
        ProcEntry::Member(comm)
    }
}

/// The command name, state and process group from `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn parse_proc_stat(stat: &str) -> Option<(String, &str, i32)> {
    let open = stat.find('(')?;
    let close = stat.rfind(") ")?;
    let comm = stat.get(open + 1..close)?.to_string();
    let mut fields = stat.get(close + 2..)?.split_whitespace();
    let state = fields.next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse::<i32>().ok()?;
    Some((comm, state, pgrp))
}

#[cfg(target_os = "linux")]
fn linux_describe_member(pid: u32, comm: String) -> LiveDescendant {
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let mut args = cmdline
        .split(|byte| *byte == 0)
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .collect::<Vec<_>>();
    // The command line ends with a NUL, which leaves one empty piece.
    if args.last().is_some_and(String::is_empty) {
        args.pop();
    }
    let argv0 = safe_argv0(args.first().filter(|value| !value.is_empty()), &comm);
    // /proc/<pid>/status is normally readable for every user's processes; its
    // Uid line lists the real, effective, saved and filesystem IDs. When it
    // cannot be read the owner is left unknown.
    let user = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))?
                .split_whitespace()
                .next()?
                .parse::<u32>()
                .ok()
        })
        .map(user_name);
    LiveDescendant {
        pid,
        command: shown_command(&comm, arguments_may_be_present(&args)),
        comm: safe_name(&comm),
        argv0,
        user,
    }
}

#[cfg(target_os = "macos")]
fn macos_process_group_members(
    pgid: i32,
) -> Result<(Vec<LiveDescendant>, usize), LiveDescendantsUnavailable> {
    use std::ffi::{c_char, c_int, c_void};

    #[link(name = "proc")]
    extern "C" {
        fn proc_listpgrppids(pgrpid: u32, buffer: *mut c_void, buffersize: c_int) -> c_int;
        fn proc_name(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    }

    if pgid <= 0 {
        return Ok((Vec::new(), 0));
    }
    let mut pids = vec![0_u32; 16_384];
    let count = unsafe {
        proc_listpgrppids(
            pgid as u32,
            pids.as_mut_ptr().cast(),
            i32::try_from(pids.len() * std::mem::size_of::<u32>()).unwrap_or(i32::MAX),
        )
    };
    if count < 0 {
        return Err(LiveDescendantsUnavailable::ListingFailed);
    }
    pids.truncate(count as usize);
    #[cfg(test)]
    if test_hooks::members_invisible(pgid) {
        pids.clear();
    }
    // kern.proc.pid answers for every user's processes. proc_name does not:
    // it fails with EPERM for another user's process, and dropping the member
    // on that failure hid exactly the processes a kill cannot reach. Only a
    // process that is gone, a zombie, or in another group is left out.
    let mut zombies = 0_usize;
    let candidates: Vec<_> = pids
        .into_iter()
        .filter(|pid| *pid != 0)
        .filter_map(|pid| {
            let info = macos_kinfo(pid);
            match &info {
                Some(info) if info.pgid != pgid => None,
                Some(info) if info.zombie => {
                    zombies += 1;
                    None
                }
                None if !macos_pid_exists(pid) => None,
                _ => Some((pid, info)),
            }
        })
        .collect();
    // A group the probe finds alive must show at least one member or zombie.
    if candidates.is_empty() && zombies == 0 {
        return incomplete_unless_gone(pgid);
    }
    Ok(bounded_members(candidates, |pid, info| {
        let mut name = [0 as c_char; 1024];
        let name_len =
            unsafe { proc_name(pid as c_int, name.as_mut_ptr().cast(), name.len() as u32) };
        let comm = if name_len > 0 {
            unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        } else {
            info.as_ref()
                .map(|info| info.comm.clone())
                .unwrap_or_else(|| "?".to_string())
        };
        let args = macos_argv(pid).unwrap_or_default();
        let argv0 = safe_argv0(args.first().filter(|value| !value.is_empty()), &comm);
        Some(LiveDescendant {
            pid,
            command: shown_command(&comm, arguments_may_be_present(&args)),
            comm: safe_name(&comm),
            argv0,
            user: info.map(|info| user_name(info.ruid)),
        })
    }))
}

/// The fields AFT reads from a macOS `struct kinfo_proc`.
#[cfg(target_os = "macos")]
struct MacosKinfo {
    comm: String,
    ruid: u32,
    pgid: i32,
    zombie: bool,
}

/// Read `kern.proc.pid.<pid>`. `None` when the process no longer exists or
/// the answer does not have the expected layout.
///
/// The `libc` crate does not declare `struct kinfo_proc` for Apple targets, so
/// the fields are read at their offsets in `<sys/sysctl.h>`. The layout is the
/// same on arm64 and x86_64; a size check rejects any other, and a test checks
/// the offsets against the test process itself.
#[cfg(target_os = "macos")]
fn macos_kinfo(pid: u32) -> Option<MacosKinfo> {
    const KINFO_PROC_SIZE: usize = 648;
    // kp_proc.p_stat (char); SZOMB marks a zombie.
    const P_STAT_OFFSET: usize = 36;
    const SZOMB: u8 = 5;
    // kp_proc.p_comm[MAXCOMLEN + 1]
    const P_COMM_OFFSET: usize = 243;
    const P_COMM_LEN: usize = 17;
    // kp_eproc.e_pcred.p_ruid
    const P_RUID_OFFSET: usize = 392;
    // kp_eproc.e_pgid
    const E_PGID_OFFSET: usize = 564;

    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROC,
        libc::KERN_PROC_PID,
        i32::try_from(pid).ok()?,
    ];
    let mut bytes = [0_u8; KINFO_PROC_SIZE];
    let mut size = bytes.len();
    // SAFETY: `size` is the buffer's real length, and sysctl writes at most
    // that many bytes.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || size != KINFO_PROC_SIZE {
        return None;
    }
    let read_u32 = |offset: usize| {
        u32::from_ne_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
    };
    let comm_bytes = &bytes[P_COMM_OFFSET..P_COMM_OFFSET + P_COMM_LEN];
    let comm_len = comm_bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(P_COMM_LEN);
    Some(MacosKinfo {
        comm: String::from_utf8_lossy(&comm_bytes[..comm_len]).into_owned(),
        ruid: read_u32(P_RUID_OFFSET),
        pgid: read_u32(E_PGID_OFFSET) as i32,
        zombie: bytes[P_STAT_OFFSET] == SZOMB,
    })
}

#[cfg(target_os = "macos")]
fn macos_argv(pid: u32) -> Option<Vec<String>> {
    const CTL_KERN: libc::c_int = 1;
    const KERN_PROCARGS2: libc::c_int = 49;
    // Two are enough to tell whether there are arguments beyond the program
    // name; their text is never kept.
    const MAX_ARGS: usize = 2;
    let mut mib = [CTL_KERN, KERN_PROCARGS2, i32::try_from(pid).ok()?];
    let mut size = 0_usize;
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
        || size <= std::mem::size_of::<libc::c_int>()
    {
        return None;
    }
    let mut bytes = vec![0_u8; size];
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            bytes.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    bytes.truncate(size);
    // Layout: argc (int), the executable path, NUL padding, then argc
    // NUL-terminated arguments followed by the environment.
    let int_size = std::mem::size_of::<libc::c_int>();
    let argc = libc::c_int::from_ne_bytes(bytes.get(..int_size)?.try_into().ok()?);
    let mut cursor = int_size;
    cursor += bytes.get(cursor..)?.iter().position(|byte| *byte == 0)?;
    while bytes.get(cursor).is_some_and(|byte| *byte == 0) {
        cursor += 1;
    }
    let mut args = Vec::new();
    while args.len() < usize::try_from(argc).unwrap_or(0).min(MAX_ARGS) && cursor < bytes.len() {
        let end = cursor
            + bytes[cursor..]
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(bytes.len() - cursor);
        args.push(String::from_utf8_lossy(&bytes[cursor..end]).into_owned());
        cursor = end + 1;
    }
    (!args.is_empty()).then_some(args)
}

/// Whether a process exists, including one owned by another user.
#[cfg(target_os = "macos")]
fn macos_pid_exists(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    (unsafe { libc::kill(pid, 0) } == 0)
        || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The Unix payload wrapper runs the user's command in the same shell that
/// owns the pipeline. Bash exposes per-segment statuses as `PIPESTATUS`, while
/// zsh exposes them as `pipestatus`; plain POSIX sh has neither and therefore
/// receives the original command without a capture suffix. The status stream
/// is published in the task's already-writable io directory only after the
/// pipeline finishes. Temporarily closing an inherited capture descriptor is
/// insufficient: some shells pass their saved redirection copy to pipeline
/// children. Exit/failure markers belong only to the outer wrapper. Windows
/// shells have no portable per-segment status equivalent.
#[cfg(unix)]
// POSIX command exits cannot produce this signed marker. Windows uses the full
// 32-bit exit range, so neither this gate nor its marker interpretation is used there.
pub(crate) const BOOTSTRAP_ABORTED_MARKER: i32 = i32::MIN;

#[cfg(unix)]
pub(crate) const PAYLOAD_WRAPPER: &[u8] = br#"#!/bin/sh
shell=$1
command=$2
exit_fd=$3
pipeline_status_path=$4
pipeline_shell=$5
if [ "${6-}" = "wait-for-identity" ]; then
  if ! IFS= read -r __aft_start || [ "$__aft_start" != "run" ]; then
    printf '%s' '-2147483648' >&"$exit_fd"
    exit 125
  fi
  exec </dev/null
fi
case "$pipeline_status_path:$pipeline_shell" in
  ?*:bash)
    "$shell" -c "__aft_pipeline_status=\$1
shift
$command
__aft_code=\$? __aft_ps=(\"\${PIPESTATUS[@]}\")
__aft_status_tmp=\"\${__aft_pipeline_status}.tmp.\$\$\"
(umask 077; set -C; printf '%s\\n' \"\${__aft_ps[@]}\" > \"\$__aft_status_tmp\") 2>/dev/null &&
  mv -f -- \"\$__aft_status_tmp\" \"\$__aft_pipeline_status\" 2>/dev/null ||
  rm -f -- \"\$__aft_status_tmp\" 2>/dev/null
exit \"\$__aft_code\"" "$shell" "$pipeline_status_path" 3>&- 4>&-
    ;;
  ?*:zsh)
    "$shell" -c "__aft_pipeline_status=\$1
shift
$command
__aft_code=\$? __aft_ps=(\"\${pipestatus[@]}\")
__aft_status_tmp=\"\${__aft_pipeline_status}.tmp.\$\$\"
(umask 077; set -C; printf '%s\\n' \"\${__aft_ps[@]}\" > \"\$__aft_status_tmp\") 2>/dev/null &&
  mv -f -- \"\$__aft_status_tmp\" \"\$__aft_pipeline_status\" 2>/dev/null ||
  rm -f -- \"\$__aft_status_tmp\" 2>/dev/null
exit \"\$__aft_code\"" "$shell" "$pipeline_status_path" 3>&- 4>&-
    ;;
  *)
    # POSIX sh has no per-segment pipeline status array; preserve the original
    # invocation instead of changing its semantics with a best-effort guess.
    "$shell" -c "$command" 3>&- 4>&-
    ;;
esac
code=$?
printf "%s" "$code" >&"$exit_fd"
exit "$code"
"#;

#[cfg(unix)]
pub(crate) fn pipeline_shell_kind(shell: &Path) -> Option<&'static str> {
    match shell.file_name().and_then(|name| name.to_str()) {
        Some("bash") => Some("bash"),
        Some("zsh") => Some("zsh"),
        _ => None,
    }
}

/// Start a non-PTY child in a new session before exec. The bridge can inherit a
/// harness's controlling terminal even though the child uses pipe-backed stdio;
/// an interactive shell could otherwise perform job control against that terminal.
#[cfg(unix)]
pub(crate) fn start_new_session(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(unix)]
pub fn terminate_process(child: &mut Child) {
    let pgid = child.id() as i32;
    terminate_pgid(pgid, Some(child));
}

#[cfg(unix)]
pub fn terminate_pgid(pgid: i32, mut child: Option<&mut Child>) {
    terminate_pgid_with(pgid, || {
        child
            .as_deref_mut()
            .is_some_and(|child| matches!(child.try_wait(), Ok(Some(_))))
    });
}

#[cfg(unix)]
pub(crate) fn terminate_pgid_with(pgid: i32, mut leader_exited: impl FnMut() -> bool) {
    unsafe {
        libc::killpg(pgid, libc::SIGTERM);
    }
    let grace_started = Instant::now();
    while grace_started.elapsed() < TERMINATE_GRACE {
        if leader_exited() {
            // The direct child (process-group leader) exited. Stop waiting,
            // but still SIGKILL the whole group below — a descendant that
            // ignored SIGTERM can outlive the leader (the wrapper-shell /
            // CLI-spawns-child orphan class). killpg on an already-empty
            // group is a harmless ESRCH.
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    #[cfg(test)]
    if test_hooks::sigkill_denied(pgid) {
        return;
    }
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
}

#[cfg(windows)]
pub fn terminate_process(child: &mut Child) {
    terminate_pid(child.id());
}

#[cfg(windows)]
pub fn terminate_pid(pid: u32) {
    let pid = pid.to_string();
    let _ = Command::new("taskkill")
        .args(["/PID", &pid, "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    (unsafe { libc::kill(pid, 0) == 0 })
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(windows)]
pub fn is_process_alive(pid: u32) -> bool {
    use std::ffi::c_void;

    type Handle = *mut c_void;

    extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> Handle;
        fn GetExitCodeProcess(hProcess: Handle, lpExitCode: *mut u32) -> i32;
        fn CloseHandle(hObject: Handle) -> i32;
    }

    const FALSE: i32 = 0;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 0x103;

    if pid == 0 {
        return false;
    }

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code) != 0 && exit_code == STILL_ACTIVE;
        let _ = CloseHandle(handle);
        ok
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn documented_termination_grace_matches_policy() {
        let comment = include_str!("process.rs")
            .lines()
            .find(|line| line.starts_with("/// Termination is graceful-first:"))
            .unwrap();
        assert!(comment.contains(&format!(
            "{}-second grace period",
            super::TERMINATE_GRACE.as_secs()
        )));
    }
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_scope_argv_wraps_the_tool_shell() {
        let args = vec![OsString::from("-c"), OsString::from("printf ready")];
        let (program, wrapped) = systemd_scope_argv(
            Path::new("/usr/bin/systemd-run"),
            Path::new("/bin/sh"),
            &args,
        );
        assert_eq!(program, PathBuf::from("/usr/bin/systemd-run"));
        assert_eq!(
            wrapped,
            vec![
                "--user",
                "--scope",
                "--collect",
                "--quiet",
                "/bin/sh",
                "-c",
                "printf ready",
            ]
            .into_iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_scope_falls_back_when_user_manager_is_unreachable() {
        let probes = std::cell::Cell::new(0);
        let selected = select_systemd_scope_launcher_with(
            || Some(PathBuf::from("/usr/bin/systemd-run")),
            |_| {
                probes.set(probes.get() + 1);
                false
            },
        );
        assert_eq!(selected, None);
        assert_eq!(probes.get(), 1);
    }

    #[test]
    fn is_process_alive_returns_true_for_self() {
        assert!(is_process_alive(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn payload_wrapper_captures_bash_and_zsh_pipeline_statuses() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::MetadataExt;
        use std::process::Command;

        let mut shells = vec![("/bin/bash", "bash")];
        if Path::new("/bin/zsh").is_file() {
            shells.push(("/bin/zsh", "zsh"));
        }

        for (shell, kind) in shells {
            let temp = tempfile::tempdir().expect("create wrapper test directory");
            let exit_path = temp.path().join("exit");
            let status_path = temp.path().join("pipeline ' status");
            let exit_file = std::fs::File::create(&exit_path).expect("create exit marker");
            let status_file = std::fs::File::create(&status_path).expect("create status file");
            let original_status_inode = status_file.metadata().unwrap().ino();
            let failure_file = std::fs::File::create(temp.path().join("failure")).unwrap();
            let mut command = Command::new("/bin/sh");
            command.args([
                "-c",
                std::str::from_utf8(PAYLOAD_WRAPPER).expect("wrapper is UTF-8"),
                "aft-payload-wrapper",
                shell,
                "false | true",
                "3",
                status_path.to_str().unwrap(),
                kind,
            ]);
            crate::sandbox_spawn::apply_marker_fd_allowlist(
                &mut command,
                exit_file.as_raw_fd(),
                failure_file.as_raw_fd(),
            )
            .unwrap();
            let result = command.status().expect("run payload wrapper");
            drop(exit_file);
            drop(status_file);
            assert!(result.success(), "wrapper failed for {kind}");
            assert_eq!(std::fs::read_to_string(&status_path).unwrap(), "1\n0\n");
            assert_ne!(
                std::fs::metadata(&status_path).unwrap().ino(),
                original_status_inode,
                "status must be published through an atomic rename"
            );
            assert_eq!(std::fs::read_to_string(&exit_path).unwrap(), "0");
        }
    }

    #[cfg(unix)]
    #[test]
    fn payload_bootstrap_eof_and_wrong_release_never_execute_command() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::process::{Command, Stdio};
        for release in [None, Some(b"not-run\n".as_slice())] {
            let temp = tempfile::tempdir().unwrap();
            let exit_path = temp.path().join("exit");
            let exit = std::fs::File::create(&exit_path).unwrap();
            let failure = std::fs::File::create(temp.path().join("failure")).unwrap();
            let marker = temp.path().join("must-not-run");
            let payload = format!("printf ran > '{}'", marker.display());
            let mut command = Command::new("/bin/sh");
            command
                .args([
                    "-c",
                    std::str::from_utf8(PAYLOAD_WRAPPER).unwrap(),
                    "aft-payload-wrapper",
                    "/bin/sh",
                    &payload,
                    "3",
                    "",
                    "",
                    "wait-for-identity",
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            crate::sandbox_spawn::apply_marker_fd_allowlist(
                &mut command,
                exit.as_raw_fd(),
                failure.as_raw_fd(),
            )
            .unwrap();
            let mut child = command.spawn().unwrap();
            let mut input = child.stdin.take().unwrap();
            if let Some(release) = release {
                input.write_all(release).unwrap();
            }
            drop(input);
            let output = child.wait_with_output().unwrap();
            assert_eq!(output.status.code(), Some(125));
            assert!(
                !marker.exists(),
                "bootstrap must not run user code without release"
            );
            assert_eq!(std::fs::read_to_string(exit_path).unwrap(), "-2147483648");
        }
    }

    #[cfg(unix)]
    #[test]
    fn payload_wrapper_keeps_control_fds_out_of_user_commands() {
        use std::os::fd::AsRawFd;
        use std::process::Command;

        let mut shells = vec![("/bin/sh", ""), ("/bin/bash", "bash")];
        if Path::new("/bin/zsh").is_file() {
            shells.push(("/bin/zsh", "zsh"));
        }
        for (shell, kind) in shells {
            for pipeline in [false, true] {
                if pipeline && kind.is_empty() {
                    continue;
                }
                let temp = tempfile::tempdir().unwrap();
                let exit_path = temp.path().join("exit");
                let status_path = temp.path().join("status");
                let exit = std::fs::File::create(&exit_path).unwrap();
                let failure = std::fs::File::create(temp.path().join("failure")).unwrap();
                let _status = std::fs::File::create(&status_path).unwrap();
                let probe = temp.path().join("probe");
                // A redirection probe would create Bash's own saved stderr fd
                // and falsely report it as an inherited control descriptor.
                crate::executable_cache::write_executable(&probe, "#!/bin/bash\nif [ ! -c /dev/fd/0 ] || IFS= read -r input; then\n  echo 'command stdin was not restored to /dev/null' >&2\n  exit 44\nfi\nif [ ! -e /dev/fd/1 ] || [ ! -e /dev/fd/2 ]; then\n  echo 'descriptor lookup cannot see open stdio' >&2\n  exit 43\nfi\nfor fd in 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do\n  if [ -e \"/dev/fd/$fd\" ]; then\n    echo \"leaked control fd $fd\" >&2\n    exit 42\n  fi\ndone\nprintf clean\n".as_bytes()).unwrap();
                let payload = if pipeline {
                    format!("'{}' | cat", probe.display())
                } else {
                    format!("'{}'", probe.display())
                };
                let mut command = Command::new("/bin/sh");
                command.args([
                    "-c",
                    std::str::from_utf8(PAYLOAD_WRAPPER).unwrap(),
                    "aft-payload-wrapper",
                    shell,
                    &payload,
                    "3",
                    if pipeline {
                        status_path.to_str().unwrap()
                    } else {
                        ""
                    },
                    kind,
                    "wait-for-identity",
                ]);
                crate::sandbox_spawn::apply_marker_fd_allowlist(
                    &mut command,
                    exit.as_raw_fd(),
                    failure.as_raw_fd(),
                )
                .unwrap();
                use std::io::Write;
                use std::process::Stdio;
                command
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                let mut child = command.spawn().unwrap();
                child.stdin.take().unwrap().write_all(b"run\n").unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{shell} pipeline={pipeline}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(
                    output.stdout,
                    b"clean",
                    "{shell} pipeline={pipeline}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert_eq!(std::fs::read_to_string(&exit_path).unwrap(), "0");
                if pipeline {
                    assert_eq!(std::fs::read_to_string(&status_path).unwrap(), "0\n0\n");
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn file_pipeline_capture_preserves_shell_pipeline_semantics() {
        use std::os::fd::AsRawFd;
        use std::process::{Command, Stdio};

        let mut shells = vec![("/bin/bash", "bash")];
        if Path::new("/bin/zsh").is_file() {
            shells.push(("/bin/zsh", "zsh"));
        }
        for (shell, kind) in shells {
            for payload in [
                "! false | true",
                "time true | cat",
                "printf '%s' \"$0:${1-unset}:$#\" | cat",
                "printf 'sample\\n' | { read value; trap 'printf \"%s\\n\" \"$value\"' EXIT; }",
            ] {
                // The uninstrumented shell is the reference for shell-specific
                // last-stage scope, pipeline negation, timing and positional args.
                let reference = Command::new(shell)
                    .args(["-c", payload])
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                let temp = tempfile::tempdir().unwrap();
                let exit_path = temp.path().join("exit");
                let status_path = temp.path().join("pipeline-status");
                let exit = std::fs::File::create(&exit_path).unwrap();
                let failure = std::fs::File::create(temp.path().join("failure")).unwrap();
                let mut command = Command::new("/bin/sh");
                command.args([
                    "-c",
                    std::str::from_utf8(PAYLOAD_WRAPPER).unwrap(),
                    "aft-payload-wrapper",
                    shell,
                    payload,
                    "3",
                    status_path.to_str().unwrap(),
                    kind,
                ]);
                crate::sandbox_spawn::apply_marker_fd_allowlist(
                    &mut command,
                    exit.as_raw_fd(),
                    failure.as_raw_fd(),
                )
                .unwrap();
                let captured = command.stdin(Stdio::null()).output().unwrap();
                assert_eq!(
                    captured.status.code(),
                    reference.status.code(),
                    "{shell}: {payload}"
                );
                assert_eq!(captured.stdout, reference.stdout, "{shell}: {payload}");
                assert!(status_path.is_file(), "{shell}: capture was not published");
                assert_eq!(
                    std::fs::read_to_string(exit_path).unwrap(),
                    reference.status.code().unwrap().to_string()
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_pty_child_starts_as_its_own_session_and_process_group_leader() {
        use std::process::Command;

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 30"]);
        start_new_session(&mut command);
        let mut child = command.spawn().expect("spawn isolated child");
        let pid = child.id() as libc::pid_t;

        assert_eq!(unsafe { libc::getsid(pid) }, pid);
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);

        terminate_process(&mut child);
        let _ = child.wait();
    }

    #[test]
    fn is_process_alive_returns_false_for_dead_pid() {
        #[cfg(unix)]
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "true"])
            .spawn()
            .expect("spawn true");

        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/D", "/C", "exit 0"])
            .spawn()
            .expect("spawn cmd");

        let pid = child.id();
        child.wait().expect("wait for child");

        assert!(!is_process_alive(pid));
    }

    /// Regression: when the process-group LEADER exits during the SIGTERM grace
    /// window, `terminate_pgid` must still SIGKILL the rest of the group. A
    /// TERM-ignoring descendant (the wrapper-shell / CLI-spawns-child orphan
    /// class) used to survive because the old code returned the instant the
    /// leader was reaped, skipping the group SIGKILL.
    #[cfg(unix)]
    #[test]
    fn terminate_pgid_kills_term_ignoring_descendant_after_leader_exits() {
        use std::os::unix::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("desc.pid");
        let ready = dir.path().join("ready");

        // Leader becomes its own process-group leader (setsid → pgid == pid).
        // It backgrounds a descendant shell that ignores SIGTERM, signals
        // readiness (so the trap is definitely installed before we terminate),
        // then sleeps. The leader waits for readiness and exits — so by the time
        // we call terminate_pgid, the leader is gone and only SIGKILL can reap
        // the descendant.
        let script = format!(
            "sh -c \"trap '' TERM; echo \\$$ > '{pid}'; touch '{ready}'; sleep 30\" & \
             while [ ! -f '{ready}' ]; do sleep 0.02; done; exit 0",
            pid = pidfile.display(),
            ready = ready.display(),
        );
        let mut leader = unsafe {
            std::process::Command::new("/bin/sh")
                .args(["-c", &script])
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()
                .expect("spawn leader")
        };
        let pgid = leader.id() as i32;

        // Wait for the descendant to be ready (trap installed + pid written).
        let start = Instant::now();
        while !ready.exists() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        let desc_pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("descendant pid file")
            .trim()
            .parse()
            .expect("parse descendant pid");
        assert!(is_process_alive(desc_pid), "descendant should be alive");

        terminate_pgid(pgid, Some(&mut leader));

        // The TERM-ignoring descendant must be gone (SIGKILL'd via the group).
        let start = Instant::now();
        while is_process_alive(desc_pid) && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !is_process_alive(desc_pid),
            "TERM-ignoring descendant must be SIGKILLed when the group is terminated"
        );
    }
}

/// Check that a persisted PID still names the process instance launched for a task.
/// On platforms without process start-time inspection, a live PID is protected;
/// deleting a live task is worse than temporarily retaining a recycled PID.
pub fn is_recorded_process_alive(pid: u32, task_started_at_ms: u64) -> bool {
    const PROCESS_START_GRACE_MS: u64 = 30_000;

    if !is_process_alive(pid) {
        return false;
    }
    crate::root_cache::process_start_time_ms(pid).is_none_or(|started_at_ms| {
        started_at_ms <= task_started_at_ms.saturating_add(PROCESS_START_GRACE_MS)
    })
}
#[cfg(target_os = "linux")]
#[test]
fn lifecycle_empty_group_avoids_proc_scan() {
    let mut command = std::process::Command::new("/bin/true");
    start_new_session(&mut command);
    let mut child = command.spawn().unwrap();
    let pgid = child.id() as i32;
    child.wait().unwrap();
    PROC_SCAN_COUNT.with(|count| count.set(0));
    assert!(linux_process_group_members(pgid).unwrap().0.is_empty());
    assert_eq!(
        PROC_SCAN_COUNT.with(std::cell::Cell::get),
        0,
        "empty group required a host-wide scan"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn lifecycle_scope_probe_deadline_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let launcher = dir.path().join("systemd-run");
    crate::executable_cache::write_executable(&launcher, b"#!/bin/sh\nsleep 0.1\nexit 0\n")
        .unwrap();
    assert!(
        !probe_systemd_scope(&launcher, Duration::ZERO),
        "expired probe still reported availability"
    );
}

/// The `struct kinfo_proc` offsets AFT reads must match this process's own
/// command name, real user and process group.
#[cfg(target_os = "macos")]
#[test]
fn macos_kinfo_reads_this_process() {
    let info = macos_kinfo(std::process::id()).expect("own process info");
    assert_eq!(info.ruid, unsafe { libc::getuid() });
    assert_eq!(info.pgid, unsafe { libc::getpgrp() });
    assert!(!info.zombie);
    let exe = std::env::current_exe().unwrap();
    let name = exe.file_name().unwrap().to_string_lossy();
    assert!(
        !info.comm.is_empty() && name.starts_with(&info.comm),
        "comm {:?} is not a prefix of {name:?}",
        info.comm
    );
}

/// Another user's process is a member AFT cannot signal, and the listing must
/// still include it. launchd (pid 1, owned by root) leads process group 1.
#[cfg(target_os = "macos")]
#[test]
fn macos_group_listing_includes_another_users_process() {
    let members = macos_process_group_members(1)
        .expect("group 1 can be listed")
        .0;
    let launchd = members
        .iter()
        .find(|member| member.pid == 1)
        .unwrap_or_else(|| panic!("launchd missing from its own group: {members:?}"));
    assert_eq!(launchd.user.as_deref(), Some("root"));
    assert_eq!(launchd.comm, "launchd");
}

#[cfg(target_os = "linux")]
#[test]
fn linux_member_records_owner_and_executable_name_only() {
    let mut command = std::process::Command::new("sleep");
    command.arg("30");
    start_new_session(&mut command);
    let mut child = command.spawn().unwrap();
    let pgid = child.id() as i32;
    let members = linux_process_group_members(pgid);
    let _ = child.kill();
    let _ = child.wait();
    let (members, omitted) = members.unwrap();
    assert_eq!(omitted, 0);
    assert_eq!(members.len(), 1, "{members:?}");
    assert_eq!(members[0].command.as_deref(), Some("sleep …"));
    assert_eq!(members[0].argv0, "sleep");
    assert_eq!(
        members[0].user.as_deref(),
        Some(user_name(unsafe { libc::getuid() }).as_str())
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn shown_command_never_carries_arguments() {
    assert_eq!(shown_command("sleep", true).as_deref(), Some("sleep …"));
    assert_eq!(shown_command("sleep", false).as_deref(), Some("sleep"));
    assert_eq!(shown_command("  ", true), None);
    assert!(arguments_may_be_present(&[]));
    assert!(!arguments_may_be_present(&["sleep".into()]));
    assert!(arguments_may_be_present(&["sleep".into(), "30".into()]));
    // A rewritten process title keeps only its first word.
    assert_eq!(
        safe_argv0(Some(&"worker --token abc123".to_string()), "sh"),
        "worker"
    );
    assert_eq!(safe_argv0(None, "sh"), "sh");
    let long = "x".repeat(LIVE_DESCENDANT_NAME_CAP + 10);
    let cut = safe_name(&long);
    assert_eq!(cut.chars().count(), LIVE_DESCENDANT_NAME_CAP);
    assert!(cut.ends_with('…'));
    // Pattern masking still applies to the name itself.
    assert!(!safe_name("ghp_abcdefghijklmnopqrstuvwxyz0123").contains("ghp_abc"));
}

/// Describing stops at the cap; the members past it are counted, not read.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn bounded_members_stops_describing_at_the_cap() {
    let total = LIVE_DESCENDANT_CAP + 5;
    let candidates = (1..=total as u32).rev().map(|pid| (pid, ())).collect();
    let mut described = 0;
    let (members, omitted) = bounded_members(candidates, |pid, ()| {
        described += 1;
        Some(LiveDescendant {
            pid,
            comm: "sleep".into(),
            argv0: "sleep".into(),
            user: None,
            command: None,
        })
    });
    assert_eq!(described, LIVE_DESCENDANT_CAP);
    assert_eq!(members.len(), LIVE_DESCENDANT_CAP);
    assert_eq!(omitted, 5);
    assert_eq!(members.first().map(|member| member.pid), Some(1));
    assert_eq!(
        members.last().map(|member| member.pid),
        Some(LIVE_DESCENDANT_CAP as u32)
    );
}
