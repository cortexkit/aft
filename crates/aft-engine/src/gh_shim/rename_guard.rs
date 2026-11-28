use super::{executing_image, resolve_real_gh, RefusalCode};
use serde_json::Value;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHECK_BUDGET: Duration = Duration::from_secs(15);
const MAX_WORKFLOWS: usize = 128;
const MAX_TREE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_WORKFLOW_BYTES: u64 = 1024 * 1024;
const MAX_DISPLAY_PATHS: usize = 5;
const PUBLISHED_MESSAGE: &str = "{repo} publishes {count} item(s) other repositories can reference by name ({paths}). GitHub does not redirect those references after a rename, so workflows that use them would break. To rename anyway, re-run with GH_SHIM_RENAME_PUBLISHED=allow.";
const UNVERIFIED_MESSAGE: &str = "could not check whether {repo} publishes reusable workflows or actions ({reason}). A rename would silently break any workflow that references them. To rename anyway, re-run with GH_SHIM_RENAME_PUBLISHED=allow.";

#[cfg(test)]
pub(super) type TestPreflightRunner = fn(&[String]) -> Result<String, String>;

#[cfg(test)]
std::thread_local! {
    static TEST_PREFLIGHT_RUNNER: std::cell::Cell<Option<TestPreflightRunner>> = const { std::cell::Cell::new(None) };
    static TEST_PREFLIGHT_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
struct TestPreflightScope {
    previous_runner: Option<TestPreflightRunner>,
    previous_calls: usize,
}

#[cfg(test)]
impl Drop for TestPreflightScope {
    fn drop(&mut self) {
        TEST_PREFLIGHT_RUNNER.with(|runner| runner.set(self.previous_runner));
        TEST_PREFLIGHT_CALLS.with(|calls| calls.set(self.previous_calls));
    }
}

#[cfg(test)]
pub(super) fn with_test_preflight_runner<T>(
    runner: TestPreflightRunner,
    f: impl FnOnce() -> T,
) -> (T, usize) {
    let previous_runner = TEST_PREFLIGHT_RUNNER.with(|slot| slot.replace(Some(runner)));
    let previous_calls = TEST_PREFLIGHT_CALLS.with(|calls| calls.replace(0));
    let scope = TestPreflightScope {
        previous_runner,
        previous_calls,
    };
    let result = f();
    let calls = TEST_PREFLIGHT_CALLS.with(std::cell::Cell::get);
    drop(scope);
    (result, calls)
}

pub(super) struct RenameCheck {
    pub repository: Option<String>,
    pub overridden: bool,
    pub published_paths: Option<Vec<String>>,
    pub published_paths_unverified: Option<String>,
}

pub(super) fn check(
    args: &[OsString],
    repository: Option<&str>,
) -> Result<RenameCheck, (RefusalCode, String)> {
    let overridden =
        std::env::var_os("GH_SHIM_RENAME_PUBLISHED").as_deref() == Some("allow".as_ref());
    let deadline = Instant::now() + CHECK_BUDGET;
    let mut repo = repository.unwrap_or("the current repository").to_string();
    let mut audit_repository = repository.map(str::to_owned);
    let result = (|| {
        let real_gh = resolve_preflight_gh().ok_or("real gh was not found")?;
        // Let gh select the current repository (including GH_REPO and remote
        // selection), rather than guessing from a local branch or origin.
        let mut view = vec!["repo".to_string(), "view".to_string()];
        if let Some(explicit) = rename_repo_argument(args)? {
            view.push(explicit);
        }
        view.extend(["--json".into(), "nameWithOwner,defaultBranchRef,url".into()]);
        let metadata: Value = serde_json::from_str(&read_preflight_gh(
            &real_gh,
            &view,
            deadline,
            MAX_WORKFLOW_BYTES,
        )?)
        .map_err(|e| format!("invalid repository metadata: {e}"))?;
        repo = metadata["nameWithOwner"]
            .as_str()
            .filter(|name| valid_repo(name))
            .ok_or("repository metadata has no valid nameWithOwner")?
            .to_string();
        audit_repository = Some(repo.clone());
        // The selected remote can be on a different host from GH_HOST. Pin API
        // reads to gh's resolved repository so a namesake cannot certify it.
        let url = url::Url::parse(
            metadata["url"]
                .as_str()
                .ok_or("repository metadata has no URL")?,
        )
        .map_err(|e| format!("invalid repository URL: {e}"))?;
        let host = url.host_str().ok_or("repository URL has no host")?;
        let branch = metadata["defaultBranchRef"]["name"]
            .as_str()
            .filter(|branch| !branch.is_empty())
            .ok_or("repository has no default branch")?;
        let tree: Value = serde_json::from_str(&read_preflight_gh(
            &real_gh,
            &[
                "api".into(),
                format!("repos/{repo}/git/trees/{}?recursive=1", segment(branch)),
                "--hostname".into(),
                host.into(),
            ],
            deadline,
            MAX_TREE_BYTES,
        )?)
        .map_err(|e| format!("invalid recursive tree: {e}"))?;
        let (mut published, workflows) = tree_candidates(&tree)?;
        for (path, sha) in workflows {
            let yaml = read_preflight_gh(
                &real_gh,
                &[
                    "api".into(),
                    format!("repos/{repo}/git/blobs/{}", segment(&sha)),
                    "-H".into(),
                    "Accept: application/vnd.github.raw+json".into(),
                    "--hostname".into(),
                    host.into(),
                ],
                deadline,
                MAX_WORKFLOW_BYTES,
            )?;
            if workflow_call(&yaml).map_err(|reason| format!("{path}: {reason}"))? {
                published.push(path);
            }
            ensure_time(deadline)?;
        }
        published.sort();
        published.dedup();
        ensure_time(deadline)?;
        Ok::<_, String>(published)
    })();
    match result {
        Ok(paths) if paths.is_empty() || overridden => Ok(RenameCheck {
            repository: Some(repo),
            overridden,
            published_paths: Some(paths),
            published_paths_unverified: None,
        }),
        Ok(paths) => Err((
            RefusalCode::RenamePublished,
            PUBLISHED_MESSAGE
                .replace("{repo}", &repo)
                .replace("{count}", &paths.len().to_string())
                .replace(
                    "{paths}",
                    &paths
                        .iter()
                        .take(MAX_DISPLAY_PATHS)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
        )),
        Err(reason) if overridden => Ok(RenameCheck {
            repository: audit_repository,
            overridden: true,
            published_paths: None,
            published_paths_unverified: Some(reason),
        }),
        Err(reason) => Err((
            RefusalCode::RenameUnverified,
            UNVERIFIED_MESSAGE
                .replace("{repo}", &repo)
                .replace("{reason}", &reason),
        )),
    }
}

fn valid_repo(repo: &str) -> bool {
    let parts: Vec<_> = repo.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        })
}

