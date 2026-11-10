//! Stat evidence: what the filesystem says about a file, recorded when its
//! content is read so that a later bind can trust the content without
//! reading it again.
//!
//! A later bind reuses a recorded content only when every field it observes
//! again is equal and the record is not racily clean: a file modified within
//! [`RACY_WINDOW_NS`] of the moment it was observed could be rewritten again
//! inside the same timestamp tick, keeping every field, so its record proves
//! nothing.
//!
//! On Windows a same-length rewrite can keep both modification and change
//! times (seen natively), so times alone cannot prove content there. The
//! evidence also carries the file's update sequence number from the NTFS
//! change journal and the journal's id; NTFS raises a file's USN on every
//! write. A Windows record without both is never reused after it was
//! written.

use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::checkout::StatEvidence;

/// Coarser than any filesystem clock AFT supports: FAT's two seconds.
pub const RACY_WINDOW_NS: i64 = 2_000_000_000;

/// The current time in nanoseconds since the Unix epoch.
#[must_use]
pub fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
        })
}

/// The evidence of a regular file at `path`, or `None` when the path is not
/// a regular file (a directory, a symlink or anything else). Symlinks are
/// described, never followed.
///
/// # Errors
///
/// When the path cannot be described.
pub fn observe(path: &Path) -> io::Result<Option<StatEvidence>> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Ok(None);
    }
    let observed_ns = now_ns();
    platform::evidence(path, &metadata, observed_ns).map(Some)
}

impl StatEvidence {
    /// Whether a file observed again with `now` still holds the content this
    /// evidence was recorded for.
    #[must_use]
    pub fn proves_unchanged(&self, now: &Self) -> bool {
        if self.is_racy() || !platform::REUSE_WITHOUT_JOURNAL && self.usn.is_none() {
            return false;
        }
        self.size == now.size
            && self.mtime_ns == now.mtime_ns
            && self.ctime_ns == now.ctime_ns
            && self.birth_ns == now.birth_ns
            && self.file_id == now.file_id
            && self.device == now.device
            && self.usn == now.usn
            && self.journal == now.journal
    }

    /// Whether the file was modified so close to its observation that a
    /// rewrite in the same clock tick would be invisible.
    #[must_use]
    pub fn is_racy(&self) -> bool {
        self.observed_ns
            .saturating_sub(self.mtime_ns.max(self.ctime_ns))
            < RACY_WINDOW_NS
    }
}

#[cfg(unix)]
mod platform {
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::time::UNIX_EPOCH;

    use crate::checkout::StatEvidence;

    /// Unix times change on every write, so no journal is needed.
    pub(super) const REUSE_WITHOUT_JOURNAL: bool = true;

    pub(super) fn evidence(
        _path: &Path,
        metadata: &std::fs::Metadata,
        observed_ns: i64,
    ) -> io::Result<StatEvidence> {
        let birth_ns = metadata
            .created()
            .ok()
            .and_then(|created| created.duration_since(UNIX_EPOCH).ok())
            .and_then(|since| i64::try_from(since.as_nanos()).ok());
        Ok(StatEvidence {
            size: metadata.size(),
            mtime_ns: metadata
                .mtime()
                .saturating_mul(1_000_000_000)
                .saturating_add(metadata.mtime_nsec()),
            ctime_ns: metadata
                .ctime()
                .saturating_mul(1_000_000_000)
                .saturating_add(metadata.ctime_nsec()),
            birth_ns,
            file_id: metadata.ino(),
            device: metadata.dev(),
            usn: None,
            journal: None,
            observed_ns,
        })
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::OpenOptions;
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;

    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileBasicInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_FILE_USN_DATA, READ_FILE_USN_DATA, USN_JOURNAL_DATA_V0,
        USN_RECORD_V2,
    };

    use crate::checkout::StatEvidence;

    /// Windows times can survive a same-length rewrite, so a record is
    /// reused only with a USN.
    pub(super) const REUSE_WITHOUT_JOURNAL: bool = false;

    /// 100 ns ticks between 1601-01-01 and the Unix epoch.
    const EPOCH_TICKS: i64 = 116_444_736_000_000_000;

    fn ticks_to_ns(ticks: i64) -> i64 {
        ticks.saturating_sub(EPOCH_TICKS).saturating_mul(100)
    }

