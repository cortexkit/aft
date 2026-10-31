use super::super::atomic_write_source;
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};

fn with_fault<T>(fault: Fault, f: impl FnOnce() -> T) -> T {
    struct Reset(Fault);
    impl Drop for Reset {
        fn drop(&mut self) {
            FAULT.set(self.0);
        }
    }
    let _reset = Reset(FAULT.replace(fault));
    f()
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.txt");
    std::fs::write(&path, b"original").unwrap();
    (dir, path)
}

fn assert_no_temporary(dir: &Path) {
    let entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, [std::ffi::OsString::from("source.txt")]);
}

fn short_name(path: &Path) -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt;
    let mut found: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe { FindFirstFileW(wide(path).as_ptr(), &mut found) };
    assert_ne!(
        handle,
        INVALID_HANDLE_VALUE,
        "{}",
        std::io::Error::last_os_error()
    );
    let _handle = FindHandle(handle);
    let end = found
        .cAlternateFileName
        .iter()
        .position(|c| *c == 0)
        .unwrap();
    std::ffi::OsString::from_wide(&found.cAlternateFileName[..end])
}

#[test]
fn windows_posix_atomic_short_name_is_regenerated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("windows_replace.rs");
    std::fs::write(&path, b"original").unwrap();
    assert!(
        !short_name(&path).is_empty(),
        "the test volume must generate short names"
    );
    let old = information(&File::open(&path).unwrap()).unwrap();

    let outcome = atomic_write_source(&path, b"edited").unwrap();

    assert_eq!(outcome.mode, crate::edit::SourceWriteMode::Replaced);
    assert_eq!(LAST_METHOD.get(), Some(Method::PosixRename));
    assert!(outcome.warnings.is_empty());
    let new = information(&File::open(&path).unwrap()).unwrap();
    assert_ne!(
        (old.nFileIndexHigh, old.nFileIndexLow),
        (new.nFileIndexHigh, new.nFileIndexLow)
    );
    let regenerated = short_name(&path);
    assert!(!regenerated.is_empty());
    assert_eq!(
        std::fs::read(path.with_file_name(regenerated)).unwrap(),
        b"edited"
    );
}

