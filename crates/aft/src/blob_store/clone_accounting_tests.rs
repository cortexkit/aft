//! Family accounting over clones, with plain fixture files.
//!
//! These tests live outside the v3 source fence because they write into a
//! clone at an offset, which needs a writable descriptor. The files are plain
//! bytes, never a SQLite file set; the walk charges by directory, not by name.

use std::fs;
use std::path::Path;

use super::v3::allocated_bytes;
use crate::views::base_refs::BASE_SCOPE_PREFIX;

/// A member view's derived database is a clone of its base's. Accounting must
/// charge the shared blocks once, through the base, and the member only for
/// what it wrote since. Where clones exist (macOS), one 4 MiB base plus a fresh
/// clone costs the base alone, and writing 1 MiB into the clone adds 1 MiB.
/// A clone under another base is charged in full, since bases always are.
/// Elsewhere the member is a full copy and is charged in full.
#[test]
fn accounting_charges_a_member_clone_only_for_what_it_wrote() {
    let family = tempfile::tempdir().unwrap();
    let views = family.path().join("views");
    let base_dir = views.join(format!("{BASE_SCOPE_PREFIX}aa"));
    let member_dir = views.join("member-scope.callgraph");
    fs::create_dir_all(&base_dir).unwrap();
    fs::create_dir_all(&member_dir).unwrap();
    let base = base_dir.join("derived-g.bin");
    let payload: Vec<u8> = (0..4u32 << 20)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8)
        .collect();
    fs::write(&base, &payload).unwrap();
    let base_charge = stat_charge(&base);
    let member = member_dir.join("derived-h.bin");

    #[cfg(target_os = "macos")]
    {
        use std::io::{Seek, SeekFrom, Write};

        clone_file(&base, &member);
        assert_eq!(
            allocated_bytes(family.path()).unwrap(),
            base_charge,
            "a fresh clone shares every block with its base"
        );
        let mut file = fs::OpenOptions::new().write(true).open(&member).unwrap();
        file.seek(SeekFrom::Start(1 << 20)).unwrap();
        file.write_all(&vec![0x5a; 1 << 20]).unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert_eq!(
            allocated_bytes(family.path()).unwrap(),
            base_charge + (1 << 20),
            "the member is charged only for the block it wrote"
        );

        let other_base = views.join(format!("{BASE_SCOPE_PREFIX}bb"));
        fs::create_dir_all(&other_base).unwrap();
        clone_file(&base, &other_base.join("derived-g.bin"));
        assert_eq!(
            allocated_bytes(family.path()).unwrap(),
            2 * base_charge + (1 << 20),
            "a base is charged in full even when it shares blocks"
        );
    }
    #[cfg(not(target_os = "macos"))]
    {
        fs::write(&member, &payload).unwrap();
        assert_eq!(
            allocated_bytes(family.path()).unwrap(),
            base_charge + stat_charge(&member),
            "without clone reporting, a member is charged its stat bytes"
        );
    }
}

#[cfg(target_os = "macos")]
fn clone_file(source: &Path, destination: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let source = std::ffi::CString::new(source.as_os_str().as_bytes()).unwrap();
    let destination = std::ffi::CString::new(destination.as_os_str().as_bytes()).unwrap();
    // SAFETY: both paths are NUL-terminated and live for the call.
    let status = unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), 0) };
    assert_eq!(status, 0, "clonefile: {}", std::io::Error::last_os_error());
}

#[cfg(unix)]
fn stat_charge(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(path).unwrap().blocks() * 512
}

#[cfg(not(unix))]
fn stat_charge(path: &Path) -> u64 {
    fs::symlink_metadata(path).unwrap().len()
}
