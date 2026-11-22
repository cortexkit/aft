use super::{
    checked, credentials, path_arg, quote, ACCEPTANCE_KEYCHAIN, GH_BINARY, REQUIRE_OSXKEYCHAIN,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const PASSWORD: &str = "disposable-keychain-password";
const ACCOUNT: &str = "aft-acceptance";
const GENERIC: &str = "disposable-keychain-generic-item";
const INTERNET: &str = "disposable-keychain-internet-item";
const GIT_ACCOUNT: &str = "aft-sandbox-acceptance-fixture";
const GH_ACCOUNT: &str = "aft-sandbox-acceptance-gh-fixture";
const GH_TOKEN: &str = "disposable-gh-keychain-token";
const PREFIX: &str = "aft-sandbox-acceptance-";
const SECURITY: &str = "/usr/bin/security";

fn fixture_host() -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{PREFIX}{}-{stamp}.invalid", std::process::id())
}

fn account_home() -> PathBuf {
    aft_base::worker_jail_canary::account_home()
        .expect("resolve the Mac mini runner's account home from the password database")
}

fn verified_ci_keychain(account_home: &Path) -> PathBuf {
    let keychain = std::env::var_os(ACCEPTANCE_KEYCHAIN).map(PathBuf::from);
    let expected = Path::new("/Users/ghrunner/Library/Keychains/ci.keychain-db");
    if keychain
        .as_deref()
        .is_some_and(|path| path == expected && path.is_file())
    {
        return keychain.unwrap();
    }
    let (stdout, stderr) = match Command::new(SECURITY)
        .args(["default-keychain", "-d", "user"])
        .env("HOME", account_home)
        .output()
    {
        Ok(output) => (
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        ),
        Err(error) => (String::new(), format!("cannot run security: {error}")),
    };
    panic!(
        "keychain acceptance requires {expected:?}; {ACCEPTANCE_KEYCHAIN}={keychain:?}, \
         security default-keychain -d user stdout={stdout:?} stderr={stderr:?}, HOME={account_home:?}"
    );
}

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
        let host = fixture_host();
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
            let account_home = account_home();
            let helper = credentials::git_helper("osxkeychain");
            // Use separate items so the installed helper and its signed copy each authorize their own binary.
            let copy_host = format!("{}-copy.invalid", host.strip_suffix(".invalid").unwrap());
            let copy = fixture.lookup(
                "git_osxkeychain",
                helper.as_path(),
                format!(
                    "tool=@TOOL@; if [ \"$tool\" = {} ]; then host={host}; else host={copy_host}; fi; \
                     printf '%s\\n' protocol=https \"host=$host\" username={GIT_ACCOUNT} '' | HOME={} \"$tool\" get",
                    path_arg(&helper),
                    path_arg(&account_home)
                ),
                format!("password={INTERNET}"),
            );
            keychain.git_item = Some(GitItem::seed(host, account_home, helper, copy_host, copy));
        } else {
            eprintln!("git osxkeychain helper not tested: set AFT_REQUIRE_ACCEPTANCE_OSXKEYCHAIN=1 only on a runner whose processes run in a user login session");
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

    pub(super) fn off_diagnostics(&self) -> String {
        let Some(item) = &self.git_item else {
            return String::new();
        };
        let home = path_arg(&item.account_home);
        format!(
            "\nobserve ci_keychain_search_list /usr/bin/env HOME={home} {SECURITY} list-keychains -d user\n\
             observe ci_keychain_implicit_search_list /usr/bin/env HOME={home} {SECURITY} list-keychains\n"
        )
    }

    pub(super) fn report_off(&self, label: &str, run: &str) {
        if self.git_item.is_none() {
            return;
        }
        let git_seed = format!("password={INTERNET}");
        let mut git_seed_found = true;
        for kind in ["direct", "child", "copied"] {
            git_seed_found &=
                report_fixture(label, run, &format!("git_osxkeychain_{kind}"), &git_seed);
        }
        if !git_seed_found {
            for (name, args) in [
                ("ci_keychain_search_list", " -d user"),
                ("ci_keychain_implicit_search_list", ""),
            ] {
                let (status, output) = credentials::observed(run, name);
                eprintln!("{label}: security list-keychains{args} (exit {status}):\n{output}");
            }
        }
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
    account_home: PathBuf,
    entries: Vec<(PathBuf, String)>,
}

impl GitItem {
    fn seed(
        host: String,
        account_home: PathBuf,
        helper: PathBuf,
        copy_host: String,
        copy: PathBuf,
    ) -> Self {
        let keychain = verified_ci_keychain(&account_home);
        // The credential helper writes to the user's default keychain. Check it
        // with the same HOME used for store and erase before touching any item.
        let default = Command::new(SECURITY)
            .arg("default-keychain")
            .env("HOME", &account_home)
            .output()
            .expect("query the Mac mini runner's default keychain");
        let stdout = String::from_utf8_lossy(&default.stdout);
        let stderr = String::from_utf8_lossy(&default.stderr);
        assert!(
            default.status.success() && Path::new(stdout.trim().trim_matches('"')) == keychain,
            "refusing to seed outside {keychain:?}; security default-keychain stdout={stdout:?} \
             stderr={stderr:?}, HOME={account_home:?}"
        );

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

        let mut item = Self {
            account_home,
            entries: Vec::new(),
        };
        for (helper, host) in [(helper, host), (copy, copy_host)] {
            let output = item
                .credential(&helper, &host, "store", Some(INTERNET))
                .unwrap();
            assert!(
                output.status.success(),
                "could not seed {helper:?} at {host}: {output:?}"
            );
            item.entries.push((helper, host));
        }
        item
    }

    fn credential(
        &self,
        helper: &Path,
        host: &str,
        action: &str,
        password: Option<&str>,
    ) -> io::Result<Output> {
        let mut child = Command::new(helper)
            .arg(action)
            .env("HOME", &self.account_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut input = child.stdin.take().unwrap();
        write!(
            input,
            "protocol=https\nhost={host}\nusername={GIT_ACCOUNT}\n"
        )?;
        if let Some(password) = password {
            writeln!(input, "password={password}")?;
        }
        writeln!(input)?;
        drop(input);
        child.wait_with_output()
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
        let mut cleaned = true;
        for (helper, host) in &self.entries {
            let output = self.credential(helper, host, "erase", None);
            if !matches!(&output, Ok(output) if output.status.success()) {
                eprintln!("failed to erase the disposable osxkeychain fixture {host}: {output:?}");
                cleaned = false;
            }
        }
        assert!(
            cleaned || std::thread::panicking(),
            "osxkeychain fixture cleanup failed"
        );
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
    pub(super) fn seed(root: &Path) -> Option<Self> {
        let Some(binary) = std::env::var_os(GH_BINARY).filter(|path| !path.is_empty()) else {
            assert_ne!(
                std::env::var(REQUIRE_OSXKEYCHAIN).as_deref(),
                Ok("1"),
                "Mac mini acceptance requires {GH_BINARY}"
            );
            eprintln!(
                "raw gh keyring not tested: the Mac mini CI runner provisions gh for this probe"
            );
            return None;
        };
        let account_home = account_home();
        let keychain = verified_ci_keychain(&account_home);
        let host = fixture_host();
        let config = root.join("project/gh-keyring-probe");
        std::fs::create_dir(&config).unwrap();
        // Configure an active user without putting the token in the file.
        std::fs::write(
            config.join("hosts.yml"),
            format!("{host}:\n  user: {GH_ACCOUNT}\n  users:\n    {GH_ACCOUNT}: {{}}\n  git_protocol: https\n"),
        )
        .unwrap();
        std::fs::write(config.join("config.yml"), "version: 1\n").unwrap();
        let service = format!("gh:{host}");
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
                .arg(&keychain),
        );
        Some(Self {
            keychain,
            host,
            service,
            binary: binary.into(),
            config,
            account_home,
        })
    }

    pub(super) fn script(&self, root: &Path) -> String {
        let mut script = credentials::observe_script(&root.join("project/gh-probe-stderr"));
        let raw = self.command();
        script.push_str(&format!("observe gh_keyring /bin/sh -c {}\n", quote(&raw)));
        script.push_str(&self.security_probe());
        script
    }

    fn command(&self) -> String {
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

    fn security_probe(&self) -> String {
        format!(
            "\nobserve ci_gh_item /usr/bin/env HOME={} {SECURITY} find-generic-password -s {} -a {GH_ACCOUNT} -w {}\n",
            path_arg(&self.account_home), self.service, path_arg(&self.keychain)
        )
    }

    pub(super) fn report(&self, label: &str, transcript: &str) {
        report_fixture(label, transcript, "ci_gh_item", &gh_keychain_value());
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
