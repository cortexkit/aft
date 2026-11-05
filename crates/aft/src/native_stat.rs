//! A file's native identity and change stamps, read from its metadata.
//! Two equal stamps prove a file unchanged only when the filesystem reports
//! precise times and the stamp predates the observation (see
//! [`NativeStat::reusable`]).

use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::io_accounting::at;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum FileIdentity {
    Unix {
        dev: u64,
        ino: u64,
        birth: Option<u128>,
    },
    Windows {
        volume: u64,
        id: [u8; 16],
        creation: i64,
    },
    StrictOnly {
        volume: u64,
        index: u64,
        birth: Option<u128>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct NativeStat {
    pub identity: FileIdentity,
    pub size: u64,
    pub modified_ns: i128,
    pub changed_ns: i128,
    pub mode: u32,
    pub reuse_supported: bool,
}

impl NativeStat {
    /// Observe the handle that supplies content bytes, so a path replacement
    /// during a read cannot attach those bytes to a different file's proof.
    pub(crate) fn read_open_file(file: &fs::File, path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Self::from_unix_metadata(&file.metadata()?, path)
        }
        #[cfg(windows)]
        {
            let _ = path;
            windows_file_stat(file)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (file, path);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stable file identity unavailable",
            ))
        }
    }

    #[cfg(unix)]
    fn from_unix_metadata(m: &fs::Metadata, path: &Path) -> io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            identity: FileIdentity::Unix {
                dev: m.dev(),
                ino: m.ino(),
                birth: m
                    .created()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos()),
            },
            size: m.len(),
            modified_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            changed_ns: i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()),
            mode: m.mode(),
            reuse_supported: local_precise_filesystem(path),
        })
    }

    pub fn read(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let m = fs::symlink_metadata(path).map_err(|e| at("reading metadata", path, e))?;
            Self::from_unix_metadata(&m, path)
        }
        #[cfg(windows)]
        {
            windows_stat(path).map_err(|e| at("reading native metadata", path, e))
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stable file identity unavailable",
            ))
        }
    }
    pub fn reusable(&self, current: &Self, observed_ns: u128) -> bool {
        self.reuse_supported
            && self == current
            && self.modified_ns >= 0
            && (self.modified_ns as u128) < observed_ns
    }

    pub fn strict_read(path: &Path) -> io::Result<Self> {
        match Self::read(path) {
            Ok(stat) => Ok(stat),
            Err(error) => {
                let (metadata, identity) = crate::file_identity::symlink_metadata(path)
                    .map_err(|e| at("reading fallback metadata", path, e))?;
                let crate::file_identity::FileIdentity::Known { volume, index } = identity else {
                    return Err(error);
                };
                Ok(Self {
                    identity: FileIdentity::StrictOnly {
                        volume,
                        index,
                        birth: metadata
                            .created()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos()),
                    },
                    size: metadata.len(),
                    modified_ns: metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(-1, |d| d.as_nanos() as i128),
                    changed_ns: -1,
                    mode: 0,
                    reuse_supported: false,
                })
            }
        }
    }
}

#[cfg(unix)]
fn local_precise_filesystem(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        matches!(
            stat.f_type as u64,
            0xef53 | 0x9123683e | 0x58465342 | 0x794c7630 | 0x01021994 | 0x2fc12fc1
        )
    }
    #[cfg(target_os = "macos")]
    {
        unsafe { std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr()) }.to_bytes() == b"apfs"
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

#[cfg(windows)]
fn windows_stat(path: &Path) -> io::Result<NativeStat> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    windows_file_stat(&file)
}

#[cfg(windows)]
fn windows_file_stat(file: &fs::File) -> io::Result<NativeStat> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
        FILE_BASIC_INFO, FILE_ID_INFO,
    };
    let handle = file.as_raw_handle();
    let mut id: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    let mut filesystem = [0u16; 32];
    // All fields come from one metadata-only handle. A 64-bit file index is
    // insufficient on ReFS, and creation time is not a substitute for ChangeTime.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            &mut id as *mut _ as _,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        ) != 0
            && GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                &mut basic as *mut _ as _,
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            ) != 0
            && GetVolumeInformationByHandleW(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            ) != 0
    };
    if !ok {
        return Err(io::Error::last_os_error());
    }
    let end = filesystem
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(filesystem.len());
    let name = String::from_utf16_lossy(&filesystem[..end]);
    if !matches!(name.as_str(), "NTFS" | "ReFS") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stat reuse requires NTFS or ReFS",
        ));
    }
    let unix_ns = |ticks: i64| (i128::from(ticks) - 116_444_736_000_000_000i128) * 100;
    Ok(NativeStat {
        identity: FileIdentity::Windows {
            volume: id.VolumeSerialNumber,
            id: id.FileId.Identifier,
            creation: basic.CreationTime,
        },
        size: file.metadata()?.len(),
        modified_ns: unix_ns(basic.LastWriteTime),
        changed_ns: unix_ns(basic.ChangeTime),
        mode: basic.FileAttributes,
        reuse_supported: true,
    })
}

/// The current time in nanoseconds since the Unix epoch, rounded down to a
/// whole second, so a stamp from within the current second never counts as
/// older than the observation (see [`NativeStat::reusable`]).
pub(crate) fn now_ns() -> u128 {
    u128::from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    ) * 1_000_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_evidence_discards_identity_change_and_racy_observations() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file.rs");
        fs::write(&file, b"one").unwrap();
        let stat = NativeStat::read(&file).unwrap();
        let observed = u128::try_from(stat.modified_ns).unwrap();
        assert!(!stat.reusable(&stat, observed));
        assert!(stat.reusable(&stat, observed + 1));
        let mut changed = stat.clone();
        changed.reuse_supported = false;
        assert!(!changed.reusable(&changed, observed + 1));
        changed = stat.clone();
        changed.changed_ns += 1;
        assert!(!stat.reusable(&changed, observed + 1));
        changed = stat.clone();
        changed.identity = FileIdentity::Windows {
            volume: 42,
            id: [3; 16],
            creation: 0,
        };
        assert!(!stat.reusable(&changed, observed + 1));
    }
}
