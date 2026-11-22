use super::{checked, credentials, path_arg, GH_BINARY, REQUIRE_OSXKEYCHAIN};
use std::path::{Path, PathBuf};
use std::process::Command;

const PASSWORD: &str = "disposable-keychain-password";
const ACCOUNT: &str = "aft-acceptance";
const GENERIC: &str = "disposable-keychain-generic-item";
const INTERNET: &str = "disposable-keychain-internet-item";
const GIT_ACCOUNT: &str = "aft-sandbox-acceptance-fixture";
const GH_ACCOUNT: &str = "aft-sandbox-acceptance-gh-fixture";
const GH_TOKEN: &str = "disposable-gh-keychain-token";
const PREFIX: &str = "aft-sandbox-acceptance-";
const GIT_ITEM_ATTRIBUTES: [&str; 10] = [
    "-a",
    GIT_ACCOUNT,
    "-r",
    "htps",
    "-t",
    "dflt",
    "-P",
    "443",
    "-p",
    "acceptance",
];

pub(super) struct Keychain {
    path: PathBuf,
    git_item: Option<GitItem>,
}

impl Keychain {
    pub(super) fn add_lookups(root: &Path, fixture: &mut credentials::Fixture) -> Self {
        let mut keychain = Self {
            path: root.join("project/acceptance.keychain-db"),
            git_item: None,
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let host = format!("{PREFIX}{}-{stamp}.invalid", std::process::id());
        let security = |args: &[&str]| {
            checked(
                Command::new("/usr/bin/security")
                    .args(args)
                    .arg(&keychain.path),
            );
        };
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
        fixture.lookup(
            "keychain_item",
            "/usr/bin/security",
            format!(
                "@TOOL@ find-generic-password -s {host} -a {ACCOUNT} -w {}",
                path_arg(&keychain.path)
            ),
            GENERIC.into(),
        );
        if std::env::var(REQUIRE_OSXKEYCHAIN).as_deref() == Ok("1") {
            keychain.git_item = Some(GitItem::seed(&host));
            fixture.lookup(
                "git_osxkeychain",
                credentials::git_helper("osxkeychain"),
                format!("printf '%s\\n' protocol=https host={host}:443 path=acceptance username={GIT_ACCOUNT} '' | @TOOL@ get"),
                format!("password={INTERNET}"),
            );
        } else {
            eprintln!("osxkeychain item not tested: only the Mac mini CI runner provisions a default keychain for this probe");
        }
        keychain
    }

    pub(super) fn readable_file_check(&self) -> String {
        // The temporary keychain is outside the protected Keychains folders.
        // A successful byte read isolates service denial from file denial.
        format!(
            "\n/bin/cat {} >/dev/null && printf 'KEYCHAIN_BYTES_READABLE\\n'\n",
            path_arg(&self.path)
        )
    }

    pub(super) fn gh_lookup(&self, root: &Path) -> Option<GhItem> {
        let binary = std::env::var_os(GH_BINARY).filter(|path| !path.is_empty())?;
        let fixture = self
            .git_item
            .as_ref()
            .expect("the gh keyring probe needs the dedicated Mac mini test keychain");
        let config = root.join("project/gh-keyring-probe");
        std::fs::create_dir(&config).unwrap();
        // Supply only the active username. The token must come from the keychain.
        std::fs::write(
            config.join("hosts.yml"),
            format!(
                "{}:\n  user: {GH_ACCOUNT}\n  users:\n    {GH_ACCOUNT}: {{}}\n",
                fixture.host
            ),
        )
        .unwrap();
        let service = format!("gh:{}", fixture.host);
        checked(
            Command::new("/usr/bin/security")
                .args([
                    "add-generic-password",
                    "-A",
                    "-s",
                    &service,
                    "-a",
                    GH_ACCOUNT,
                    "-w",
                    GH_TOKEN,
                ])
                .arg(&fixture.keychain),
        );
        Some(GhItem {
            keychain: fixture.keychain.clone(),
            host: fixture.host.clone(),
            service,
            binary: binary.into(),
            config,
        })
    }
}

impl Drop for Keychain {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/security")
            .arg("delete-keychain")
            .arg(&self.path)
            .output();
    }
}

struct GitItem {
    keychain: PathBuf,
    host: String,
}

