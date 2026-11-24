//! TCP has no portable peer credentials. Resolve the connecting socket's owner,
//! then check the kernel session identity; the URL token is not an identity.

#[cfg(not(target_os = "macos"))]
use tokio::net::TcpStream;

#[cfg(unix)]
pub(super) fn is_session_leader(pid: i32) -> bool {
    pid > 0 && unsafe { libc::getsid(pid) } == pid
}

#[cfg(not(unix))]
pub(super) fn is_session_leader(_pid: i32) -> bool {
    false
}

#[cfg(target_os = "linux")]
pub(super) fn process_start_time(pid: i32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    fields.split_whitespace().nth(19)?.parse().ok()
}

#[cfg(target_os = "linux")]
fn session_pids(session: i32) -> impl Iterator<Item = i32> {
    #[cfg(test)]
    PROCESS_TABLE_WALKS.with(|walks| walks.set(walks.get() + 1));
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(move |pid| unsafe { libc::getsid(*pid) } == session)
        .filter(|pid| running(*pid))
}

/// Whether `pid` exists and is neither a zombie nor dead.
#[cfg(target_os = "linux")]
fn running(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, fields)) = stat.rsplit_once(") ") else {
        return false;
    };
    !matches!(fields.as_bytes().first(), Some(b'Z' | b'X'))
}

/// Counts process-table walks on this thread, so a test can check that a
/// liveness check with a living leader does not enumerate every process.
#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
thread_local! {
    pub(super) static PROCESS_TABLE_WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(target_os = "linux")]
pub(super) fn session_alive(session: i32) -> bool {
    // A session leader cannot leave its session, so a running leader settles
    // the question without walking the process table. Only a leader that has
    // exited needs the walk, to find a descendant still in the session.
    if running(session) && unsafe { libc::getsid(session) } == session {
        return true;
    }
    session_pids(session).next().is_some()
}

#[cfg(target_os = "linux")]
pub(super) fn authorized(socket: &TcpStream, session: i32) -> bool {
    use std::os::unix::fs::MetadataExt;
    let (Ok(client), Ok(server)) = (socket.peer_addr(), socket.local_addr()) else {
        return false;
    };
    let encode = |addr: std::net::SocketAddr| match addr.ip() {
        std::net::IpAddr::V4(ip) => format!(
            "{:08X}:{:04X}",
            u32::from_ne_bytes(ip.octets()),
            addr.port()
        ),
        std::net::IpAddr::V6(ip) => {
            let ip = ip
                .octets()
                .chunks_exact(4)
                .map(|w| format!("{:08X}", u32::from_ne_bytes(w.try_into().unwrap())))
                .collect::<String>();
            format!("{ip}:{:04X}", addr.port())
        }
    };
    let (client, server) = (encode(client), encode(server));
    let table = if socket.local_addr().is_ok_and(|a| a.is_ipv4()) {
        "/proc/net/tcp"
    } else {
        "/proc/net/tcp6"
    };
    let Ok(table) = std::fs::read_to_string(table) else {
        return false;
    };
    let inode = table.lines().skip(1).find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.get(1) == Some(&client.as_str())
            && fields.get(2) == Some(&server.as_str())
            && fields.get(3) == Some(&"01")
        {
            fields
                .get(9)
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|i| *i != 0)
        } else {
            None
        }
    });
    let Some(inode) = inode else {
        return false;
    };
    let target = format!("socket:[{inode}]");
    for pid in session_pids(session) {
        let path = format!("/proc/{pid}/fd");
        if !std::fs::metadata(&path).is_ok_and(|m| m.uid() == unsafe { libc::geteuid() }) {
            continue;
        }
        let Ok(fds) = std::fs::read_dir(&path) else {
            continue;
        };
        if fds.filter_map(Result::ok).any(|fd| {
            std::fs::read_link(fd.path()).is_ok_and(|link| link.as_os_str() == target.as_str())
        }) && unsafe { libc::getsid(pid) } == session
        {
            return true;
        }
    }
    false
}

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(super) use macos::{authorized, process_start_time, session_alive};

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn authorized(_socket: &TcpStream, _session: i32) -> bool {
    false
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn session_alive(_session: i32) -> bool {
    false
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(super) fn process_start_time(_pid: i32) -> Option<u64> {
    None
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    fn session_leader(script: &str) -> std::process::Child {
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        // `spawn` returns after the exec, so `setsid` has already run.
        command.spawn().unwrap()
    }

    fn walks() -> usize {
        PROCESS_TABLE_WALKS.with(std::cell::Cell::get)
    }

    #[test]
    fn living_leader_is_alive_without_walking_the_process_table() {
        let mut leader = session_leader("exec sleep 60");
        let pid = leader.id() as i32;
        let before = walks();
        for _ in 0..10 {
            assert!(session_alive(pid));
        }
        assert_eq!(walks(), before, "a living leader needs no process walk");
        leader.kill().unwrap();
        leader.wait().unwrap();
    }

    #[test]
    fn exited_leader_with_a_running_descendant_keeps_the_session_alive() {
        let mut leader = session_leader("sleep 60 </dev/null >/dev/null 2>&1 & echo $!");
        let pid = leader.id() as i32;
        let mut line = String::new();
        std::io::BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let descendant: i32 = line.trim().parse().unwrap();
        // Block until the leader exits but leave it unreaped, as a zombie.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let waited = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        assert_eq!(waited, 0, "{}", std::io::Error::last_os_error());
        let before = walks();
        assert!(session_alive(pid));
        assert!(
            walks() > before,
            "an exited leader needs the descendant walk"
        );
        unsafe { libc::kill(descendant, libc::SIGKILL) };
        leader.wait().unwrap();
    }
}
