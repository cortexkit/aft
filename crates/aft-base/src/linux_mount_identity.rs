use std::fs::{self, File};
use std::io;
use std::os::fd::AsRawFd;

// Including the source in equality makes a mixed-source comparison refuse.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MountIdentity {
    Statx(u64),
    Fdinfo(u64),
}

pub(super) fn file_mount_identity(file: &File) -> io::Result<MountIdentity> {
    mount_identity_from_statx(file, statx_mount_id(file))
}

fn statx_mount_id(file: &File) -> io::Result<Option<u64>> {
    let mut stat = std::mem::MaybeUninit::<libc::statx>::zeroed();
    // The glibc statx wrapper requires glibc 2.28, newer than the release binary requires.
    if unsafe {
        libc::syscall(
            libc::SYS_statx,
            file.as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_MNT_ID,
            stat.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.stx_mask & libc::STATX_MNT_ID != 0).then_some(stat.stx_mnt_id))
}

fn mount_identity_from_statx(
    file: &File,
    statx: io::Result<Option<u64>>,
) -> io::Result<MountIdentity> {
    match statx {
        Ok(Some(id)) => return Ok(MountIdentity::Statx(id)),
        Ok(None) => {}
        Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {}
        Err(error) => return Err(error),
    }

    // fdinfo reports the mountinfo ID for the open descriptor. Unlike
    // name_to_handle_at, it also works on filesystems without export support.
    fs::read(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))
        .ok()
        .and_then(|data| parse_fdinfo_mount_id(&data))
        .map(MountIdentity::Fdinfo)
        .ok_or_else(|| io::Error::other("kernel cannot verify repair mount boundary"))
}

fn parse_fdinfo_mount_id(data: &[u8]) -> Option<u64> {
    let mut fields = data
        .split(|&byte| byte == b'\n')
        .filter_map(|line| line.strip_prefix(b"mnt_id:"));
    let value = std::str::from_utf8(fields.next()?)
        .ok()?
        .trim_matches([' ', '\t']);
    if fields.next().is_some() || value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_mount_identity_parses_exactly_one_decimal_fdinfo_mount_id() {
        assert_eq!(
            parse_fdinfo_mount_id(b"pos:\t0\nflags:\t0100000\nmnt_id:\t42\nino:\t7\n"),
            Some(42)
        );
        for invalid in [
            &b"pos:\t0\n"[..],
            b"mnt_id:\t42\nmnt_id:\t42\n",
            b"mnt_id:\tabc\n",
            b"mnt_id:\t18446744073709551616\n",
            b"mnt_id:\t+42\n",
            b"mnt_id:\t\n",
        ] {
            assert_eq!(parse_fdinfo_mount_id(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn linux_mount_identity_falls_back_to_fdinfo_without_mixing_sources() {
        let dir = tempfile::tempdir().unwrap();
        let first = File::open(dir.path()).unwrap();
        let second = File::create(dir.path().join("child")).unwrap();
        let statx = file_mount_identity(&first).unwrap();
        let MountIdentity::Statx(id) = statx else {
            panic!("this test requires statx mount IDs to verify the fallback");
        };

        let missing_mask = mount_identity_from_statx(&first, Ok(None)).unwrap();
        let missing_syscall =
            mount_identity_from_statx(&second, Err(io::Error::from_raw_os_error(libc::ENOSYS)))
                .unwrap();
        assert_eq!(missing_mask, MountIdentity::Fdinfo(id));
        assert_eq!(missing_mask, missing_syscall);
        assert_ne!(statx, missing_mask);
    }
}