#[test]
fn windows_posix_atomic_zone_identifier_and_named_streams_preserve_bytes() {
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
    for clear_archive in [false, true] {
        let (dir, path) = fixture();
        let streams: [(&str, &[u8]); 3] = [
            (
                "Zone.Identifier",
                b"[ZoneTransfer]\r\nZoneId=3\r\nHostUrl=https://example.test/source\r\n",
            ),
            ("private-é", &[0, 255, 128, b'x', 0]),
            ("empty", b""),
        ];
        for (name, bytes) in streams {
            std::fs::write(path.with_file_name(format!("source.txt:{name}")), bytes).unwrap();
        }
        if clear_archive {
            assert_ne!(
                unsafe { SetFileAttributesW(wide(&path).as_ptr(), FILE_ATTRIBUTE_NORMAL) },
                0
            );
        }
        let old = information(&File::open(&path).unwrap()).unwrap();

        let outcome = atomic_write_source(&path, b"edited").unwrap();

        assert_eq!(outcome.mode, crate::edit::SourceWriteMode::Replaced);
        assert_eq!(LAST_METHOD.get(), Some(Method::PosixRename));
        assert!(outcome.warnings.is_empty());
        let new = information(&File::open(&path).unwrap()).unwrap();
        assert_ne!(
            (old.nFileIndexHigh, old.nFileIndexLow),
            (new.nFileIndexHigh, new.nFileIndexLow)
        );
        for (name, bytes) in streams {
            assert_eq!(
                std::fs::read(path.with_file_name(format!("source.txt:{name}"))).unwrap(),
                bytes
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"edited");
        assert_no_temporary(dir.path());
    }
}

fn assert_fallback(fault: Fault, method: Method) {
    let (dir, path) = fixture();
    // Prove this volume and fixture can reach the fast path before forcing a guard.
    atomic_write_source(&path, b"control").unwrap();
    assert_eq!(
        LAST_METHOD.get(),
        Some(Method::PosixRename),
        "{}",
        LAST_REASON.with(|r| r.borrow().clone())
    );
    let outcome = with_fault(fault, || atomic_write_source(&path, b"edited")).unwrap();
    assert_eq!(LAST_METHOD.get(), Some(method), "fault={fault:?}");
    if method == Method::InPlace {
        assert_eq!(outcome.warnings, ["non_atomic_windows_write: atomic replacement is unavailable for this file; saved in place without removing its name. Concurrent readers may see incomplete contents."]);
    } else {
        assert!(outcome.warnings.is_empty());
    }
    assert_eq!(std::fs::read(path).unwrap(), b"edited");
    assert_no_temporary(dir.path());
}

#[test]
fn windows_posix_owner_mismatch_falls_back() {
    assert_fallback(Fault::OwnerMismatch, Method::InPlace);
}

#[test]
fn windows_posix_invalid_stream_description_falls_back() {
    assert_fallback(Fault::InvalidStreamName, Method::InPlace);
}

#[test]
fn windows_posix_stream_copy_failure_preserves_bytes_and_exact_warning() {
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_WRITE};
    let (dir, path) = fixture();
    let stream = path.with_file_name("source.txt:Zone.Identifier");
    let bytes = b"[ZoneTransfer]\r\nZoneId=3\r\n";
    std::fs::write(&stream, bytes).unwrap();
    let old = information(&File::open(&path).unwrap()).unwrap();
    // Sharing is per stream. Deny reads of the named stream while leaving the unnamed
    // stream writable and allowing delete access, so only stream copying fails.
    let held = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(&stream)
        .unwrap();
    assert_eq!(
        File::open(&stream).unwrap_err().raw_os_error(),
        Some(ERROR_SHARING_VIOLATION as i32)
    );
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    LAST_METHOD.set(None);

    let outcome = atomic_write_source(&path, b"edited").unwrap();

    assert_eq!(outcome.mode, crate::edit::SourceWriteMode::InPlace);
    assert_eq!(LAST_METHOD.get(), Some(Method::InPlace));
    let reason = LAST_REASON.with(|r| r.borrow().clone());
    assert!(
        reason.starts_with("copying named stream :Zone.Identifier:$DATA:"),
        "{reason}"
    );
    assert!(reason.contains("os error 32"), "{reason}");
    assert_eq!(outcome.warnings.len(), 1);
    assert_eq!(outcome.warnings[0].as_bytes(), b"non_atomic_windows_write: atomic replacement is unavailable for this file; saved in place without removing its name. Concurrent readers may see incomplete contents.");
    let new = information(&File::open(&path).unwrap()).unwrap();
    assert_eq!(
        (old.nFileIndexHigh, old.nFileIndexLow),
        (new.nFileIndexHigh, new.nFileIndexLow)
    );
    drop(held);
    assert_eq!(std::fs::read(&stream).unwrap(), bytes);
    assert_eq!(std::fs::read(&path).unwrap(), b"edited");
    assert_no_temporary(dir.path());
}

#[test]
fn windows_posix_unknown_checks_fall_back() {
    for fault in [
        Fault::OwnerUnknown,
        Fault::StreamsUnknown,
        Fault::LinksUnknown,
        Fault::MetadataUnknown,
    ] {
        assert_fallback(fault, Method::InPlace);
    }
}

#[test]
fn windows_posix_dacl_mismatch_falls_back() {
    assert_fallback(Fault::DaclMismatch, Method::InPlace);
}

#[test]
fn windows_posix_link_count_guard_falls_back() {
    assert_fallback(Fault::LinksUnknown, Method::InPlace);
}

#[test]
fn windows_posix_unsupported_volume_falls_back() {
    for code in [
        ERROR_NOT_SUPPORTED,
        ERROR_INVALID_PARAMETER,
        ERROR_INVALID_FUNCTION,
    ] {
        assert_fallback(Fault::Rename(code), Method::MoveFile);
        // The benchmark's unsupported-filesystem counter reads this prefix.
        let reason = LAST_REASON.with(|reason| reason.borrow().clone());
        assert!(
            reason.starts_with("rename unsupported:"),
            "{code}: {reason}"
        );
    }
}

#[test]
fn windows_posix_rename_constraints_fall_back() {
    for code in [
        ERROR_ACCESS_DENIED,
        ERROR_SHARING_VIOLATION,
        ERROR_LOCK_VIOLATION,
        ERROR_USER_MAPPED_FILE,
    ] {
        assert_fallback(Fault::Rename(code), Method::MoveFile);
        // Per-file refusals must not count as unsupported filesystems.
        let reason = LAST_REASON.with(|reason| reason.borrow().clone());
        assert!(reason.starts_with("rename failed:"), "{code}: {reason}");
    }
}

