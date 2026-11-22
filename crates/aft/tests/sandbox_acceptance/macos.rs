use super::{checked, credentials, path_arg, ACCEPTANCE_KEYCHAIN, GH_BINARY, REQUIRE_OSXKEYCHAIN};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::fmt::Write;
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
const SECURITY: &str = "/usr/bin/security";
const GIT_ITEM_ATTRIBUTES: [&str; 6] = ["-a", GIT_ACCOUNT, "-r", "htps", "-t", "dflt"];

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
            // Keep the search list visible without changing the fixture daemon's HOME.
            let account_home = aft_base::worker_jail_canary::account_home()
                .expect("resolve the Mac mini runner's account home from the password database");
            let helper = credentials::git_helper("osxkeychain");
            let copy = fixture.lookup(
                "git_osxkeychain",
                helper.as_path(),
                format!(
                    "printf '%s\\n' protocol=https host={host} username={GIT_ACCOUNT} '' | HOME={} @TOOL@ get",
                    path_arg(&account_home)
                ),
                format!("password={INTERNET}"),
            );
            keychain.git_item = Some(GitItem::seed(&host, account_home, &helper, &copy));
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

    pub(super) fn off_diagnostics(&self, gh: Option<&GhItem>) -> String {
        let Some(item) = &self.git_item else {
            return String::new();
        };
        let home = path_arg(&item.account_home);
        let keychain = path_arg(&item.keychain);
        let host = &item.host;
        let mut script = format!(
            "\nobserve ci_keychain_search_list /usr/bin/env HOME={home} {SECURITY} list-keychains -d user\n\
             observe ci_keychain_implicit_search_list /usr/bin/env HOME={home} {SECURITY} list-keychains\n\
             observe ci_git_item /usr/bin/env HOME={home} {SECURITY} find-internet-password -s {host} -a {GIT_ACCOUNT} -w {keychain}\n"
        );
        if let Some(gh) = gh {
            let service = &gh.service;
            writeln!(script, "observe ci_gh_item /usr/bin/env HOME={home} {SECURITY} find-generic-password -s {service} -a {GH_ACCOUNT} -w {keychain}").unwrap();
        }
        script
    }

    pub(super) fn report_off(&self, label: &str, run: &str, gh: Option<&GhItem>) {
        if self.git_item.is_none() {
            return;
        }
        let git_seed = format!("password={INTERNET}");
        let mut git_seed_found = true;
        for kind in ["direct", "child", "copied"] {
            git_seed_found &=
                report_fixture(label, run, &format!("git_osxkeychain_{kind}"), &git_seed);
        }
        let gh_seed_found = gh.is_none_or(|_| report_fixture(label, run, "gh_keyring", GH_TOKEN));
        if !git_seed_found || !gh_seed_found {
            for (name, args) in [
                ("ci_keychain_search_list", " -d user"),
                ("ci_keychain_implicit_search_list", ""),
            ] {
                let (status, output) = credentials::observed(run, name);
                eprintln!("{label}: security list-keychains{args} (exit {status}):\n{output}");
            }
            report_fixture(label, run, "ci_git_item", INTERNET);
            if gh.is_some() {
                report_fixture(label, run, "ci_gh_item", &gh_keychain_value());
            }
        }
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
        std::fs::write(config.join("config.yml"), "version: 1\n").unwrap();
        let service = format!("gh:{}", fixture.host);
        let value = gh_keychain_value();
        checked(
            Command::new(SECURITY)
                .args([
                    "add-generic-password",
                    "-s",
                    &service,
                    "-a",
                    GH_ACCOUNT,
                    "-w",
                    &value,
                    "-T",
                    SECURITY,
                    "-T",
                ])
                .arg(&binary)
                .arg(&fixture.keychain),
        );
        Some(GhItem {
            keychain: fixture.keychain.clone(),
            host: fixture.host.clone(),
            service,
            binary: binary.into(),
            config,
            account_home: fixture.account_home.clone(),
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
    account_home: PathBuf,
}

impl GitItem {
    fn seed(host: &str, account_home: PathBuf, helper: &Path, copy: &Path) -> Self {
        let keychain = std::env::var_os(ACCEPTANCE_KEYCHAIN).map(PathBuf::from);
        let expected = Path::new("/Users/ghrunner/Library/Keychains/ci.keychain-db");
        if keychain
            .as_deref()
            .is_none_or(|path| path != expected || !path.is_file())
        {
            let (stdout, stderr) = match Command::new("/usr/bin/security")
                .args(["default-keychain", "-d", "user"])
                .output()
            {
                Ok(output) => (
                    String::from_utf8_lossy(&output.stdout).into_owned(),
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ),
                Err(error) => (String::new(), format!("cannot run security: {error}")),
            };
            panic!(
                "osxkeychain acceptance requires {expected:?}; {ACCEPTANCE_KEYCHAIN}={keychain:?}, \
                 security default-keychain -d user stdout={stdout:?} stderr={stderr:?}, HOME={:?}",
                std::env::var_os("HOME")
            );
        }
        let keychain = keychain.unwrap();

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
            Command::new(SECURITY)
                .args(["add-internet-password", "-s", host, "-w", INTERNET])
                .args(GIT_ITEM_ATTRIBUTES)
                .arg("-T")
                .arg(helper)
                .arg("-T")
                .arg(copy)
                .arg(&keychain),
        );
        Self {
            keychain,
            host: host.into(),
            account_home,
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
    account_home: PathBuf,
}

impl GhItem {
    pub(super) fn command(&self) -> String {
        // Isolate the file config and environment so the raw CLI cannot find
        // an operator token. Do not refresh the fixture token over the network.
        format!(
            "/usr/bin/env -i HOME={} PATH=/usr/bin:/bin GH_CONFIG_DIR={} \
             GH_NO_UPDATE_NOTIFIER=1 GH_NO_EXTENSION_UPDATE_NOTIFIER=1 \
             {} auth token --hostname {} --no-refresh",
            path_arg(&self.account_home),
            path_arg(&self.config),
            path_arg(&self.binary),
            self.host,
        )
    }

    pub(super) fn report(&self, label: &str, transcript: &str) {
        report_fixture(label, transcript, "gh_keyring", GH_TOKEN);
    }
}

fn gh_keychain_value() -> String {
    // Match the value gh's Darwin keyring backend writes through security.
    format!("go-keyring-base64:{}", STANDARD.encode(GH_TOKEN))
}

fn report_fixture(label: &str, transcript: &str, name: &str, seed: &str) -> bool {
    let (status, output) = credentials::observed(transcript, name);
    let found = output.lines().any(|line| line == seed);
    let result = if found {
        "matched disposable seed"
    } else if output.is_empty() {
        "empty output"
    } else {
        "nonempty output, did not match seed"
    };
    eprintln!("{label}: {name} exit={status}; returned_seed={found}; {result}");
    status == 0 && found
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
