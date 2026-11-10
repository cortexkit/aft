use super::*;
use crate::callgraph_store::disk_facts::DiskFacts;
use crate::callgraph_store::facts::{path_bytes, DirEntry, EntryKind};
#[test]
fn disk_facts_match_direct_probes_and_directory_walk() {
    let fixture = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(fixture.path()).unwrap();
    std::fs::create_dir(root.join("app")).unwrap();
    std::fs::write(root.join("app/main.ts"), b"export function run() {}\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("app", root.join("linked")).unwrap();
    let disk = DiskFacts::new(&root);
    for rel in [
        "",
        "app",
        "app/main.ts",
        "absent.ts",
        "linked",
        "linked/main.ts",
    ] {
        let path = root.join(rel);
        assert_eq!(disk.is_file(rel.as_bytes()), path.is_file(), "{rel}");
        assert_eq!(disk.is_dir(rel.as_bytes()), path.is_dir(), "{rel}");
        let canonical = super::super::canonicalize_path(&path);
        let canonical = super::super::relative_path(&root, &canonical);
        assert_eq!(
            disk.canonical(rel.as_bytes()),
            Some(canonical.into_bytes()),
            "{rel}"
        );
    }
    let boundary = crate::walk_boundary::DeviceBoundary::for_root(&root).unwrap();
    let mut expected = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let ty = entry.file_type().unwrap();
            let kind = if ty.is_symlink() {
                EntryKind::Symlink
            } else if ty.is_dir() {
                assert!(boundary.should_descend(&entry.path()).unwrap());
                EntryKind::Directory
            } else {
                EntryKind::Regular
            };
            DirEntry {
                name: path_bytes(Path::new(&entry.file_name())),
                kind,
            }
        })
        .collect::<Vec<_>>();
    let mut actual = disk.list_dir(b"");
    expected.sort_by(|a, b| a.name.cmp(&b.name));
    actual.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(actual, expected);
    #[cfg(unix)]
    assert_eq!(disk.symlink_target(b"linked"), Some(b"app".as_slice()));
}