fn rename_repo_argument(args: &[OsString]) -> Result<Option<String>, String> {
    let mut found = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let arg = arg.to_str().ok_or("non-UTF-8 rename argument")?;
        if arg == "--" {
            break;
        }
        let value = if arg == "--repo" || arg == "-R" {
            Some(
                args.next()
                    .and_then(|a| a.to_str())
                    .ok_or("missing repository flag value")?,
            )
        } else {
            arg.strip_prefix("--repo=").or_else(|| {
                arg.strip_prefix("-R")
                    .map(|v| v.strip_prefix('=').unwrap_or(v))
            })
        };
        if let Some(value) = value {
            if value.is_empty() {
                return Err("empty repository flag value".into());
            }
            found = Some(value.to_string());
        }
    }
    Ok(found)
}

fn segment(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

type Candidates = (Vec<String>, Vec<(String, String)>);

fn tree_candidates(tree: &Value) -> Result<Candidates, String> {
    if tree["truncated"].as_bool() != Some(false) {
        return Err("recursive tree is truncated or its completeness is unknown".into());
    }
    let entries = tree["tree"]
        .as_array()
        .ok_or("recursive tree has no entries array")?;
    let mut published = Vec::new();
    let mut workflows = Vec::new();
    for entry in entries {
        let path = entry["path"].as_str().ok_or("tree entry has no path")?;
        let kind = entry["type"].as_str().ok_or("tree entry has no type")?;
        if kind != "blob" {
            continue;
        }
        if matches!(path.rsplit('/').next(), Some("action.yml" | "action.yaml")) {
            published.push(path.to_string());
        }
        if path.strip_prefix(".github/workflows/").is_some_and(|name| {
            !name.contains('/') && (name.ends_with(".yml") || name.ends_with(".yaml"))
        }) {
            if workflows.len() == MAX_WORKFLOWS {
                return Err(format!(
                    "more than {MAX_WORKFLOWS} workflow files require inspection"
                ));
            }
            let sha = entry["sha"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("workflow tree entry has no blob SHA")?;
            workflows.push((path.to_string(), sha.to_string()));
        }
    }
    Ok((published, workflows))
}

fn ensure_time(deadline: Instant) -> Result<(), String> {
    if Instant::now() >= deadline {
        Err("15 s rename check deadline exceeded".into())
    } else {
        Ok(())
    }
}

pub(super) fn resolve_preflight_gh() -> Option<PathBuf> {
    #[cfg(test)]
    if TEST_PREFLIGHT_RUNNER.with(std::cell::Cell::get).is_some() {
        return Some(PathBuf::new());
    }
    super::native_guard::permission_from_process().ok()?;
    resolve_real_gh(&executing_image())
}

fn read_preflight_gh(
    real_gh: &Path,
    args: &[String],
    deadline: Instant,
    cap: u64,
) -> Result<String, String> {
    #[cfg(test)]
    if let Some(runner) = TEST_PREFLIGHT_RUNNER.with(std::cell::Cell::get) {
        TEST_PREFLIGHT_CALLS.with(|calls| calls.set(calls.get() + 1));
        return runner(args);
    }
    read_gh(real_gh, args, deadline, cap)
}

fn read_gh(real_gh: &Path, args: &[String], deadline: Instant, cap: u64) -> Result<String, String> {
    ensure_time(deadline)?;
    read_gh_after_spawn(real_gh, args, cap, || Ok(()), || deadline)
}

// The process-tree tests wait for the grandchild to be ready before starting
// their timeout budget. Production callers keep passing their existing absolute
// deadline through the closure.
fn read_gh_after_spawn(
    real_gh: &Path,
    args: &[String],
    cap: u64,
    after_spawn: impl FnOnce() -> Result<(), String>,
    deadline: impl FnOnce() -> Instant,
) -> Result<String, String> {
    let mut command = Command::new(real_gh);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::format::isolate_in_process_group(&mut command);
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not run real gh: {e}"))?;
    let result = (|| {
        after_spawn()?;
        let deadline = deadline();
        ensure_time(deadline)?;
        let (tx, rx) = std::sync::mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        for (is_stdout, pipe, cap) in [
            (true, Box::new(stdout) as Box<dyn Read + Send>, cap),
            (false, Box::new(stderr) as Box<dyn Read + Send>, 8192),
        ] {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut bytes = Vec::new();
                let read = pipe.take(cap + 1).read_to_end(&mut bytes);
                let result = read.map_err(|e| e.to_string()).and_then(|_| {
                    if bytes.len() as u64 > cap {
                        Err("gh response exceeded capture limit".into())
                    } else {
                        String::from_utf8(bytes).map_err(|e| e.to_string())
                    }
                });
                let _ = tx.send((is_stdout, result));
            });
        }
        let mut stdout = String::new();
        let mut stderr = String::new();
        for _ in 0..2 {
            let (is_stdout, result) = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| "15 s rename check deadline exceeded".to_string())?;
            if is_stdout {
                stdout = result?;
            } else {
                stderr = result?;
            }
        }
        loop {
            ensure_time(deadline)?;
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                return if status.success() {
                    Ok(stdout)
                } else {
                    Err(format!(
                        "gh {} failed ({status}): {}",
                        args.join(" "),
                        stderr.trim()
                    ))
                };
            }
            std::thread::sleep(
                Duration::from_millis(5).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    })();
    if result.is_err() {
        // A child or descendant may still hold a pipe open after gh exits.
        // Never join those readers past the shared preflight deadline.
        crate::format::kill_process_tree(&mut child);
        let _ = child.wait();
    }
    result
}

