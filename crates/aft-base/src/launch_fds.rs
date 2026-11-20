//! Final descriptor sweep in the single-threaded sandbox launcher, after the
//! profile input has been consumed. Only stdio and reserved markers survive.

#[cfg(unix)]
pub fn close_inherited() -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::syscall(libc::SYS_close_range, 5_u32, u32::MAX, 0_u32) } != 0 {
            return Err(format!(
                "failed to close inherited descriptors: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // The launcher is single-threaded here. Collect before closing so the
        // directory reader's own descriptor is not invalidated during iteration.
        let descriptors: Vec<i32> = std::fs::read_dir("/dev/fd")
            .map_err(|error| error.to_string())?
            .map(|item| {
                item.map_err(|error| error.to_string())
                    .map(|item| item.file_name().to_string_lossy().parse::<i32>().ok())
            })
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .filter(|fd| *fd >= 5)
            .collect();
        for fd in descriptors {
            if unsafe { libc::close(fd) } != 0
                && std::io::Error::last_os_error().raw_os_error() != Some(libc::EBADF)
            {
                return Err(format!(
                    "failed to close inherited descriptor: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::fd::IntoRawFd;
    #[test]
    #[ignore]
    fn descriptor_sweep_child() {
        if std::env::var_os("AFT_DESCRIPTOR_SWEEP_CHILD").is_none() {
            return;
        }
        let fd = std::fs::File::open("/dev/null").unwrap().into_raw_fd();
        for target in [3, 4, 10] {
            assert!(unsafe { libc::dup2(fd, target) } >= 0);
        }
        if ![3, 4, 10].contains(&fd) {
            unsafe {
                libc::close(fd);
            }
        }
        super::close_inherited().unwrap();
        let closed = unsafe { libc::fcntl(10, libc::F_GETFD) } < 0;
        let markers = [3, 4]
            .into_iter()
            .all(|fd| unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
        std::process::exit(if closed && markers { 0 } else { 42 });
    }
    #[test]
    fn gh_shim_launcher_descriptor_sweep_preserves_only_reserved_fds() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "launch_fds::tests::descriptor_sweep_child",
                "--ignored",
                "--nocapture",
            ])
            .env("AFT_DESCRIPTOR_SWEEP_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "descriptor 10 must close while 3/4 survive: {} {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
