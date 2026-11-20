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
fn session_pids(session: i32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|pid| unsafe { libc::getsid(*pid) } == session)
        .filter(|pid| {
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                return false;
            };
            let Some((_, fields)) = stat.rsplit_once(") ") else {
                return false;
            };
            !matches!(fields.as_bytes().first(), Some(b'Z' | b'X'))
        })
        .collect()
}

#[cfg(target_os = "linux")]
pub(super) fn session_alive(session: i32) -> bool {
    !session_pids(session).is_empty()
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