fn yaml_payload(mut node: tree_sitter::Node<'_>) -> Result<tree_sitter::Node<'_>, String> {
    while matches!(
        node.kind(),
        "document" | "block_node" | "flow_node" | "block_sequence_item"
    ) {
        let mut cursor = node.walk();
        let children: Vec<_> = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() != "comment")
            .collect();
        if children.len() != 1 {
            return Err("YAML trigger has unsupported tags or anchors".into());
        }
        node = children[0];
    }
    Ok(node)
}

fn yaml_scalar(node: tree_sitter::Node<'_>, source: &str) -> Result<String, String> {
    let node = yaml_payload(node)?;
    let text = node
        .utf8_text(source.as_bytes())
        .map_err(|e| e.to_string())?
        .trim();
    match node.kind() {
        "plain_scalar" if !text.contains(['\r', '\n']) => Ok(text.to_string()),
        "single_quote_scalar" if !text.contains(['\r', '\n']) => {
            Ok(text[1..text.len() - 1].replace("''", "'"))
        }
        "double_quote_scalar" => {
            serde_json::from_str(text).map_err(|_| "unsupported YAML quoted trigger".into())
        }
        _ => Err("YAML trigger is not an inspectable scalar".into()),
    }
}

fn yaml_pairs<'a>(
    node: tree_sitter::Node<'a>,
    source: &str,
) -> Result<Vec<(String, Option<tree_sitter::Node<'a>>)>, String> {
    let node = yaml_payload(node)?;
    if !matches!(node.kind(), "block_mapping" | "flow_mapping") {
        return Err("YAML workflow or trigger is not a mapping".into());
    }
    let mut cursor = node.walk();
    let mut pairs = Vec::new();
    let mut keys = std::collections::BTreeSet::new();
    for pair in node
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
    {
        let key = yaml_scalar(
            pair.child_by_field_name("key")
                .ok_or("YAML mapping has no key")?,
            source,
        )?;
        // A merge or duplicate key could change the effective trigger without
        // appearing in the direct mapping, so absence is not safe to infer.
        if key == "<<" || !keys.insert(key.clone()) {
            return Err("YAML mapping has a merge or duplicate key".into());
        }
        pairs.push((key, pair.child_by_field_name("value")));
    }
    Ok(pairs)
}

