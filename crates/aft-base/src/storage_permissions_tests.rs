//! Exercise actual store writers rather than reproducing their creation logic.

use std::fs;

use std::path::Path;

#[test]
fn storage_debug_archive_ignores_public_member_modes() {
    let scratch = tempfile::tempdir().unwrap();
    let archive = scratch.path().join("debug.zip");
    write_zip_member_with_mode(&archive, "secret.txt", b"debug data", 0o100644);
    assert!(
        which::which("unzip").is_ok(),
        "required executable `unzip` is missing; debug archive extraction uses it"
    );
    let destination = scratch.path().join("storage/dsym/download");
    crate::private_storage::extract_zip(&archive, &destination).unwrap();
    assert_mode(&destination, 0o700);
    assert_mode(&destination.join("secret.txt"), 0o600);
    assert_eq!(
        fs::read_to_string(destination.join("secret.txt")).unwrap(),
        "debug data"
    );
}

// Build a stored ZIP member directly so this fixture needs no `zip` executable.
fn write_zip_member_with_mode(path: &Path, name: &str, contents: &[u8], mode: u32) {
    let name = name.as_bytes();
    let name_len = u16::try_from(name.len()).unwrap();
    let contents_len = u32::try_from(contents.len()).unwrap();
    let crc = crc32fast::hash(contents);
    let local_size = 30 + name.len() + contents.len();
    let central_size = 46 + name.len();
    let mut archive = Vec::with_capacity(local_size + central_size + 22);

    archive.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    archive.extend_from_slice(&20u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&crc.to_le_bytes());
    archive.extend_from_slice(&contents_len.to_le_bytes());
    archive.extend_from_slice(&contents_len.to_le_bytes());
    archive.extend_from_slice(&name_len.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(name);
    archive.extend_from_slice(contents);

    archive.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    archive.extend_from_slice(&0x0314u16.to_le_bytes());
    archive.extend_from_slice(&20u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&crc.to_le_bytes());
    archive.extend_from_slice(&contents_len.to_le_bytes());
    archive.extend_from_slice(&contents_len.to_le_bytes());
    archive.extend_from_slice(&name_len.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&(mode << 16).to_le_bytes());
    archive.extend_from_slice(&0u32.to_le_bytes());
    archive.extend_from_slice(name);

    archive.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());
    archive.extend_from_slice(&1u16.to_le_bytes());
    archive.extend_from_slice(&1u16.to_le_bytes());
    archive.extend_from_slice(&(central_size as u32).to_le_bytes());
    archive.extend_from_slice(&(local_size as u32).to_le_bytes());
    archive.extend_from_slice(&0u16.to_le_bytes());

    fs::write(path, archive).unwrap();
}

pub fn assert_mode(path: &Path, mode: u32) {
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        mode,
        "{}",
        path.display()
    );
}
