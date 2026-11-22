use super::{checked, path_arg, quote};
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CHANGED: &str = "changed-by-write-oracle";

struct FileCredential {
    name: &'static str,
    path: PathBuf,
    seed: String,
    identity: (u64, u64),
}

struct Lookup {
    name: &'static str,
    program: PathBuf,
    command: String,
    expected: String,
}

pub(super) struct Fixture {
    copies: PathBuf,
    files: Vec<FileCredential>,
    lookups: Vec<Lookup>,
    enclosing: PathBuf,
    moved: PathBuf,
    public: PathBuf,
    _agent: Agent,
}

impl Fixture {
    pub(super) fn new(root: &Path) -> Self {
        let project = root.join("project");
        let home = root.join("home");
        let key = home.join(".ssh/id_ed25519");
        fs::create_dir_all(key.parent().unwrap()).unwrap();
        checked(
            Command::new("/usr/bin/ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&key),
        );
        let store = home.join(".git-credentials");
        let store_seed = "https://aft-acceptance:disposable-git-store@git-probe.invalid\n";
        let public = project.join("public.txt");
        let copies = project.join("copied-bin");
        fs::create_dir_all(&copies).unwrap();
        fs::write(&public, "public-control").unwrap();
        let mut fixture = Self {
            copies,
            files: Vec::new(),
            lookups: Vec::new(),
            enclosing: project.join("kube-relative"),
            moved: project.join("kube-relocated"),
            public,
            _agent: Agent::start(&root.join("a"), &key),
        };
        for (name, path) in [
            ("gnupg", home.join(".gnupg/private.key")),
            ("netrc", home.join(".netrc")),
            ("aws_default", home.join(".aws/credentials")),
            ("aws_override", root.join("operator/aws")),
            (
                "gcloud",
                home.join(".config/gcloud/application_default_credentials.json"),
            ),
            ("azure", home.join(".azure/accessTokens.json")),
            ("gh_store", home.join(".config/gh/hosts.yml")),
            ("nested_git_store", home.join(".config/git/credentials")),
            ("docker", home.join(".docker/config.json")),
            ("npm", home.join(".npmrc")),
            ("pypi", home.join(".pypirc")),
            ("cargo", home.join(".cargo/credentials.toml")),
            ("kube_default", home.join(".kube/config")),
            ("kube_absolute", root.join("operator/kube")),
            ("kube_relative", fixture.enclosing.join("config")),
            ("configured_read_deny", root.join("private-notes")),
        ] {
            let seed = format!("disposable-{name}\n");
            fixture.file(name, path, seed.clone(), seed.trim());
        }
        fixture.file(
            "git_store_file",
            store.clone(),
            store_seed.into(),
            "disposable-git-store",
        );
        let key_seed = fs::read_to_string(&key).unwrap();
        let key_marker = key_seed.lines().nth(1).unwrap().to_owned();
        fixture.file("ssh_key", key.clone(), key_seed, &key_marker);
        fixture.lookup(
            "git_credential_store",
            git_helper("store"),
            format!(
                "printf '%s\\n' protocol=https host=git-probe.invalid '' | @TOOL@ --file {} get",
                path_arg(&store)
            ),
            "password=disposable-git-store".into(),
        );
        let fingerprint = checked(Command::new("/usr/bin/ssh-keygen").arg("-lf").arg(&key));
        let fingerprint = String::from_utf8(fingerprint.stdout).unwrap();
        let fingerprint = fingerprint.split_whitespace().nth(1).unwrap();
        fixture.lookup(
            "ssh_agent",
            "/usr/bin/ssh-add",
            format!("SSH_AUTH_SOCK={} @TOOL@ -l", path_arg(&root.join("a"))),
            fingerprint.to_owned(),
        );
        fixture
    }

    fn file(&mut self, name: &'static str, path: PathBuf, seed: String, marker: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // Do not replace a generated ssh key: the running agent already holds it.
        if !path.exists() {
            fs::write(&path, &seed).unwrap();
        }
        let identity = identity(&path);
        self.lookup(
            name,
            "/bin/cat",
            format!("@TOOL@ {}", path_arg(&path)),
            marker.to_owned(),
        );
        self.files.push(FileCredential {
            name,
            path,
            seed,
            identity,
        });
    }

    pub(super) fn lookup(
        &mut self,
        name: &'static str,
        program: impl Into<PathBuf>,
        command: String,
        expected: String,
    ) -> PathBuf {
        let program = program.into();
        let copy = self.copies.join(program.file_name().unwrap());
        if !copy.exists() {
            fs::copy(&program, &copy).unwrap_or_else(|error| panic!("copy {program:?}: {error}"));
            #[cfg(target_os = "macos")]
            // Apple system executables may be restricted to their original
            // path. Sign the disposable copy for local execution.
            checked(
                Command::new("/usr/bin/codesign")
                    .args(["--force", "--sign", "-", "--timestamp=none"])
                    .arg(&copy),
            );
        }
        self.lookups.push(Lookup {
            name,
            program,
            command,
            expected,
        });
        copy
    }

    pub(super) fn script(&self, measure_setuid: bool, gh_probe: Option<&str>) -> String {
        let stderr_path = path_arg(&self.copies.join("probe-stderr"));
        let mut script =
            format!("probe_stderr={stderr_path}\ntrap '/bin/rm -f \"$probe_stderr\"' EXIT\n");
        script.push_str(
            r#"observe() {
    name=$1; shift
    stdout=$("$@" 2>"$probe_stderr"); code=$?
    stderr=$(/bin/cat "$probe_stderr")
    # Read just the first line without printing any credential stdout.
    stderr_first=
    IFS= read -r stderr_first <"$probe_stderr" || true
    [ -n "$stdout" ] && stdout_empty=no || stdout_empty=yes
    printf 'PROBE %s: command=%s; exit=%s; stdout_empty=%s; stderr=%s\n' \
        "$name" "$*" "$code" "$stdout_empty" "${stderr_first:-<none>}"
    result=$stdout
    if [ -n "$stderr" ]; then
        if [ -n "$result" ]; then result=$(printf '%s\n%s' "$result" "$stderr"); else result=$stderr; fi
    fi
    printf 'BEGIN %s\n%s\nEND %s %s\n' "$name" "$result" "$name" "$code"
}
"#,
        );
        if measure_setuid {
            script.push_str("observe sudo /usr/bin/sudo -n /usr/bin/true\n");
            if cfg!(target_os = "macos") {
                script.push_str("observe setuid_ps /bin/ps -p \"$$\" -o pid=\n");
            } else {
                script.push_str("observe setuid_passwd /usr/bin/passwd -S\n");
            }
        }
        if let Some(command) = gh_probe {
            writeln!(script, "observe gh_keyring /bin/sh -c {}", quote(command)).unwrap();
        }
        writeln!(
            script,
            "observe copied_public {} {}",
            path_arg(&self.copies.join("cat")),
            path_arg(&self.public)
        )
        .unwrap();
        for lookup in &self.lookups {
            let direct = lookup.command.replace("@TOOL@", &path_arg(&lookup.program));
            let child = format!(
                "/bin/sh -c {}",
                quote(&format!("{direct}; code=$?; exit \"$code\""))
            );
            let copy = self.copies.join(lookup.program.file_name().unwrap());
            let copied = lookup.command.replace("@TOOL@", &path_arg(&copy));
            for (kind, command) in [("direct", direct), ("child", child), ("copied", copied)] {
                writeln!(
                    script,
                    "observe {}_{kind} /bin/sh -c {}",
                    lookup.name,
                    quote(&command)
                )
                .unwrap();
            }
        }
        for item in &self.files {
            let attempt = format!("printf %s {} > {}", quote(CHANGED), path_arg(&item.path));
            writeln!(
                script,
                "observe write_{} /bin/sh -c {}",
                item.name,
                quote(&attempt)
            )
            .unwrap();
        }
        writeln!(
            script,
            "observe rename_enclosing /bin/mv {} {}",
            path_arg(&self.enclosing),
            path_arg(&self.moved)
        )
        .unwrap();
        script
    }