#[test]
fn windows_posix_failed_rename_keeps_original_and_removes_temp() {
    use windows_sys::Win32::Foundation::{
        ERROR_CRC, ERROR_DISK_FULL, ERROR_IO_DEVICE, ERROR_PATH_NOT_FOUND, ERROR_WRITE_FAULT,
    };

    for code in [
        ERROR_FILE_NOT_FOUND,
        ERROR_PATH_NOT_FOUND,
        ERROR_DISK_FULL,
        ERROR_CRC,
        ERROR_IO_DEVICE,
        ERROR_WRITE_FAULT,
    ] {
        let (dir, path) = fixture();
        let error = with_fault(Fault::Rename(code), || {
            atomic_write_source(&path, b"edited")
        })
        .unwrap_err();
        assert_eq!(error.raw_os_error(), Some(code as i32));
        assert_eq!(LAST_METHOD.get(), Some(Method::PosixRename));
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_no_temporary(dir.path());
    }
}

#[test]
fn windows_posix_read_only_still_refuses_write() {
    let (dir, path) = fixture();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&path, permissions.clone()).unwrap();
    assert!(atomic_write_source(&path, b"edited").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert!(std::fs::metadata(&path).unwrap().permissions().readonly());
    assert_no_temporary(dir.path());
    permissions.set_readonly(false);
    std::fs::set_permissions(&path, permissions).unwrap();
}

#[test]
fn windows_posix_legacy_failure_keeps_original_name_and_bytes() {
    let (dir, path) = fixture();
    let missing = dir.path().join("missing.txt");

    assert_eq!(
        move_file(&path, &missing).unwrap_err().raw_os_error(),
        Some(ERROR_FILE_NOT_FOUND as i32)
    );

    assert_eq!(std::fs::read(&path).unwrap(), b"original");
    assert_no_temporary(dir.path());
}

fn set_times(file: &File, creation: u64, access: u64, write: u64) {
    use windows_sys::Win32::Foundation::FILETIME;
    fn ft(value: u64) -> FILETIME {
        FILETIME {
            dwLowDateTime: value as u32,
            dwHighDateTime: (value >> 32) as u32,
        }
    }
    // SAFETY: all three FILETIMEs and the writable handle outlive the call.
    assert_ne!(
        unsafe {
            SetFileTime(
                file.as_raw_handle() as _,
                &ft(creation),
                &ft(access),
                &ft(write),
            )
        },
        0
    );
}

#[test]
fn windows_posix_verbatim_destination_is_supported() {
    let (dir, path) = fixture();
    let verbatim = std::fs::canonicalize(&path).unwrap();
    assert!(verbatim.to_string_lossy().starts_with(r"\\?\"));
    atomic_write_source(&verbatim, b"edited").unwrap();
    assert_eq!(LAST_METHOD.get(), Some(Method::PosixRename));
    assert_eq!(std::fs::read(&path).unwrap(), b"edited");
    assert_no_temporary(dir.path());
}

#[test]
fn windows_posix_verbatim_trailing_space_preserves_neighbor() {
    let dir = tempfile::tempdir().unwrap();
    let parent = std::fs::canonicalize(dir.path()).unwrap();
    let destination = parent.join("source.txt ");
    let neighbor = parent.join("source.txt");
    std::fs::write(&destination, b"original").unwrap();
    std::fs::write(&neighbor, b"neighbor").unwrap();

    atomic_write_source(&destination, b"edited").unwrap();

    assert_eq!(std::fs::read(&neighbor).unwrap(), b"neighbor");
    assert_eq!(std::fs::read(&destination).unwrap(), b"edited");
    assert_eq!(LAST_METHOD.get(), Some(Method::InPlace));
    assert!(
        LAST_REASON.with(|r| r.borrow().contains("verbatim")),
        "{}",
        LAST_REASON.with(|r| r.borrow().clone())
    );
    assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 2);
}

#[test]
fn windows_posix_open_without_delete_sharing_saves_in_place() {
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    let (dir, path) = fixture();
    atomic_write_source(&path, b"control").unwrap();
    assert_eq!(LAST_METHOD.get(), Some(Method::PosixRename));
    FALLBACK_COUNTS.with(|counts| counts.borrow_mut().clear());
    let held = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&path)
        .unwrap();

    atomic_write_source(&path, b"edited").unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), b"edited");
    assert_eq!(LAST_METHOD.get(), Some(Method::InPlace));
    assert!(FALLBACK_COUNTS.with(|counts| counts
        .borrow()
        .keys()
        .any(|reason| reason.contains("rename failed"))));
    assert_no_temporary(dir.path());
    drop(held);
}

