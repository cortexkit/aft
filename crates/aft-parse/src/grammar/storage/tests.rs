use super::*;
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;

const WASM: &[u8] = include_bytes!("../../../../aft/tests/fixtures/grammars/56b2a562244e6f35f7b320bce59099126185389df1277a93e450ba4156c0e039.wasm");

#[test]
fn review_gpt10_opus9_grammar_publication_writes_one_staged_file() {
    let manifest = super::super::manifest("luau").unwrap();
    let root = tempfile::tempdir().unwrap();
    let storage = Storage {
        root: root.path().into(),
        fault: StorageFault::None,
    };
    storage.directory(true).unwrap();
    let folder = CString::new(storage.folder().as_os_str().as_bytes()).unwrap();
    // Read the kernel's queued directory events after synchronous publication.
    let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
    assert!(fd >= 0, "{}", io::Error::last_os_error());
    let mut events = unsafe { std::fs::File::from_raw_fd(fd) };
    let watch = unsafe {
        libc::inotify_add_watch(events.as_raw_fd(), folder.as_ptr(), libc::IN_MODIFY | libc::IN_MOVED_FROM)
    };
    assert!(watch >= 0, "{}", io::Error::last_os_error());
    assert_eq!(storage.publish(manifest, WASM).unwrap(), WASM);
    let mut modified = std::collections::BTreeSet::new();
    let mut renamed = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let size = match events.read(&mut buffer) {
            Ok(size) => size,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("reading inotify events: {error}"),
        };
        let mut offset = 0;
        while offset < size {
            let header = std::mem::size_of::<libc::inotify_event>();
            assert!(size - offset >= header);
            let event = unsafe { buffer.as_ptr().add(offset).cast::<libc::inotify_event>().read_unaligned() };
            offset += header;
            let name = &buffer[offset..offset + event.len as usize];
            offset += name.len();
            assert_eq!(event.mask & libc::IN_Q_OVERFLOW, 0);
            if event.mask & libc::IN_MODIFY != 0 {
                modified.insert(name.to_vec());
            }
            if event.mask & libc::IN_MOVED_FROM != 0 {
                renamed.push(name.to_vec());
            }
        }
    }
    eprintln!("{} written staging files, {} renamed", modified.len(), renamed.len());
    assert_eq!(modified.len(), 1);
    assert_eq!(renamed.len(), 1);
    assert!(modified.contains(&renamed[0]));
}

#[test]
fn review_gpt10_opus9_grammar_publication_discards_faulted_staging_file() {
    let manifest = super::super::manifest("luau").unwrap();
    for fault in [StorageFault::Write, StorageFault::Publish] {
        let root = tempfile::tempdir().unwrap();
        let storage = Storage {
            root: root.path().into(),
            fault,
        };
        assert!(storage.publish(manifest, WASM).is_err(), "{fault:?}");
        assert!(std::fs::read_dir(storage.folder()).unwrap().next().is_none());
    }
}