    pub(super) fn assert_oracle(&mut self, label: &str, transcript: &str, enabled: bool) {
        let (copy_status, copy_output) = observed(transcript, "copied_public");
        assert_eq!(
            (copy_status, copy_output.trim()),
            (0, "public-control"),
            "{label}: a copied executable could not read an ordinary file"
        );
        for lookup in &self.lookups {
            for kind in ["direct", "child", "copied"] {
                let name = format!("{}_{kind}", lookup.name);
                let (status, output) = observed(transcript, &name);
                if enabled {
                    assert!(
                        !output.contains(&lookup.expected),
                        "{label}: {name} disclosed its seed"
                    );
                } else {
                    assert_eq!(status, 0, "{label}: {name} failed: {output}");
                    assert!(
                        output.contains(&lookup.expected),
                        "{label}: {name} did not retrieve its seed: {output}"
                    );
                }
            }
        }
        let (rename_status, rename_output) = observed(transcript, "rename_enclosing");
        if enabled {
            assert!(
                !self.moved.exists(),
                "{label}: credential's enclosing folder was renamed"
            );
        } else {
            assert_eq!(
                rename_status, 0,
                "Off must be able to rename the enclosing folder: {rename_output}"
            );
            assert!(!self.enclosing.exists() && self.moved.join("config").is_file());
            fs::rename(&self.moved, &self.enclosing).unwrap();
        }
        for item in &self.files {
            let (status, output) = observed(transcript, &format!("write_{}", item.name));
            let contents = fs::read_to_string(&item.path)
                .unwrap_or_else(|error| panic!("{label}: {}: {error}", item.name));
            assert_eq!(
                identity(&item.path),
                item.identity,
                "{label}: {} changed file identity",
                item.name
            );
            if enabled {
                assert_eq!(
                    contents, item.seed,
                    "{label}: {} changed the host file",
                    item.name
                );
            } else {
                assert_eq!(status, 0, "Off write failed for {}: {output}", item.name);
                assert_eq!(contents, CHANGED, "Off write did not touch {}", item.name);
                fs::write(&item.path, &item.seed).unwrap();
            }
        }
    }
}

