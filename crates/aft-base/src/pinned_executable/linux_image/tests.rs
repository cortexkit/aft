use super::*;
use std::io::{Seek, SeekFrom};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Barrier;

fn unsupported_clone(_: &File, _: &File) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::EXDEV))
}

#[test]
fn pinned_executable_linux_reuses_the_pin_without_cloning_or_copying_again() {
    let root = tempfile::tempdir().unwrap();
    let deployed = root.path().join("deployed");
    fs::write(&deployed, b"first image").unwrap();
    let source = File::open(&deployed).unwrap();
    let key = ImageKey::new(&source).unwrap();
    let (first, identity) = key
        .pin_with(root.path(), &source, unsupported_clone)
        .unwrap();
    let (second, _) = ImageKey::new(&File::open(&deployed).unwrap())
        .unwrap()
        .pin_with(root.path(), &source, |_, _| {
            panic!("reused pin was rematerialized")
        })
        .unwrap();
    assert_eq!(first, second);
    assert!(identity.matches(&second.symlink_metadata().unwrap()));
}

#[test]
fn pinned_executable_linux_changed_identity_has_a_new_pin() {
    let root = tempfile::tempdir().unwrap();
    let deployed = root.path().join("deployed");
    fs::write(&deployed, b"old image").unwrap();
    let source = File::open(&deployed).unwrap();
    let key = ImageKey::new(&source).unwrap();
    let (older, _) = key
        .pin_with(root.path(), &source, unsupported_clone)
        .unwrap();

    let replacement = root.path().join("replacement");
    fs::write(&replacement, b"new image").unwrap();
    // Equal size and mtime isolate the changed deployment inode.
    let modified = source.metadata().unwrap().modified().unwrap();
    File::open(&replacement)
        .unwrap()
        .set_modified(modified)
        .unwrap();
    fs::rename(&replacement, &deployed).unwrap();
    let source = File::open(&deployed).unwrap();
    let (newer, _) = ImageKey::new(&source)
        .unwrap()
        .pin_with(root.path(), &source, unsupported_clone)
        .unwrap();
    assert_ne!(older, newer);
    assert_eq!(fs::read(older).unwrap(), b"old image");
    assert_eq!(fs::read(newer).unwrap(), b"new image");
}

#[test]
fn pinned_executable_linux_clone_fallback_copies_the_entire_held_image() {
    let root = tempfile::tempdir().unwrap();
    let deployed = root.path().join("deployed");
    let bytes = b"real image\n".repeat(20_000);
    fs::write(&deployed, &bytes).unwrap();
    let mut source = File::open(&deployed).unwrap();
    source.seek(SeekFrom::End(0)).unwrap();
    let original = source.metadata().unwrap();
    let (pin, _) = ImageKey::new(&source)
        .unwrap()
        .pin_with(root.path(), &source, unsupported_clone)
        .unwrap();
    let pinned = pin.symlink_metadata().unwrap();
    assert_ne!(pinned.ino(), original.ino());
    assert_eq!(pinned.mode() & 0o777, 0o500);
    assert_eq!(fs::read(pin).unwrap(), bytes);
}

#[test]
fn pinned_executable_linux_concurrent_publishers_materialize_once() {
    let root = tempfile::tempdir().unwrap();
    let deployed = root.path().join("deployed");
    let bytes = b"real image\n".repeat(20_000);
    fs::write(&deployed, &bytes).unwrap();

    let barrier = Barrier::new(4);
    let attempts = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let readers = (0..4)
            .map(|_| {
                scope.spawn(|| {
                    let source = File::open(&deployed).unwrap();
                    let key = ImageKey::new(&source).unwrap();
                    barrier.wait();
                    let (pin, _) = key
                        .pin_with(root.path(), &source, |_, _| {
                            attempts.fetch_add(1, Ordering::Relaxed);
                            let directory =
                                root.path().join("aft-images").join(key.directory_name());
                            assert!(
                                !directory.join("aft").exists(),
                                "unfinished pin was published"
                            );
                            Err(io::Error::from_raw_os_error(libc::EXDEV))
                        })
                        .unwrap();
                    assert_eq!(fs::read(&pin).unwrap(), bytes);
                    pin
                })
            })
            .collect::<Vec<_>>();
        let mut pins = readers.into_iter().map(|reader| reader.join().unwrap());
        let first = pins.next().unwrap();
        assert!(pins.all(|pin| pin == first));
    });
    assert_eq!(attempts.load(Ordering::Relaxed), 1);
}
