use super::{checked, credentials, path_arg};
use std::path::{Path, PathBuf};
use std::process::Command;

const PASSWORD: &str = "disposable-keychain-password";
const ACCOUNT: &str = "aft-acceptance";
const GENERIC: &str = "disposable-keychain-generic-item";
const INTERNET: &str = "disposable-keychain-internet-item";

pub(super) struct Keychain(PathBuf);

impl Keychain {
    pub(super) fn add_lookups(root: &Path, fixture: &mut credentials::Fixture) -> Self {
        let keychain = Self(root.join("project/acceptance.keychain-db"));
        let host = format!("aft-sandbox-{}.invalid", std::process::id());
        let security = |args: &[&str]| {
            checked(
                Command::new("/usr/bin/security")
                    .args(args)
                    .arg(&keychain.0),
            );
        };
        // A uniquely named temporary keychain joins the search list so Git can
        // find it. Cleanup removes this keychain, not the user's other entries.
        security(&["create-keychain", "-p", PASSWORD]);
        security(&["unlock-keychain", "-p", PASSWORD]);
        security(&[
            "add-generic-password",
            "-A",
            "-s",
            &host,
            "-a",
            ACCOUNT,
            "-w",
            GENERIC,
        ]);
        security(&[
            "add-internet-password",
            "-A",
            "-s",
            &host,
            "-a",
            ACCOUNT,
            "-r",
            "htps",
            "-w",
            INTERNET,
        ]);
        fixture.lookup(
            "keychain_item",
            "/usr/bin/security",
            format!(
                "@TOOL@ find-generic-password -s {host} -a {ACCOUNT} -w {}",
                path_arg(&keychain.0)
            ),
            GENERIC.into(),
        );
        fixture.lookup(
            "git_osxkeychain",
            credentials::git_helper("osxkeychain"),
            format!("printf '%s\\n' protocol=https host={host} username={ACCOUNT} '' | @TOOL@ get"),
            format!("password={INTERNET}"),
        );
        keychain
    }

    pub(super) fn readable_file_check(&self) -> String {
        // The temporary keychain is outside the protected Keychains folders.
        // A successful byte read isolates service denial from file denial.
        format!(
            "\n/bin/cat {} >/dev/null && printf 'KEYCHAIN_BYTES_READABLE\\n'\n",
            path_arg(&self.0)
        )
    }
}

impl Drop for Keychain {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/security")
            .arg("delete-keychain")
            .arg(&self.0)
            .output();
    }
}