fn identity(path: &Path) -> (u64, u64) {
    let metadata = fs::metadata(path).unwrap();
    (metadata.dev(), metadata.ino())
}

pub(super) fn observed<'a>(transcript: &'a str, name: &str) -> (i32, &'a str) {
    let (_, record) = transcript
        .split_once(&format!("BEGIN {name}\n"))
        .unwrap_or_else(|| panic!("{name} did not run: {transcript}"));
    let (output, end) = record
        .split_once(&format!("\nEND {name} "))
        .unwrap_or_else(|| panic!("{name} did not finish: {transcript}"));
    (end.lines().next().unwrap().parse().unwrap(), output)
}

pub(super) fn git_helper(name: &str) -> PathBuf {
    let output = checked(Command::new("/usr/bin/git").arg("--exec-path"));
    Path::new(String::from_utf8(output.stdout).unwrap().trim())
        .join(format!("git-credential-{name}"))
}

struct Agent(Child);

impl Agent {
    fn start(socket: &Path, key: &Path) -> Self {
        let mut agent = Self(
            Command::new("/usr/bin/ssh-agent")
                .args(["-D", "-a"])
                .arg(socket)
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(socket).is_err() {
            assert!(
                agent.0.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "disposable ssh-agent did not listen"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        checked(
            Command::new("/usr/bin/ssh-add")
                .env("SSH_AUTH_SOCK", socket)
                .arg(key),
        );
        agent
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