#[test]
fn windows_posix_verbatim_name_conversion_rejects_normalization() {
    let long_path = format!(r"\\?\C:\{}\source.txt", ["segment"; 36].join(r"\"));
    assert!(rename_name(Path::new(&long_path)).is_err());
    for path in [
        r"\\?\C:\normal\source.txt ",
        r"\\?\C:\normal\source.txt.",
        r"\\?\C:\parent.\source.txt",
        r"\\?\C:\parent \source.txt",
        r"\\?\C:\normal\CON",
        r"\\?\C:\normal\con.txt",
        r"\\?\C:\normal\CON .txt",
        r"\\?\C:\normal\PRN",
        r"\\?\C:\normal\AUX.txt",
        r"\\?\C:\normal\NUL",
        r"\\?\C:\normal\COM1.txt",
        r"\\?\C:\normal\LPT9",
        r"\\?\C:\normal\COM¹",
        r"\\?\C:\normal\lpt².txt",
        r"\\?\C:\normal\COM³",
        r"\\?\C:\normal\CONIN$",
        r"\\?\C:\normal\CONOUT$",
        r"\\?\C:\normal\source.txt:stream",
        r"\\?\C:\normal\file/name.txt",
        r"\\?\C:\normal\file?name.txt",
        r"\\?\C:\normal\file\..\source.txt",
        r"\\?\C:\normal\.\source.txt",
        r"\\?\C:\normal\\source.txt",
        "\\\\?\\C:\\normal\\file\u{1}.txt",
        r"\\?\UNC\server\share\parent.\source.txt",
        r"\\?\UNC\server\share \source.txt",
    ] {
        assert!(rename_name(Path::new(path)).is_err(), "{path:?}");
    }
    for path in [
        r"\\?\C:\normal\source.txt",
        r"\\?\C:\normal\COM10.txt",
        r"\\?\C:\normal\CONSOLE.txt",
        r"\\?\C:\normal\日本語.txt",
        r"\\?\UNC\server\share\normal\source.txt",
    ] {
        assert!(rename_name(Path::new(path)).is_ok(), "{path:?}");
    }
}

#[test]
fn windows_posix_rename_path_alignment_preserves_content() {
    let dir = tempfile::tempdir().unwrap();
    // Exercise every alignment of the counted UTF-16 name within the u64
    // buffer; success must not depend on zero-filled allocator padding.
    for length in 1..=8 {
        let path = dir.path().join(format!("{}.txt", "a".repeat(length)));
        std::fs::write(&path, b"old\n").unwrap();
        atomic_write_source(&path, b"new\n").unwrap();
        assert_eq!(LAST_METHOD.get(), Some(Method::PosixRename));
        assert_eq!(std::fs::read(&path).unwrap(), b"new\n", "{path:?}");
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 8);
}

#[test]
fn windows_posix_metadata_matches_replace_file() {
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
    fn stamp(info: &BY_HANDLE_FILE_INFORMATION) -> (u32, u64, u64, u64) {
        use windows_sys::Win32::Foundation::FILETIME;
        fn t(ft: &FILETIME) -> u64 {
            ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64
        }
        (
            info.dwFileAttributes,
            t(&info.ftCreationTime),
            t(&info.ftLastAccessTime),
            t(&info.ftLastWriteTime),
        )
    }
    let mut snapshots = Vec::new();
    for guarded in [false, true] {
        let attributes = FILE_ATTRIBUTE_ARCHIVE;
        let (dir, path) = fixture();
        let temp = dir.path().join("new.txt");
        let original = OpenOptions::new()
            .write(true)
            .read(true)
            .open(&path)
            .unwrap();
        set_times(
            &original,
            132000000000000000,
            132000000010000000,
            132000000020000000,
        );
        drop(original);
        // Compare ordinary-file metadata with the historical replacement policy.
        assert_ne!(
            unsafe { SetFileAttributesW(wide(&path).as_ptr(), attributes,) },
            0
        );
        std::fs::write(&temp, b"edited").unwrap();
        super::super::windows_dacl::copy_protected(&path, &temp).unwrap();
        let file = OpenOptions::new().write(true).open(&temp).unwrap();
        set_times(
            &file,
            133000000000000000,
            133000000010000000,
            133000000020000000,
        );
        drop(file);
        let dacl_copy = dir.path().join("acl.txt");
        std::fs::write(&dacl_copy, b"").unwrap();
        super::super::windows_dacl::copy_protected(&path, &dacl_copy).unwrap();
        if guarded {
            assert!(replace(&path, &temp).unwrap());
            assert_eq!(LAST_METHOD.get(), Some(Method::PosixRename));
        } else {
            use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;
            // Compare with the historical metadata contract without using
            // ReplaceFileW on any production replacement path.
            assert_ne!(
                unsafe {
                    ReplaceFileW(
                        wide(&path).as_ptr(),
                        wide(&temp).as_ptr(),
                        std::ptr::null(),
                        0,
                        std::ptr::null(),
                        std::ptr::null(),
                    )
                },
                0
            );
        }
        let info = information(&File::open(&path).unwrap()).unwrap();
        assert_eq!(stamp(&info).1, 132000000000000000);
        if guarded && attributes == FILE_ATTRIBUTE_ARCHIVE {
            assert!(super::super::windows_dacl::matches(&path, &dacl_copy).unwrap());
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"edited");
        assert!(!temp.exists());
        snapshots.push(stamp(&info));
    }
    assert_eq!(
        snapshots[1], snapshots[0],
        "POSIX metadata must match ReplaceFileW"
    );
}

#[test]
fn windows_posix_attributes_survive_metadata_guard_in_place() {
    use windows_sys::Win32::Storage::FileSystem::SetFileAttributesW;
    let (dir, path) = fixture();
    // Windows sets ARCHIVE when writing. Keep it set in the fixture so exact
    // equality checks metadata preservation rather than write bookkeeping.
    let attributes =
        FILE_ATTRIBUTE_ARCHIVE | FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
    assert_ne!(
        unsafe { SetFileAttributesW(wide(&path).as_ptr(), attributes) },
        0
    );
    let old = information(&File::open(&path).unwrap()).unwrap();

    let outcome = with_fault(Fault::MetadataUnknown, || {
        atomic_write_source(&path, b"edited")
    })
    .unwrap();

    assert_eq!(LAST_METHOD.get(), Some(Method::InPlace));
    assert_eq!(outcome.warnings.len(), 1);
    let new = information(&File::open(&path).unwrap()).unwrap();
    assert_eq!(new.dwFileAttributes, old.dwFileAttributes);
    assert_eq!(
        new.ftCreationTime.dwHighDateTime,
        old.ftCreationTime.dwHighDateTime
    );
    assert_eq!(
        new.ftCreationTime.dwLowDateTime,
        old.ftCreationTime.dwLowDateTime
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"edited");
    assert_no_temporary(dir.path());
}

#[test]
fn windows_posix_concurrent_reader_never_loses_destination() {
    concurrent_reader_never_loses_destination(Fault::None, Method::PosixRename);
}

#[test]
fn windows_posix_unsupported_concurrent_reader_never_loses_destination() {
    for code in [
        ERROR_NOT_SUPPORTED,
        ERROR_INVALID_PARAMETER,
        ERROR_INVALID_FUNCTION,
    ] {
        concurrent_reader_never_loses_destination(Fault::Rename(code), Method::MoveFile);
    }
}

fn concurrent_reader_never_loses_destination(fault: Fault, method: Method) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("source.txt");
    eprintln!(
        "concurrent-reader fixture={} fault={fault:?}",
        path.display()
    );
    let bytes = vec![b'x'; 64 * 1024];
    std::fs::write(&path, &bytes).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(Barrier::new(2));
    let reader = {
        let path = path.clone();
        let stop = stop.clone();
        let ready = ready.clone();
        std::thread::spawn(move || {
            let mut missing = 0;
            let mut missing_codes = std::collections::BTreeMap::new();
            let mut reads = 0;
            ready.wait();
            while !stop.load(Ordering::Acquire) {
                match std::fs::read(&path) {
                    Ok(content) => assert_eq!(content, vec![b'x'; 64 * 1024]),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        missing += 1;
                        *missing_codes.entry(error.raw_os_error()).or_insert(0usize) += 1;
                    }
                    Err(error) => panic!("reader: {error}"),
                }
                reads += 1;
            }
            (reads, missing, missing_codes)
        })
    };
    ready.wait();
    let writes = std::panic::catch_unwind(|| {
        for _ in 0..200 {
            // Early in-place exits do not call the Windows replacement policy.
            // Clear the previous result so they cannot masquerade as a POSIX rename.
            LAST_METHOD.set(None);
            with_fault(fault, || atomic_write_source(&path, &bytes)).unwrap();
            assert_eq!(
                LAST_METHOD.get(),
                Some(method),
                "{}",
                LAST_REASON.with(|r| r.borrow().clone())
            );
        }
    });
    stop.store(true, Ordering::Release);
    let read_result = reader.join();
    if let Err(panic) = writes {
        std::panic::resume_unwind(panic);
    }
    let (reads, missing, missing_codes) = read_result.unwrap();
    eprintln!("concurrent-reader reads={reads} missing_by_os_code={missing_codes:?}");
    assert!(reads > 0);
    assert_eq!(missing, 0, "destination disappeared during {reads} reads");
}

