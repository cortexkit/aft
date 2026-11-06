//! The volume and boot a path lives on: which mounted filesystem holds it,
//! that filesystem's identity, and this boot's identity. Path identity uses
//! them to tell a project root from a volume root, the consent probe to judge
//! a directory's scope, the cache location check to notice a cache folder
//! that moved to another disk, and speech process identities to tell one
//! boot's process ids from another's.

use std::fs;
use std::path::{Path, PathBuf};

#[cfg(unix)]
fn device(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(path).ok().map(|metadata| metadata.dev())
}
#[cfg(windows)]
fn device(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetVolumePathNameW(path: *const u16, volume: *mut u16, length: u32) -> i32;
        fn GetVolumeInformationW(
            volume: *const u16,
            name: *mut u16,
            name_length: u32,
            serial: *mut u32,
            component_length: *mut u32,
            flags: *mut u32,
            filesystem: *mut u16,
            filesystem_length: u32,
        ) -> i32;
    }
    // A deep cache parent is past the plain-path limit of GetVolumePathNameW.
    let path = crate::windows_path::win32_call_path(path)
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut volume = vec![0_u16; 32768];
    let mut serial = 0;
    unsafe {
        if GetVolumePathNameW(path.as_ptr(), volume.as_mut_ptr(), volume.len() as u32) == 0 {
            return None;
        }
        if GetVolumeInformationW(
            volume.as_ptr(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            0,
        ) == 0
        {
            return None;
        }
    }
    Some(u64::from(serial))
}
#[cfg(not(any(unix, windows)))]
fn device(_path: &Path) -> Option<u64> {
    None
}

#[cfg(target_os = "macos")]
fn mount_point(path: &Path) -> Option<PathBuf> {
    use std::ffi::{CStr, CString};
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // statfs returns the actual mount, including the APFS Data volume behind
    // firmlinks; walking lexical parents can instead end at the system snapshot.
    let info = unsafe {
        if libc::statfs(path.as_ptr(), info.as_mut_ptr()) != 0 {
            return None;
        }
        info.assume_init()
    };
    let bytes = unsafe { CStr::from_ptr(info.f_mntonname.as_ptr()) }.to_bytes();
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
}

#[cfg(target_os = "macos")]
fn volume_uuid(path: &Path) -> Option<String> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut attributes = libc::attrlist {
        bitmapcount: 5,
        reserved: 0,
        commonattr: 0,
        volattr: 0x8000_0000 | 0x0004_0000, // ATTR_VOL_INFO | ATTR_VOL_UUID
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // getattrlist packs a four-byte length followed by the 16-byte volume UUID.
    let mut buffer = [0_u8; 20];
    let result = unsafe {
        libc::getattrlist(
            path.as_ptr(),
            (&mut attributes as *mut libc::attrlist).cast(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            0,
        )
    };
    if result != 0
        || u32::from_ne_bytes(buffer[..4].try_into().ok()?) != 20
        || buffer[4..].iter().all(|b| *b == 0)
    {
        return None;
    }
    Some(buffer[4..].iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(target_os = "linux")]
fn volume_uuid(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let dev = device(path)?;
    for entry in fs::read_dir("/dev/disk/by-uuid").ok()? {
        let entry = entry.ok()?;
        if fs::metadata(entry.path()).ok()?.rdev() == dev {
            return entry.file_name().to_str().map(str::to_owned);
        }
    }
    None
}

#[cfg(windows)]
fn volume_uuid(path: &Path) -> Option<String> {
    device(path).map(|serial| format!("windows-volume-serial:{serial}"))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn volume_uuid(_path: &Path) -> Option<String> {
    None
}

#[cfg(target_os = "macos")]
pub(crate) fn boot_id() -> Option<String> {
    let mut buffer = [0_u8; 128];
    let mut size = buffer.len();
    let result = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if result != 0 || size > buffer.len() {
        return None;
    }
    std::str::from_utf8(&buffer[..size])
        .ok()
        .map(|s| s.trim_end_matches('\0').to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(target_os = "linux")]
pub(crate) fn boot_id() -> Option<String> {
    fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn boot_id() -> Option<String> {
    None
}

/// The root of the mounted filesystem holding `root`, with that
/// filesystem's device number.
pub(crate) fn volume_anchor(root: &Path) -> Option<(PathBuf, u64)> {
    #[cfg(target_os = "macos")]
    {
        let anchor = mount_point(root)?;
        return Some((anchor.clone(), device(&anchor)?));
    }
    #[cfg(not(target_os = "macos"))]
    {
        let dev = device(root)?;
        let mut anchor = root;
        while let Some(parent) = anchor.parent() {
            if device(parent) != Some(dev) {
                break;
            }
            anchor = parent;
        }
        Some((anchor.to_path_buf(), dev))
    }
}
/// The identity of the filesystem holding `root`, where the platform
/// reports one.
pub(crate) fn cache_volume_uuid(root: &Path) -> Option<String> {
    volume_uuid(root)
}
