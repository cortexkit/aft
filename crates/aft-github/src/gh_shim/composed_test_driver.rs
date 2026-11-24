use super::*;

pub fn empty_home_uses_userprofile_for_config_and_parent_exclusion(
    test_name: &str,
    no_parent: impl FnOnce(&std::path::Path) -> bool,
) {
    const CHILD: &str = "AFT_TEST_EMPTY_HOME_PROFILE";
    if let Some(profile) = std::env::var_os(CHILD).map(PathBuf::from) {
        let config_path = profile.join(".config/cortexkit/aft.jsonc");
        let connection_path = profile.join("connection.json");
        let doc = fs::read_to_string(&config_path).unwrap();
        assert_eq!(
            crate::subc_config::cortexkit_user_config_path(),
            Some(config_path)
        );
        assert_eq!(configured_connection_file(), Some(connection_path));
        assert_eq!(read_user_config_doc(), Some(doc));
        assert!(no_parent(&profile));

        std::env::set_var("USERPROFILE", "");
        assert_eq!(
            crate::subc_config::cortexkit_user_config_path(),
            Some(PathBuf::from(".config/cortexkit/aft.jsonc"))
        );
        assert_eq!(configured_connection_file(), None);
        assert_eq!(read_user_config_doc(), None);
        return;
    }

    // A fresh process keeps HOME changes away from concurrently running tests.
    let temp = tempfile::tempdir().unwrap();
    let profile = temp.path().join("profile");
    fs::create_dir_all(profile.join(".config/cortexkit")).unwrap();
    fs::create_dir_all(profile.join("checkout/.git")).unwrap();
    let connection_path = profile.join("connection.json");
    fs::write(&connection_path, "{}").unwrap();
    let doc = serde_json::json!({"subc": {"connection_file": connection_path}}).to_string();
    fs::write(profile.join(".config/cortexkit/aft.jsonc"), doc).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture"])
        .env(CHILD, &profile)
        .env("HOME", "")
        .env("USERPROFILE", &profile)
        .env_remove("APPDATA")
        .env_remove("XDG_CONFIG_HOME")
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "empty-home child failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