/// Run on native Windows: cargo test -p agent-file-tools --lib --
/// windows_posix_1000_edit_benchmark --ignored --nocapture
#[test]
#[ignore = "1,000 durable 2 MiB edits per replacement method"]
fn windows_posix_1000_edit_benchmark() {
    for (method, fault) in [
        (Method::MoveFile, Fault::Rename(ERROR_NOT_SUPPORTED)),
        (Method::PosixRename, Fault::None),
    ] {
        let (dir, path) = fixture();
        let bytes = vec![b'x'; 2 * 1024 * 1024];
        std::fs::write(&path, &bytes).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(Barrier::new(2));
        let reader = {
            let (path, stop, ready) = (path.clone(), stop.clone(), ready.clone());
            std::thread::spawn(move || {
                let (mut reads, mut missing, mut sharing) = (0, 0, 0);
                ready.wait();
                while !stop.load(Ordering::Acquire) {
                    match std::fs::read(&path) {
                        Ok(content) => assert_eq!(content.len(), 2 * 1024 * 1024),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => missing += 1,
                        Err(error) if error.raw_os_error() == Some(32) => sharing += 1,
                        Err(error) => panic!("reader: {error}"),
                    }
                    reads += 1;
                }
                (reads, missing, sharing)
            })
        };
        ready.wait();
        let started = std::time::Instant::now();
        let mut errors = 0;
        let mut in_place = 0;
        FALLBACK_COUNTS.with(|counts| counts.borrow_mut().clear());
        with_fault(fault, || {
            for _ in 0..1000 {
                if atomic_write_source(&path, &bytes).is_err() {
                    errors += 1;
                }
                if LAST_METHOD.get() == Some(Method::InPlace) {
                    in_place += 1;
                }
            }
        });
        stop.store(true, Ordering::Release);
        let (reads, missing, sharing) = reader.join().unwrap();
        println!("{method:?}: edits=1000 bytes={} elapsed_ms={} reads={reads} not_found={missing} sharing_violations={sharing} write_errors={errors} in_place={in_place}", bytes.len(), started.elapsed().as_millis());
        FALLBACK_COUNTS.with(|counts| {
            let counts = counts.borrow();
            let count = |reason: &str| counts.get(reason).copied().unwrap_or(0);
            let unsupported: usize = counts.iter().filter(|(reason, _)| reason.starts_with("rename unsupported:")).map(|(_, count)| count).sum();
            println!("{method:?}: fallback_attributes_differ={} fallback_owner_sid_differs={} fallback_path_unrepresentable={} fallback_unsupported_filesystem={unsupported} fallback_reasons={counts:?}", count("file attributes differ"), count("owner SIDs differ"), count("rename namespace is unsupported"));
        });
        if method == Method::PosixRename {
            assert_eq!(LAST_METHOD.get(), Some(method));
            assert!(reads > 0);
            assert_eq!((missing, sharing, errors, in_place), (0, 0, 0, 0));
        }
        assert_no_temporary(dir.path());
    }
}
