use libproc::libproc::bsd_info::BSDInfo;
use libproc::libproc::file_info::{pidfdinfo, ListFDs, ProcFDType};
use libproc::libproc::net_info::{SocketFDInfo, SocketInfoKind};
use libproc::libproc::proc_pid::{listpidinfo, pidinfo};
use libproc::processes::{pids_by_type, ProcFilter};
use tokio::net::TcpStream;

pub(crate) fn process_start_time(pid: i32) -> Option<u64> {
    let info = pidinfo::<BSDInfo>(pid, 0).ok()?;
    info.pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)
}

fn session_pids(session: i32) -> Vec<i32> {
    pids_by_type(ProcFilter::All)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pid| i32::try_from(pid).ok())
        .filter(|pid| unsafe { libc::getsid(*pid) } == session)
        .filter(|pid| pidinfo::<BSDInfo>(*pid, 0).is_ok_and(|info| info.pbi_status != 5))
        .collect()
}

pub(crate) fn session_alive(session: i32) -> bool {
    !session_pids(session).is_empty()
}

pub(crate) fn authorized(socket: &TcpStream, session: i32) -> bool {
    let (Ok(client), Ok(server)) = (socket.peer_addr(), socket.local_addr()) else {
        return false;
    };
    let (std::net::IpAddr::V4(client_ip), std::net::IpAddr::V4(server_ip)) =
        (client.ip(), server.ip())
    else {
        return false;
    };
    for pid in session_pids(session) {
        let Ok(before) = pidinfo::<BSDInfo>(pid, 0) else {
            continue;
        };
        if before.pbi_uid != unsafe { libc::geteuid() } {
            continue;
        }
        let Ok(fds) = listpidinfo::<ListFDs>(pid, before.pbi_nfiles as usize) else {
            continue;
        };
        for fd in fds {
            if !matches!(ProcFDType::from(fd.proc_fdtype), ProcFDType::Socket) {
                continue;
            }
            let Ok(socket) = pidfdinfo::<SocketFDInfo>(pid, fd.proc_fd) else {
                continue;
            };
            if socket.psi.soi_protocol != libc::IPPROTO_TCP
                || socket.psi.soi_family != libc::AF_INET
                || !matches!(
                    SocketInfoKind::from(socket.psi.soi_kind),
                    SocketInfoKind::Tcp
                )
            {
                continue;
            }
            // The kernel's kind/family tags select these union arms. Compare
            // both endpoints; a port alone is not a socket identity.
            let tcp = unsafe { socket.psi.soi_proto.pri_tcp };
            let ini = tcp.tcpsi_ini;
            let local = unsafe { ini.insi_laddr.ina_46.i46a_addr4.s_addr };
            let foreign = unsafe { ini.insi_faddr.ina_46.i46a_addr4.s_addr };
            if u16::from_be(ini.insi_lport as u16) != client.port()
                || u16::from_be(ini.insi_fport as u16) != server.port()
                || local.to_ne_bytes() != client_ip.octets()
                || foreign.to_ne_bytes() != server_ip.octets()
            {
                continue;
            }
            let Ok(after) = pidinfo::<BSDInfo>(pid, 0) else {
                continue;
            };
            if before.pbi_start_tvsec == after.pbi_start_tvsec
                && before.pbi_start_tvusec == after.pbi_start_tvusec
                && unsafe { libc::getsid(pid) } == session
            {
                return true;
            }
        }
    }
    false
}