impl GitItem {
    fn seed(host: &str) -> Self {
        let output = Command::new("/usr/bin/security")
            .args(["default-keychain", "-d", "user"])
            .output()
            .expect("check Mac mini CI default keychain");
        let keychain = PathBuf::from(
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .trim_matches('"'),
        );
        assert!(output.status.success()
                && keychain == Path::new("/Users/ghrunner/Library/Keychains/ci.keychain-db")
                && keychain.is_file(),
            "osxkeychain acceptance will only seed the Mac mini runner's dedicated default ci.keychain");

        // Check the reserved account even if an earlier run seeded different
        // protocol attributes. Remove only a server with the fixture prefix.
        loop {
            let output = Command::new("/usr/bin/security")
                .args(["find-internet-password", "-a", GIT_ACCOUNT])
                .arg(&keychain)
                .output()
                .unwrap();
            if output.status.code() == Some(44) {
                break; // security's exit status when no item matches.
            }
            assert!(
                output.status.success(),
                "could not check for stale osxkeychain fixtures: {output:?}"
            );
            let attributes = String::from_utf8(output.stdout).unwrap();
            let stale = attributes
                .lines()
                .find_map(|line| {
                    line.trim()
                        .strip_prefix("\"srvr\"<blob>=\"")?
                        .strip_suffix('"')
                })
                .expect("osxkeychain fixture item did not report its server name");
            assert!(
                stale.starts_with(PREFIX) && stale.ends_with(".invalid"),
                "unexpected item for the reserved osxkeychain fixture account; refusing deletion"
            );
            checked(&mut Self::delete_command(&keychain, stale));
        }

        checked(
            Command::new("/usr/bin/security")
                .args(["add-internet-password", "-A", "-s", host, "-w", INTERNET])
                .args(GIT_ITEM_ATTRIBUTES)
                .arg(&keychain),
        );
        Self {
            keychain,
            host: host.into(),
        }
    }

    fn delete_command(keychain: &Path, host: &str) -> Command {
        let mut command = Command::new("/usr/bin/security");
        command
            .args(["delete-internet-password", "-s", host, "-a", GIT_ACCOUNT])
            .arg(keychain);
        command
    }
}

impl Drop for GitItem {
    fn drop(&mut self) {
        let output = Self::delete_command(&self.keychain, &self.host).output();
        if !matches!(&output, Ok(output) if output.status.success()) {
            eprintln!(
                "failed to delete the disposable osxkeychain fixture {}: {output:?}",
                self.host
            );
            assert!(
                std::thread::panicking(),
                "osxkeychain fixture cleanup failed"
            );
        }
    }
}

pub(super) struct GhItem {
    keychain: PathBuf,
    host: String,
    service: String,
    binary: PathBuf,
    config: PathBuf,
}

impl GhItem {
    pub(super) fn command(&self) -> String {
        // Isolate the file config and environment so the raw CLI cannot find
        // an operator token. Do not refresh the fixture token over the network.
        format!(
            "/usr/bin/env -i HOME=\"$HOME\" PATH=/usr/bin:/bin GH_CONFIG_DIR={} \
             GH_NO_UPDATE_NOTIFIER=1 GH_NO_EXTENSION_UPDATE_NOTIFIER=1 \
             {} auth token --hostname {} --no-refresh",
            path_arg(&self.config),
            path_arg(&self.binary),
            self.host,
        )
    }

    pub(super) fn report(&self, label: &str, transcript: &str) {
        let (_, output) = credentials::observed(transcript, "gh_keyring");
        eprintln!(
            "{label}: gh_keyring returned_seed={}",
            output.lines().any(|line| line == GH_TOKEN)
        );
    }
}

impl Drop for GhItem {
    fn drop(&mut self) {
        let output = Command::new("/usr/bin/security")
            .args([
                "delete-generic-password",
                "-s",
                &self.service,
                "-a",
                GH_ACCOUNT,
            ])
            .arg(&self.keychain)
            .output();
        if !matches!(&output, Ok(output) if output.status.success()) {
            eprintln!(
                "failed to delete the disposable gh keyring fixture {}: {output:?}",
                self.host
            );
            assert!(
                std::thread::panicking(),
                "gh keyring fixture cleanup failed"
            );
        }
    }
}
