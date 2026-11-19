//! Landlock mediates filesystem access; this filter fences credentials, IPC and process groups.

const LOAD: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
const EQ: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
const HAS: u16 = (libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K) as u16;
const RET: u16 = (libc::BPF_RET | libc::BPF_K) as u16;
const DENY: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
const KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;
#[cfg(target_arch = "x86_64")]
const ARCH: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const ARCH: u32 = 0xc000_00b7;

const fn line(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

#[cfg(not(all(
    target_endian = "little",
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
pub fn apply() -> Result<(), String> {
    Err("remote shell syscall filter is not supported on this Linux architecture".into())
}

#[cfg(all(
    target_endian = "little",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub fn apply() -> Result<(), String> {
    // Offsets in struct seccomp_data: system call 0, ABI 4, arguments 16.
    let mut filter = vec![
        line(LOAD, 4, 0, 0),
        line(EQ, ARCH, 1, 0),
        line(RET, KILL, 0, 0),
        line(LOAD, 0, 0, 0),
    ];
    #[cfg(target_arch = "x86_64")]
    filter.extend([line(HAS, 0x4000_0000, 0, 1), line(RET, KILL, 0, 0)]);
    filter.extend([
        line(EQ, libc::SYS_prctl as u32, 0, 4),
        line(LOAD, 16, 0, 0),
        line(EQ, super::PROBE as u32, 0, 1),
        line(RET, libc::SECCOMP_RET_ERRNO | libc::EREMOTE as u32, 0, 0),
        line(LOAD, 0, 0, 0),
        // clone3 hides flags behind a pointer. ENOSYS lets libc use clone instead.
        line(EQ, libc::SYS_clone3 as u32, 0, 1),
        line(RET, libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32, 0, 0),
        line(EQ, libc::SYS_clone as u32, 0, 4),
        line(LOAD, 16, 0, 0),
        line(
            HAS,
            (libc::CLONE_NEWUSER
                | libc::CLONE_NEWNS
                | libc::CLONE_NEWPID
                | libc::CLONE_NEWNET
                | libc::CLONE_NEWIPC
                | libc::CLONE_NEWUTS
                | libc::CLONE_NEWCGROUP
                | libc::CLONE_PARENT) as u32,
            0,
            1,
        ),
        line(RET, DENY, 0, 0),
        line(LOAD, 0, 0, 0),
    ]);
    for syscall in [
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_keyctl,
        libc::SYS_request_key,
        libc::SYS_add_key,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_pidfd_getfd,
        libc::SYS_pidfd_send_signal,
        libc::SYS_kill,
        libc::SYS_tkill,
        libc::SYS_tgkill,
        libc::SYS_rt_sigqueueinfo,
        libc::SYS_rt_tgsigqueueinfo,
        libc::SYS_setsid,
        libc::SYS_setpgid,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_open_by_handle_at,
        libc::SYS_perf_event_open,
        libc::SYS_bpf,
        libc::SYS_mq_open,
        libc::SYS_mq_unlink,
        libc::SYS_msgget,
        libc::SYS_msgsnd,
        libc::SYS_msgrcv,
        libc::SYS_msgctl,
        libc::SYS_shmget,
        libc::SYS_shmat,
        libc::SYS_shmctl,
        libc::SYS_semget,
        libc::SYS_semop,
        libc::SYS_semtimedop,
        libc::SYS_semctl,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_move_mount,
        libc::SYS_open_tree,
        libc::SYS_fsopen,
        libc::SYS_fsmount,
        libc::SYS_fsconfig,
        libc::SYS_mount_setattr,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
    ] {
        filter.extend([line(EQ, syscall as u32, 0, 1), line(RET, DENY, 0, 0)]);
    }
    filter.push(line(RET, libc::SECCOMP_RET_ALLOW, 0, 0));
    let program = libc::sock_fprog {
        len: filter.len().try_into().expect("small static filter"),
        filter: filter.as_mut_ptr(),
    };
    // SAFETY: the kernel copies the live program and its array from these pointers.
    let code = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &program) };
    if code != 0 {
        return Err(format!(
            "could not install remote shell syscall filter: {}",
            std::io::Error::last_os_error()
        ));
    }
    super::verify()
}