    pub(super) fn evidence(
        path: &Path,
        metadata: &std::fs::Metadata,
        observed_ns: i64,
    ) -> io::Result<StatEvidence> {
        // A handle that reads attributes only: it neither blocks writers nor
        // deleters, and opening it does not change the file's times.
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let handle = file.as_raw_handle() as HANDLE;

        let mut basic = FILE_BASIC_INFO::default();
        // SAFETY: `handle` is open for the duration of the call and `basic`
        // is a FILE_BASIC_INFO of the size passed.
        let ok = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                (&raw mut basic).cast(),
                u32::try_from(size_of::<FILE_BASIC_INFO>()).expect("small struct"),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        // SAFETY: as above, with a BY_HANDLE_FILE_INFORMATION.
        if unsafe { GetFileInformationByHandle(handle, &raw mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let (usn, journal) = match (file_usn(handle), journal_id(handle)) {
            (Ok(usn), Ok(journal)) => (Some(usn), Some(journal)),
            _ => (None, None),
        };
        Ok(StatEvidence {
            size: metadata.len(),
            mtime_ns: ticks_to_ns(basic.LastWriteTime),
            ctime_ns: ticks_to_ns(basic.ChangeTime),
            birth_ns: Some(ticks_to_ns(basic.CreationTime)),
            file_id: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            device: u64::from(info.dwVolumeSerialNumber),
            usn,
            journal,
            observed_ns,
        })
    }

    /// The file's update sequence number (USN) and its volume's journal id,
    /// each with the error that kept it from being read, for tests to tell
    /// an absent journal from a refused read.
    #[cfg(test)]
    pub(super) fn journal_reads(path: &Path) -> io::Result<(io::Result<u64>, io::Result<u64>)> {
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        let handle = file.as_raw_handle() as HANDLE;
        Ok((file_usn(handle), journal_id(handle)))
    }

    /// The file's current USN. Fails when the volume keeps no journal or
    /// the read is refused.
    fn file_usn(handle: HANDLE) -> io::Result<u64> {
        let request = READ_FILE_USN_DATA {
            MinMajorVersion: 2,
            MaxMajorVersion: 2,
        };
        // A V2 record with room for a long file name.
        let mut buffer = [0u64; 128];
        let mut returned = 0u32;
        // SAFETY: the input and output buffers outlive the call and their
        // sizes are passed; `handle` is open.
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_READ_FILE_USN_DATA,
                (&raw const request).cast(),
                u32::try_from(size_of::<READ_FILE_USN_DATA>()).expect("small struct"),
                buffer.as_mut_ptr().cast(),
                u32::try_from(size_of_val(&buffer)).expect("small buffer"),
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if (returned as usize) < size_of::<USN_RECORD_V2>() {
            return Err(io::Error::other(
                "the USN record is shorter than a V2 record",
            ));
        }
        // SAFETY: the call wrote at least a whole V2 record into the
        // 8-byte-aligned buffer.
        let record = unsafe { &*buffer.as_ptr().cast::<USN_RECORD_V2>() };
        if record.MajorVersion != 2 {
            return Err(io::Error::other("the USN record is not version 2"));
        }
        Ok(record.Usn.cast_unsigned())
    }

    /// The id of the change journal of the file's volume. A journal deleted
    /// and created again gets a new id, which invalidates every USN taken
    /// under the old one.
    fn journal_id(handle: HANDLE) -> io::Result<u64> {
        let mut data = USN_JOURNAL_DATA_V0::default();
        let mut returned = 0u32;
        // SAFETY: the output buffer outlives the call and its size is
        // passed; `handle` is open.
        let ok = unsafe {
            DeviceIoControl(
                handle,
                FSCTL_QUERY_USN_JOURNAL,
                std::ptr::null(),
                0,
                (&raw mut data).cast(),
                u32::try_from(size_of::<USN_JOURNAL_DATA_V0>()).expect("small struct"),
                &raw mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(data.UsnJournalID)
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use std::io;
    use std::path::Path;

    use crate::checkout::StatEvidence;

    pub(super) const REUSE_WITHOUT_JOURNAL: bool = false;

    pub(super) fn evidence(
        _path: &Path,
        metadata: &std::fs::Metadata,
        observed_ns: i64,
    ) -> io::Result<StatEvidence> {
        Ok(StatEvidence {
            size: metadata.len(),
            observed_ns,
            ..StatEvidence::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rewrite_breaks_the_evidence_and_fresh_files_are_racy() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").unwrap();
        let first = observe(&path).unwrap().unwrap();
        assert_eq!(first.size, 3);
        // Just written: racily clean, so it proves nothing yet.
        assert!(first.is_racy());
        assert!(!first.proves_unchanged(&first));

        let old = StatEvidence {
            observed_ns: first.observed_ns + RACY_WINDOW_NS,
            ..first
        };
        // Unix times prove an unchanged file; Windows evidence also needs a
        // USN, which a temporary volume may not keep.
        let again = observe(&path).unwrap().unwrap();
        assert_eq!(
            old.proves_unchanged(&again),
            cfg!(unix) || again.usn.is_some()
        );

        std::fs::write(&path, "two!").unwrap();
        let rewritten = observe(&path).unwrap().unwrap();
        assert!(!old.proves_unchanged(&rewritten));

        assert!(observe(dir.path()).unwrap().is_none());
    }

    #[test]
    fn a_changed_or_missing_journal_proves_nothing() {
        let recorded = StatEvidence {
            size: 3,
            mtime_ns: 1_000,
            ctime_ns: 1_000,
            birth_ns: Some(1_000),
            file_id: 7,
            device: 9,
            usn: Some(40),
            journal: Some(1),
            observed_ns: 1_000 + RACY_WINDOW_NS,
        };
        assert!(recorded.proves_unchanged(&recorded));
        // The journal was deleted and created again: its USNs restart, so an
        // equal number means nothing.
        assert!(!recorded.proves_unchanged(&StatEvidence {
            journal: Some(2),
            ..recorded
        }));
        assert!(!recorded.proves_unchanged(&StatEvidence {
            usn: Some(41),
            ..recorded
        }));
        // The journal is gone now.
        let unjournaled = StatEvidence {
            usn: None,
            journal: None,
            ..recorded
        };
        assert!(!recorded.proves_unchanged(&unjournaled));
        // Without a journal only Unix times prove anything.
        assert_eq!(unjournaled.proves_unchanged(&unjournaled), cfg!(unix));
    }

    /// NTFS can keep every time of a same-length rewrite, and a program can
    /// set them back on purpose; the USN still moves. Run natively: the
    /// temporary folder's volume must keep a change journal (Windows keeps
    /// one on the system volume), and the reads must work for the user the
    /// tests run as.
    #[cfg(windows)]
    #[test]
    fn windows_evidence_reads_the_journal_and_catches_a_rewrite_that_keeps_every_time() {
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;

        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_BASIC_INFO, FILE_READ_ATTRIBUTES, FILE_WRITE_ATTRIBUTES, FileBasicInfo,
            GetFileInformationByHandleEx, SetFileInformationByHandle,
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").unwrap();
        let (usn, journal) = platform::journal_reads(&path).unwrap();
        assert!(
            usn.is_ok() && journal.is_ok(),
            "{}: USN {usn:?}, journal {journal:?}",
            path.display()
        );
        let first = observe(&path).unwrap().unwrap();
        assert_eq!(first.usn, usn.ok());
        assert_eq!(first.journal, journal.ok());

        let old = StatEvidence {
            observed_ns: first.observed_ns + RACY_WINDOW_NS,
            ..first
        };
        assert!(old.proves_unchanged(&observe(&path).unwrap().unwrap()));

        let attributes = std::fs::OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES)
            .open(&path)
            .unwrap();
        let mut times = FILE_BASIC_INFO::default();
        let size = u32::try_from(size_of::<FILE_BASIC_INFO>()).unwrap();
        // SAFETY: the handle is open and `times` is a FILE_BASIC_INFO of the
        // size passed.
        let read = unsafe {
            GetFileInformationByHandleEx(
                attributes.as_raw_handle() as HANDLE,
                FileBasicInfo,
                (&raw mut times).cast(),
                size,
            )
        };
        assert_ne!(read, 0, "{}", io::Error::last_os_error());
        drop(attributes);

        let mut writer = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        writer.write_all(b"two").unwrap();
        drop(writer);
        let attributes = std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .open(&path)
            .unwrap();
        // SAFETY: as above; the call only reads `times`.
        let written = unsafe {
            SetFileInformationByHandle(
                attributes.as_raw_handle() as HANDLE,
                FileBasicInfo,
                (&raw const times).cast(),
                size,
            )
        };
        assert_ne!(written, 0, "{}", io::Error::last_os_error());
        drop(attributes);

        let rewritten = observe(&path).unwrap().unwrap();
        // Everything but the USN says the file is as it was.
        assert_eq!(
            (
                rewritten.size,
                rewritten.mtime_ns,
                rewritten.ctime_ns,
                rewritten.birth_ns,
                rewritten.file_id,
                rewritten.journal
            ),
            (
                old.size,
                old.mtime_ns,
                old.ctime_ns,
                old.birth_ns,
                old.file_id,
                old.journal
            )
        );
        assert_ne!(rewritten.usn, old.usn);
        assert!(!old.proves_unchanged(&rewritten));
    }
}
