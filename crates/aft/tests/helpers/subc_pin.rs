use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::process::Command;

/// Prepare the normal private pin before starting a module whose storage is a fixture.
pub(crate) fn prepare(command: &Command) -> io::Result<()> {
    let Some(data) = command
        .get_envs()
        .find(|(name, _)| *name == "XDG_DATA_HOME")
        .and_then(|(_, value)| value)
        .map(Path::new)
        .filter(|path| path.is_absolute())
    else {
        return Ok(());
    };
    let lookup = |name: &str| -> Option<OsString> {
        match command.get_envs().find(|(key, _)| *key == name) {
            Some((_, value)) => value.map(OsString::from),
            None => std::env::var_os(name),
        }
    };
    let storage = aft::storage_path::storage_dir_from_test_environment(&lookup);
    // The detach fixture also uses the legacy cache-dir override during
    // its busy-index test. Do not seed any storage inherited from the runner.
    if storage != data.join("cortexkit/aft") && storage != data.join("aft") {
        return Ok(());
    }
    let binary = Path::new(command.get_program());
    let parent = binary.parent().expect("cached fixture image has a parent");
    // aft_binary() publishes the executable in a private cache directory
    // outside the isolated projects and their shell write grants. Sharing a
    // seed there also covers nextest's separate processes for each test.
    aft::pinned_executable::prepare_fixture_pin(binary, &parent.join("startup-pin-seed"), &storage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn subc_fixture_pins_share_a_separate_immutable_image() {
        let root = tempfile::tempdir().unwrap();
        let cache = root.path().join("cached-image");
        fs::create_dir(&cache).unwrap();
        let binary = cache.join("aft");
        fs::write(&binary, b"fixture executable").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o500)).unwrap();
        let mut pins = Vec::new();
        for name in ["first", "second"] {
            let data = root.path().join(name);
            let mut command = Command::new(&binary);
            command
                .env("XDG_DATA_HOME", &data)
                .env_remove("AFT_STORAGE_DIR")
                .env_remove("AFT_CACHE_DIR");
            prepare(&command).unwrap();
            let directory = fs::read_dir(data.join("cortexkit/aft/run/aft-images"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap();
            let pin = directory.path().join("aft");
            assert_eq!(directory.metadata().unwrap().mode() & 0o777, 0o700);
            assert_eq!(fs::metadata(&pin).unwrap().mode() & 0o777, 0o500);
            assert_eq!(fs::read(&pin).unwrap(), b"fixture executable");
            pins.push(pin);
        }
        let first = fs::metadata(&pins[0]).unwrap();
        assert_eq!(first.ino(), fs::metadata(&pins[1]).unwrap().ino());
        assert_ne!(first.ino(), fs::metadata(binary).unwrap().ino());
        assert_eq!(first.nlink(), 3, "the seed and the two storage paths");
        fs::remove_file(&pins[0]).unwrap();
        assert_eq!(fs::read(&pins[1]).unwrap(), b"fixture executable");
    }
}
