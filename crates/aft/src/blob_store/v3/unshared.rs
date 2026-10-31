//! The bytes a file holds that no other file shares.
//!
//! A view member's derived call-graph database usually starts as a clone of
//! its base's database (APFS `clonefile`), sharing every block until the
//! member writes. `stat` reports a clone's blocks as if the file owned them,
//! so charging members by `stat` counts the shared data once per clone. On
//! one repository that charged a single checkout 4.0 GB for 2.4 GB on disk,
//! and the family limit then refused a second checkout at the same commit.
//!
//! Only macOS can say what a file holds alone without opening it. Linux
//! reports shared extents only through `FIEMAP`, which needs a descriptor, and
//! opening then closing a descriptor on a database this process has open
//! releases SQLite's locks on it. So elsewhere the `stat` charge stands.

use std::path::Path;

/// What a file under a member view is charged: the bytes it holds alone where
/// the filesystem can report them, otherwise `allocated`, its `stat` charge.
/// Never more than `allocated`.
pub(super) fn charge(path: &Path, allocated: u64) -> u64 {
    private_bytes(path).map_or(allocated, |bytes| bytes.min(allocated))
}

/// `ATTR_CMNEXT_PRIVATESIZE`: the size of the file's data not shared with a
/// clone. `getattrlist` takes a path, so no descriptor is opened. `None` when
/// the filesystem does not return the attribute (HFS+, exFAT, network
/// volumes) or the call fails.
#[cfg(target_os = "macos")]
fn private_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    // Attributes come back packed at 4-byte alignment after the length word:
    // the returned-attributes set, then the private size.
    #[repr(C, packed(4))]
    struct Reply {
        length: u32,
        returned: libc::attribute_set_t,
        private_size: libc::off_t,
    }

    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: attrlist is a plain C struct for which all-zero is valid.
    let mut request: libc::attrlist = unsafe { std::mem::zeroed() };
    request.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    request.commonattr = libc::ATTR_CMN_RETURNED_ATTRS;
    request.forkattr = libc::ATTR_CMNEXT_PRIVATESIZE;
    let mut reply = std::mem::MaybeUninit::<Reply>::zeroed();
    // SAFETY: `path` is NUL-terminated, `request` is a valid attrlist, and the
    // buffer is `size_of::<Reply>()` writable bytes that outlive the call.
    let status = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            std::ptr::addr_of_mut!(request).cast(),
            reply.as_mut_ptr().cast(),
            std::mem::size_of::<Reply>(),
            libc::FSOPT_ATTR_CMN_EXTENDED | libc::FSOPT_NOFOLLOW,
        )
    };
    if status != 0 {
        return None;
    }
    // SAFETY: the buffer was zeroed and getattrlist succeeded, so every field
    // holds an initialized value.
    let reply = unsafe { reply.assume_init() };
    let returned = reply.returned;
    if returned.forkattr & libc::ATTR_CMNEXT_PRIVATESIZE == 0 {
        return None;
    }
    let private_size = reply.private_size;
    u64::try_from(private_size).ok()
}

#[cfg(not(target_os = "macos"))]
fn private_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_shared_with_nothing_is_charged_its_stat_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain");
        std::fs::write(&path, vec![7u8; 1 << 20]).unwrap();
        let allocated = stat_charge(&path);
        assert_eq!(charge(&path, allocated), allocated);
    }

    #[test]
    fn the_charge_never_exceeds_the_stat_charge() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("plain");
        std::fs::write(&path, vec![7u8; 1 << 20]).unwrap();
        assert_eq!(charge(&path, 4096), 4096);
    }

    #[test]
    fn a_missing_file_falls_back_to_the_given_charge() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(charge(&dir.path().join("missing"), 1234), 1234);
    }

    #[cfg(unix)]
    pub(crate) fn stat_charge(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).unwrap().blocks() * 512
    }

    #[cfg(not(unix))]
    pub(crate) fn stat_charge(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().len()
    }
}