fn workflow_call(source: &str) -> Result<bool, String> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_yaml::LANGUAGE.into())
        .map_err(|e| e.to_string())?;
    let tree = parser
        .parse(source, None)
        .ok_or("YAML parse did not complete")?;
    let root = tree.root_node();
    if root.has_error() {
        return Err("unparseable YAML workflow".into());
    }
    let mut cursor = root.walk();
    let documents: Vec<_> = root
        .named_children(&mut cursor)
        .filter(|n| n.kind() != "comment")
        .collect();
    if documents.len() != 1 {
        return Err("YAML workflow must contain one document".into());
    }
    let pairs = yaml_pairs(documents[0], source)?;
    let Some((_, trigger)) = pairs.into_iter().find(|(key, _)| key == "on") else {
        return Ok(false);
    };
    let trigger = yaml_payload(trigger.ok_or("YAML on trigger has no value")?)?;
    match trigger.kind() {
        "block_mapping" | "flow_mapping" => Ok(yaml_pairs(trigger, source)?
            .iter()
            .any(|(key, _)| key == "workflow_call")),
        "block_sequence" | "flow_sequence" => {
            let mut cursor = trigger.walk();
            let mut reusable = false;
            for item in trigger
                .named_children(&mut cursor)
                .filter(|n| n.kind() != "comment")
            {
                reusable |= yaml_scalar(item, source)? == "workflow_call";
            }
            Ok(reusable)
        }
        _ => Ok(yaml_scalar(trigger, source)? == "workflow_call"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn real_gh_capture_is_bounded_even_when_descendants_hold_pipes() {
        let start = Instant::now();
        let result = read_gh(
            Path::new("/bin/sh"),
            &["-c".into(), "sleep 30 & exit 0".into()],
            start + Duration::from_secs(1),
            1024,
        );
        assert!(result.unwrap_err().contains("deadline"));
        // Generous for loaded runners, yet far below the descendant's 30 s.
        assert!(start.elapsed() < Duration::from_secs(15));
        let result = read_gh(
            Path::new("/bin/sh"),
            &["-c".into(), "printf 123456789".into()],
            Instant::now() + Duration::from_secs(2),
            8,
        );
        assert!(result.unwrap_err().contains("capture limit"));
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_a_grandchild_process() {
        use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};

        let directory = tempfile::tempdir().unwrap();
        let fifo_path = directory.path().join("grandchild-lifetime");
        let fifo_name = std::ffi::CString::new(fifo_path.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_name is a valid, NUL-terminated path in our private temp directory.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let mut lifetime = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo_path)
            .unwrap();
        // Only the grandchild opens the FIFO for writing. Its PID acknowledges
        // that it holds the write end, which exec keeps open throughout sleep.
        let script = "sh -c 'echo $$; exec sleep 30' > \"$1\" & wait";
        let start = Instant::now();
        let mut pid = String::new();
        let result = read_gh_after_spawn(
            Path::new("/bin/sh"),
            &[
                "-c".into(),
                script.into(),
                "rename-guard-test".into(),
                fifo_path.to_string_lossy().into_owned(),
            ],
            1024,
            || {
                let setup_deadline = start + Duration::from_secs(3);
                loop {
                    let mut byte = [0];
                    match lifetime.read(&mut byte) {
                        Ok(1) if byte[0] == b'\n' => return Ok(()),
                        Ok(1) => pid.push(char::from(byte[0])),
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(error) => {
                            return Err(format!("could not read grandchild PID: {error}"))
                        }
                    }
                    if Instant::now() >= setup_deadline {
                        return Err("grandchild did not open its lifetime pipe".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            },
            || Instant::now() + Duration::from_secs(3),
        );
        assert!(result.unwrap_err().contains("deadline"));
        assert!(start.elapsed() < Duration::from_secs(15));

        let pid: i32 = pid
            .parse()
            .expect("grandchild acknowledged its lifetime pipe");
        // FIFO EOF observes closure of the grandchild's write end, even while it
        // remains a zombie. This liveness observation is independent of reaping
        // and PID reuse.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match lifetime.read(&mut [0]) {
                Ok(0) => break,
                Ok(_) => panic!("unexpected data after grandchild PID"),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("could not observe grandchild exit: {error}"),
            }
            if Instant::now() >= deadline {
                // The writer is still alive; clean it up before reporting the failure.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                panic!("grandchild process {pid} survived timeout (lifetime pipe still open)");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(windows)]
    #[test]
    fn timeout_kills_a_grandchild_process() {
        let directory = tempfile::tempdir().unwrap();
        let pid_file = directory.path().join("grandchild.pid");
        let powershell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "''"));
        let script = format!(
            "$child = Start-Process -FilePath {} -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 120' -PassThru; Set-Content -LiteralPath {} -Value $child.Id; Start-Sleep -Seconds 120",
            quote(&powershell),
            quote(&pid_file),
        );
        let mut grandchild_pid = None;
        let mut timeout_started = None;
        let result =
            read_gh_after_spawn(
                &powershell,
                &[
                    "-NoProfile".into(),
                    "-NonInteractive".into(),
                    "-Command".into(),
                    script,
                ],
                1024,
                || {
                    let setup_deadline = Instant::now() + Duration::from_secs(120);
                    loop {
                        match std::fs::read_to_string(&pid_file) {
                            Ok(contents) => {
                                grandchild_pid =
                                    Some(contents.trim().parse::<u32>().map_err(|error| {
                                        format!("invalid grandchild pid: {error}")
                                    })?);
                                return Ok(());
                            }
                            Err(error)
                                if error.kind() == std::io::ErrorKind::NotFound
                                    || error.raw_os_error() == Some(32) =>
                            {
                                // PowerShell can briefly keep the pid file locked after writing it.
                            }
                            Err(error) => {
                                return Err(format!("could not read grandchild pid: {error}"));
                            }
                        }
                        if Instant::now() >= setup_deadline {
                            return Err("grandchild pid setup guard exceeded 120 seconds".into());
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                },
                || {
                    let start = Instant::now();
                    timeout_started = Some(start);
                    start + Duration::from_secs(20)
                },
            );
        assert!(
            grandchild_pid.is_some(),
            "the grandchild never recorded its pid before the setup guard: {result:?}"
        );
        let start = timeout_started.expect("timeout starts after the grandchild pid is recorded");
        assert!(result.unwrap_err().contains("deadline"));
        assert!(start.elapsed() < Duration::from_secs(35));

        let pid = grandchild_pid.unwrap();
        let query = format!(
            "if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 1 }} else {{ exit 0 }}"
        );
        let status = Command::new(&powershell)
            .args(["-NoProfile", "-NonInteractive", "-Command", &query])
            .status()
            .unwrap();
        assert!(
            status.success(),
            "grandchild process {pid} survived timeout"
        );
    }

    #[test]
    fn yaml_trigger_forms_are_parsed_not_substring_matched() {
        for source in [
            "on: workflow_call\n",
            "on: [push, workflow_call]\n",
            "'on':\n  - workflow_call\n",
            "on:\n  workflow_call:\n    inputs: {}\n",
            "{\"on\": {workflow_call: {}}}\n",
            "\"o\\u006e\": {\"workflow_call\": {}}\n",
        ] {
            assert_eq!(workflow_call(source), Ok(true), "{source}");
        }
        for source in [
            "on: push\n# workflow_call\n",
            "on: [push, pull_request]\njobs: {workflow_call: {}}\n",
            "on:\n  push:\n    branches: [workflow_call]\n",
            "name: workflow_call\non: push\n",
        ] {
            assert_eq!(workflow_call(source), Ok(false), "{source}");
        }
        for source in [
            "on: [\n",
            "on: *events\n",
            "<<: *defaults\n",
            "on: push\non: workflow_call\n",
            "on: &events [workflow_call]\n",
            "on: push\n---\non: workflow_call\n",
        ] {
            assert!(workflow_call(source).is_err(), "{source}");
        }
    }

    #[test]
    fn recursive_tree_requires_explicit_completeness_and_bounds_reads() {
        for tree in [
            serde_json::json!({"tree": []}),
            serde_json::json!({"truncated": true, "tree": []}),
        ] {
            assert!(tree_candidates(&tree).is_err());
        }
        let tree = serde_json::json!({"truncated":false, "tree": (0..=MAX_WORKFLOWS).map(|n| serde_json::json!({
            "path":format!(".github/workflows/{n}.yaml"), "type":"blob", "sha":"abc123"
        })).collect::<Vec<_>>()});
        assert!(tree_candidates(&tree).unwrap_err().contains("128"));
        let tree = serde_json::json!({"truncated":false, "tree":[
            {"path":"nested/action.yml", "type":"blob"},
            {"path":".github/workflows/nested/file.yml", "type":"blob"},
            {"path":"action.yaml", "type":"tree"}
        ]});
        assert_eq!(
            tree_candidates(&tree).unwrap(),
            (vec!["nested/action.yml".into()], vec![])
        );
    }

    #[test]
    fn rename_repository_flags_follow_gh_and_stop_at_double_dash() {
        for flag in [
            vec!["-R", "other/repo"],
            vec!["--repo=other/repo"],
            vec!["-Rother/repo"],
            vec!["-R=other/repo"],
        ] {
            let args: Vec<_> = flag.into_iter().map(OsString::from).collect();
            assert_eq!(
                rename_repo_argument(&args).unwrap().as_deref(),
                Some("other/repo")
            );
        }
        assert_eq!(
            rename_repo_argument(&["--".into(), "-Rother/repo".into()]).unwrap(),
            None
        );
        assert_eq!(segment("release/next"), "release%2Fnext");
    }
}
