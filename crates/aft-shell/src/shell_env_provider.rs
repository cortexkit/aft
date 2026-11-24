//! User-selected, sandboxed per-project environment exports for agent shells.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::config::{ShellEnvProvider, ShellEnvProviderKind};
use crate::sandbox_spawn::SpawnPlan;

const OUTPUT_LIMIT: usize = 256 * 1024;

type Export = BTreeMap<String, Option<String>>;

pub(crate) const REMOVALS_ENV: &str = "AFT_INTERNAL_SHELL_ENV_REMOVALS";

/// The launch channel carries only string overrides. Encode removals in one
/// server-owned control and consume it before building a child environment or
/// saving a payload; it must never become a child variable or replay instruction.
pub(crate) fn removals(environment: &HashMap<String, String>) -> Vec<String> {
    environment
        .get(REMOVALS_ENV)
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|name| {
            #[cfg(windows)]
            {
                !environment.keys().any(|key| key.eq_ignore_ascii_case(name))
            }
            #[cfg(not(windows))]
            {
                !environment.contains_key(name)
            }
        })
        .collect()
}

fn encode_removals(environment: &mut HashMap<String, String>, names: Vec<String>) {
    if !names.is_empty() {
        environment.insert(REMOVALS_ENV.into(), serde_json::to_string(&names).unwrap());
    }
}

#[doc(hidden)]
pub fn clear_request_control(environment: &mut HashMap<String, String>) {
    environment.retain(|key, _| !key.eq_ignore_ascii_case(REMOVALS_ENV));
}

pub(crate) fn apply_removals_to_snapshot(
    environment: &mut BTreeMap<OsString, OsString>,
    request: &HashMap<String, String>,
) {
    environment.remove(OsStr::new(REMOVALS_ENV));
    for name in removals(request) {
        #[cfg(windows)]
        environment.retain(|key, _| !key.to_string_lossy().eq_ignore_ascii_case(&name));
        #[cfg(not(windows))]
        environment.remove(OsStr::new(&name));
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FileStamp {
    path: PathBuf,
    metadata: Option<(u64, Option<std::time::SystemTime>)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    provider: ShellEnvProvider,
    workdir: PathBuf,
    base: [u8; 32],
    watched: Vec<FileStamp>,
}

#[derive(Default)]
struct Cache(VecDeque<(CacheKey, Export)>);

impl Cache {
    fn get(&mut self, key: &CacheKey) -> Option<Export> {
        let index = self.0.iter().position(|(candidate, _)| candidate == key)?;
        let entry = self.0.remove(index)?;
        let result = entry.1.clone();
        self.0.push_back(entry);
        Some(result)
    }

    fn insert(&mut self, key: CacheKey, export: Export) {
        if let Some(index) = self.0.iter().position(|(candidate, _)| *candidate == key) {
            self.0.remove(index);
        }
        if self.0.len() == 64 {
            self.0.pop_front();
        }
        self.0.push_back((key, export));
    }
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn stamp(path: PathBuf) -> FileStamp {
    FileStamp {
        metadata: std::fs::metadata(&path)
            .ok()
            .map(|m| (m.len(), m.modified().ok())),
        path,
    }
}

fn watched_files(
    provider: &ShellEnvProvider,
    cwd: &Path,
    base: &BTreeMap<String, String>,
) -> Vec<FileStamp> {
    let names: Vec<&str> = match provider.kind {
        ShellEnvProviderKind::Mise => vec![
            "mise.toml",
            ".mise.toml",
            "mise.local.toml",
            ".mise.local.toml",
            ".tool-versions",
            ".config/mise.toml",
            ".config/mise/config.toml",
            ".mise/config.toml",
            "mise/config.toml",
        ],
        ShellEnvProviderKind::Direnv => vec![".envrc", ".env"],
        ShellEnvProviderKind::Custom => provider.watch.iter().map(String::as_str).collect(),
    };
    let mut files = Vec::new();
    for parent in cwd.ancestors() {
        files.extend(names.iter().map(|name| stamp(parent.join(name))));
    }
    let home = base
        .get("HOME")
        .or_else(|| base.get("USERPROFILE"))
        .map(PathBuf::from);
    let directory = |key: &str, fallback: &str| {
        base.get(key)
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(fallback)))
    };
    let trust = match provider.kind {
        ShellEnvProviderKind::Mise => {
            if let Some(home) = &home {
                files.push(stamp(home.join(".config/mise/config.toml")));
            }
            let config = base
                .get("MISE_CONFIG_DIR")
                .map(PathBuf::from)
                .or_else(|| directory("XDG_CONFIG_HOME", ".config").map(|p| p.join("mise")));
            if let Some(config) = config {
                files.push(stamp(config.join("config.toml")));
            }
            base.get("MISE_STATE_DIR")
                .map(PathBuf::from)
                .or_else(|| directory("XDG_STATE_HOME", ".local/state").map(|p| p.join("mise")))
                .map(|p| p.join("trusted-configs"))
        }
        ShellEnvProviderKind::Direnv => {
            directory("XDG_DATA_HOME", ".local/share").map(|p| p.join("direnv/allow"))
        }
        ShellEnvProviderKind::Custom => None,
    };
    if let Some(trust) = trust {
        files.push(stamp(trust.clone()));
        if let Ok(entries) = std::fs::read_dir(trust) {
            for entry in entries.flatten() {
                files.push(stamp(entry.path()));
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files.dedup_by(|a, b| a.path == b.path);
    files
}

fn cache_key(
    provider: &ShellEnvProvider,
    cwd: &Path,
    base: &BTreeMap<OsString, OsString>,
) -> Option<CacheKey> {
    if provider.kind == ShellEnvProviderKind::Custom && provider.watch.is_empty() {
        return None;
    }
    let workdir = std::fs::canonicalize(cwd).ok()?;
    let mut hash = blake3::Hasher::new();
    for (name, value) in base {
        for bytes in [name.as_encoded_bytes(), value.as_encoded_bytes()] {
            hash.update(&(bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
    let paths = base
        .iter()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.to_string_lossy().into_owned(),
            )
        })
        .collect();
    Some(CacheKey {
        provider: provider.clone(),
        watched: watched_files(provider, &workdir, &paths),
        workdir,
        base: *hash.finalize().as_bytes(),
    })
}

pub(crate) fn apply(
    provider: Option<&ShellEnvProvider>,
    plan: &mut SpawnPlan,
    cwd: &Path,
    environment: &mut HashMap<String, String>,
    remote: bool,
) -> Option<String> {
    clear_request_control(environment);
    let provider = provider.filter(|_| !remote)?;
    let base = base_environment(plan, environment);
    let key = cache_key(provider, cwd, &base);
    let hit = key
        .as_ref()
        .and_then(|key| cache().lock().unwrap_or_else(|e| e.into_inner()).get(key));
    let export = match hit
        .map(Ok)
        .unwrap_or_else(|| run(provider, plan, cwd, environment))
    {
        Ok(export) => export,
        Err(reason) => return Some(failure_line(&reason)),
    };
    if let Some(key) = key.filter(|_| cacheable_export(&export, plan.temp_dir())) {
        cache()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, export.clone());
    }
    let mut removed = Vec::new();
    for (name, value) in export {
        #[cfg(windows)]
        let name = if name.eq_ignore_ascii_case("PATH") {
            "PATH".to_owned()
        } else {
            name
        };
        #[cfg(windows)]
        environment.retain(|key, _| !key.eq_ignore_ascii_case(&name));
        match value {
            Some(value) => {
                environment.insert(name, value);
            }
            None => {
                environment.remove(&name);
                removed.push(name);
            }
        }
    }
    encode_removals(environment, removed);
    crate::sandbox_spawn::allow_provider_path(plan, environment.get("PATH").map(String::as_str));
    None
}

fn base_environment(
    plan: &SpawnPlan,
    overrides: &HashMap<String, String>,
) -> BTreeMap<OsString, OsString> {
    let mut base = crate::sandbox_spawn::provider_base_environment(plan, overrides);
    if let Some(private_temp) = plan.temp_dir() {
        // Each sandbox receives a fresh private temp directory. Hash the base
        // before installing its temp variables or prepending macOS's private
        // mktemp executable directory to PATH, so identical calls share a key.
        for name in ["TMPDIR", "TEMP", "TMP"] {
            base.remove(OsStr::new(name));
            if let Some(value) = std::env::var_os(name) {
                base.insert(name.into(), value);
            }
        }
        if let Some(path) = base.get(OsStr::new("PATH")) {
            if let Ok(path) = std::env::join_paths(
                std::env::split_paths(path).filter(|entry| !entry.starts_with(private_temp)),
            ) {
                base.insert("PATH".into(), path);
            }
        }
    }
    base
}

fn cacheable_export(export: &Export, private_temp: Option<&Path>) -> bool {
    let Some(temp) = private_temp.and_then(Path::to_str) else {
        return true;
    };
    !export.values().flatten().any(|value| {
        #[cfg(windows)]
        {
            value
                .replace('\\', "/")
                .to_ascii_lowercase()
                .contains(&temp.replace('\\', "/").to_ascii_lowercase())
        }
        #[cfg(not(windows))]
        {
            value.contains(temp)
        }
    })
}

fn run(
    provider: &ShellEnvProvider,
    plan: &SpawnPlan,
    cwd: &Path,
    overrides: &HashMap<String, String>,
) -> Result<Export, Failure> {
    let argv = provider.argv();
    let program = OsStr::new(&argv[0]);
    let args: Vec<OsString> = argv[1..].iter().map(OsString::from).collect();
    #[cfg(unix)]
    let command =
        crate::sandbox_spawn::probe_command_for_plan(plan, program, &args, cwd, overrides)
            .map_err(Failure::Spawn)?;
    #[cfg(not(unix))]
    let command = {
        let mut command = crate::effective_path::new_command(program).build();
        command.args(args).current_dir(cwd);
        crate::agent_child_env::apply_to_command(&mut command, overrides);
        crate::sandbox_spawn::apply_sandbox_environment(plan, &mut command, overrides);
        command
    };
    // Startup admission wins over the configured provider timeout: reserve one
    // second to create the fallback shell before a caller is told nothing ran.
    let timeout_ms = effective_timeout_ms(
        provider.timeout_ms,
        crate::bash_background::provider_startup_remaining(),
    );
    if timeout_ms < 100 {
        return Err(Failure::Timeout(timeout_ms));
    }
    let result = crate::format::capture_bounded_command(
        command,
        &argv[0],
        std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms),
        || false,
        OUTPUT_LIMIT + 1,
        // Unix probes already start a new session. setpgid after setsid would
        // refuse the child with EPERM; both forms give tree cleanup its own group.
        cfg!(not(unix)),
    )
    .map_err(|error| match error {
        crate::format::FormatError::Timeout { .. } => Failure::Timeout(timeout_ms),
        crate::format::FormatError::Failed { stderr, .. } => Failure::Spawn(stderr),
        other => Failure::Spawn(other.to_string()),
    })?;
    if !result.status.success() {
        let stderr = result.stderr.to_lowercase();
        if provider.kind == ShellEnvProviderKind::Mise && stderr.contains("not trusted") {
            return Err(Failure::MiseUntrusted);
        }
        if provider.kind == ShellEnvProviderKind::Direnv
            && (stderr.contains("is blocked") || stderr.contains("is not allowed"))
        {
            return Err(Failure::DirenvBlocked);
        }
        let first = result.stderr.lines().next().unwrap_or_default();
        let mut end = first.len().min(200);
        while !first.is_char_boundary(end) {
            end -= 1;
        }
        return Err(Failure::Exit(
            result.status.code().unwrap_or(-1),
            first[..end].into(),
        ));
    }
    if result.truncated {
        return Err(Failure::InvalidOutput("over 256 KiB".into()));
    }
    if !result.stdout_utf8_valid {
        return Err(Failure::InvalidOutput("not a JSON object".into()));
    }
    parse_export(result.stdout.as_bytes())
}

fn effective_timeout_ms(configured: u64, remaining: Option<std::time::Duration>) -> u64 {
    remaining.map_or(configured, |remaining| {
        configured.min(
            remaining
                .saturating_sub(std::time::Duration::from_millis(1000))
                .as_millis()
                .min(u64::MAX as u128) as u64,
        )
    })
}

#[cfg(unix)]
/// Owns scratch and control files referenced by a frozen host approval or its
/// running task. Ownership stays in memory; restart cleanup checks the recorded
/// daemon pid and OS start time before reclaiming orphaned files.
#[derive(Debug)]
#[doc(hidden)]
pub struct PreflightLayout(PathBuf);

#[cfg(unix)]
impl PreflightLayout {
    pub(crate) fn create() -> std::io::Result<std::sync::Arc<Self>> {
        crate::sandbox_spawn::create_provider_temp_layout()
            .map(|path| std::sync::Arc::new(Self(path)))
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(unix)]
impl Drop for PreflightLayout {
    fn drop(&mut self) {
        let _ = crate::sandbox_spawn::remove_provider_temp_layout(&self.0);
    }
}

#[cfg(unix)]
pub(crate) fn sweep_preflight_layouts_on_startup() {
    static SWEPT: OnceLock<()> = OnceLock::new();
    SWEPT.get_or_init(|| {
        let _ = crate::sandbox_spawn::sweep_provider_temp_layouts();
    });
}

#[cfg(unix)]
#[doc(hidden)]
pub fn host_preflight(
    ctx: &dyn crate::sandbox_spawn::SandboxHost,
    cwd: &Path,
    environment: &mut HashMap<String, String>,
) -> (Option<String>, Option<std::sync::Arc<PreflightLayout>>) {
    let config = ctx.config();
    let Some(provider) = config.shell.env_provider.as_ref() else {
        return (None, None);
    };
    if let Err(error) = crate::agent_child_env::maintain(
        config.as_ref(),
        &crate::bash_background::storage_dir(config.storage_dir.as_deref()),
    ) {
        return (Some(failure_line(&Failure::Spawn(error))), None);
    }
    let layout = match PreflightLayout::create() {
        Ok(layout) => layout,
        Err(error) => return (Some(failure_line(&Failure::Spawn(error.to_string()))), None),
    };
    let task = match crate::bash_background::persistence::allocate_task_layout(
        layout.path(),
        "env-provider",
    ) {
        Ok(task) => task,
        Err(error) => return (Some(failure_line(&Failure::Spawn(error.to_string()))), None),
    };
    let mut plan = crate::sandbox_spawn::resolve_sandbox_spawn(
        ctx,
        &crate::sandbox_spawn::current_authenticated_principal(),
        crate::sandbox_spawn::RequestedSandboxTier::Native,
        crate::sandbox_spawn::SandboxTaskKind::BashForeground,
        &task.paths.io_dir,
        None,
    );
    let result = if let Some(refusal) = plan.refusal_message() {
        Some(failure_line(&Failure::Spawn(refusal.into())))
    } else {
        apply(Some(provider), &mut plan, cwd, environment, false)
    };
    let retain = plan.temp_dir().and_then(Path::to_str).is_some_and(|temp| {
        result.is_none() && environment.values().any(|value| value.contains(temp))
    });
    (result, retain.then_some(layout))
}

/// Keep agent-visible reason wording together: callers append the same fallback
/// line regardless of which provider or execution path failed.
#[derive(Debug)]
enum Failure {
    InvalidOutput(String),
    Spawn(String),
    Timeout(u64),
    MiseUntrusted,
    DirenvBlocked,
    Exit(i32, String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidOutput(category) => write!(f, "invalid provider output: {category}"),
            Self::Spawn(error) => write!(f, "could not start provider: {error}"),
            Self::Timeout(ms) => write!(f, "timed out after {} s", *ms as f64 / 1000.0),
            Self::MiseUntrusted => write!(f, "mise: config not trusted, run 'mise trust'"),
            Self::DirenvBlocked => write!(f, "direnv: .envrc is blocked, run 'direnv allow'"),
            Self::Exit(code, stderr) => write!(f, "exit {code}: {stderr}"),
        }
    }
}

fn failure_line(reason: &Failure) -> String {
    format!("Project environment not loaded ({reason}); this command used AFT's default PATH.")
}

fn valid_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn parse_export(output: &[u8]) -> Result<Export, Failure> {
    let invalid = |category: &str| Failure::InvalidOutput(category.to_owned());
    if output.len() > OUTPUT_LIMIT {
        return Err(invalid("over 256 KiB"));
    }
    let value: serde_json::Value =
        serde_json::from_slice(output).map_err(|_| invalid("not a JSON object"))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("not a JSON object"))?;
    let mut export = Export::new();
    for (name, value) in object {
        if !valid_name(name) {
            return Err(invalid("invalid variable name"));
        }
        let value = match value {
            serde_json::Value::Null => None,
            serde_json::Value::String(value) => {
                if value.contains('\0') {
                    return Err(invalid(&format!("NUL byte in value for {name}")));
                }
                Some(value.clone())
            }
            _ => return Err(invalid(&format!("non-string value for {name}"))),
        };
        if crate::bash_env::is_blocked_provider_env_key(name) {
            log::debug!("dropped project environment provider variable: {name}");
            continue;
        }
        export.insert(name.clone(), value);
    }
    Ok(export)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_env_provider_startup_timeout_keeps_spawn_headroom() {
        use std::time::Duration;
        assert_eq!(
            effective_timeout_ms(10000, Some(Duration::from_millis(5000))),
            4000
        );
        assert_eq!(
            effective_timeout_ms(2000, Some(Duration::from_millis(5000))),
            2000
        );
        assert_eq!(
            effective_timeout_ms(10000, Some(Duration::from_millis(1050))),
            50
        );
        assert_eq!(
            effective_timeout_ms(10000, Some(Duration::from_millis(900))),
            0
        );
        assert_eq!(effective_timeout_ms(2000, None), 2000);
    }

    #[test]
    fn shell_env_provider_does_not_start_with_less_than_minimum_startup_budget() {
        let project = tempfile::tempdir().unwrap();
        let provider = ShellEnvProvider {
            kind: ShellEnvProviderKind::Custom,
            command: Some(vec!["provider-must-not-start".into()]),
            timeout_ms: 10000,
            watch: vec![],
        };
        let receipt = std::sync::Arc::new(crate::bash_background::SpawnReceipt::new(
            std::time::Instant::now() + std::time::Duration::from_millis(500),
        ));
        let result = crate::bash_background::with_spawn_receipt(receipt, || {
            run(
                &provider,
                &SpawnPlan::Unsandboxed,
                project.path(),
                &HashMap::new(),
            )
        });
        assert!(matches!(result, Err(Failure::Timeout(0))));
    }

    fn fake(kind: ShellEnvProviderKind, output: &Path) -> ShellEnvProvider {
        #[cfg(unix)]
        let command = vec![
            "sh".into(),
            "-c".into(),
            "cat \"$1\"".into(),
            "provider".into(),
            output.to_string_lossy().into_owned(),
        ];
        #[cfg(windows)]
        let command = vec![
            "cmd.exe".into(),
            "/C".into(),
            // Command quotes the path argument; embedded quotes in a command
            // string would be escaped for C runtime argument parsing, which
            // cmd.exe does not use.
            "type".into(),
            output.to_string_lossy().into_owned(),
        ];
        ShellEnvProvider {
            kind,
            command: Some(command),
            timeout_ms: 2000,
            watch: vec![],
        }
    }

    #[test]
    fn shell_env_provider_fake_kinds_end_to_end() {
        let project = tempfile::tempdir().unwrap();
        let bin = project.path().join("toolchain");
        std::fs::create_dir(&bin).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let tool = bin.join("provider-node");
            std::fs::write(&tool, "#!/bin/sh\nprintf 'Node20\\n'\n").unwrap();
            std::fs::set_permissions(tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        #[cfg(windows)]
        std::fs::write(bin.join("provider-node.cmd"), "@echo Node20\r\n").unwrap();
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            crate::effective_path::effective_path(),
        ));
        let path = std::env::join_paths(paths)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let output = project.path().join("export.json");
        std::fs::write(&output, serde_json::to_vec(&serde_json::json!({"PATH":path, "GIT_AUTHOR_NAME":"Project Author", "REMOVE":null})).unwrap()).unwrap();
        let kinds = [
            ShellEnvProviderKind::Mise,
            ShellEnvProviderKind::Custom,
            ShellEnvProviderKind::Direnv,
        ];
        for kind in kinds {
            if cfg!(windows) && kind == ShellEnvProviderKind::Direnv {
                continue;
            }
            let mut env = HashMap::from([("REMOVE".into(), "base".into())]);
            let mut plan = SpawnPlan::Unsandboxed;
            assert!(apply(
                Some(&fake(kind, &output)),
                &mut plan,
                project.path(),
                &mut env,
                false
            )
            .is_none());
            let mut config = crate::config::Config::default();
            config.git.co_author = "off".into();
            crate::agent_child_env::inject(&config, project.path(), &mut env, None).unwrap();
            #[cfg(unix)]
            let mut child = {
                let mut command = std::process::Command::new("/bin/sh");
                command.args(["-c", "provider-node; printf '%s|%s|%s' \"$GIT_AUTHOR_NAME\" \"${REMOVE-unset}\" \"${AFT_INTERNAL_SHELL_ENV_REMOVALS-unset}\""]);
                command
            };
            #[cfg(windows)]
            let mut child = {
                let mut command = std::process::Command::new("cmd.exe");
                command.args(["/D", "/C", "call provider-node.cmd & echo %GIT_AUTHOR_NAME%& if defined REMOVE echo REMOVAL_LEAK& if defined AFT_INTERNAL_SHELL_ENV_REMOVALS echo MARKER_LEAK"]);
                command
            };
            child.env("REMOVE", "base");
            crate::agent_child_env::apply_to_command(&mut child, &env);
            let result = child.output().unwrap();
            assert!(result.status.success(), "{kind:?}: {:?}", result.stderr);
            let stdout = String::from_utf8(result.stdout).unwrap();
            assert!(stdout.contains("Node20"), "{kind:?}: {stdout}");
            assert!(stdout.contains("Project Author"), "{kind:?}: {stdout}");
            assert!(!stdout.contains("LEAK"), "{kind:?}: {stdout}");
            #[cfg(unix)]
            assert!(
                stdout.ends_with("Project Author|unset|unset"),
                "{kind:?}: {stdout}"
            );
        }
    }

    #[test]
    fn shell_env_provider_off_environment_golden() {
        let mut environment = HashMap::from([
            ("PATH".into(), "/daemon/node22:/usr/bin".into()),
            ("TZ".into(), "UTC".into()),
            ("GIT_AUTHOR_NAME".into(), "Original".into()),
        ]);
        let before = environment.clone();
        assert!(apply(
            None,
            &mut SpawnPlan::Unsandboxed,
            Path::new("."),
            &mut environment,
            false
        )
        .is_none());
        assert_eq!(environment, before);
        let golden =
            r#"{"GIT_AUTHOR_NAME":"Original","PATH":"/daemon/node22:/usr/bin","TZ":"UTC"}"#;
        assert_eq!(
            serde_json::to_string(&environment.into_iter().collect::<BTreeMap<_, _>>()).unwrap(),
            golden
        );
    }

    #[test]
    fn shell_env_provider_cache_parent_changes_and_uncached_custom() {
        let project = tempfile::tempdir().unwrap();
        let nested = project.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let output = project.path().join("output with spaces.json");
        std::fs::write(&output, r#"{"VALUE":"first"}"#).unwrap();
        let mut provider = fake(ShellEnvProviderKind::Custom, &output);
        provider.watch = vec!["project.env".into()];
        let run = |provider: &ShellEnvProvider| {
            let mut env = HashMap::new();
            let failure = apply(
                Some(provider),
                &mut SpawnPlan::Unsandboxed,
                &nested,
                &mut env,
                false,
            );
            assert!(failure.is_none(), "{failure:?}");
            env["VALUE"].clone()
        };
        assert_eq!(run(&provider), "first");
        std::fs::write(&output, r#"{"VALUE":"second"}"#).unwrap();
        assert_eq!(
            run(&provider),
            "first",
            "unchanged watch key reuses successful output"
        );
        std::fs::write(project.path().join("project.env"), "created").unwrap();
        assert_eq!(
            run(&provider),
            "second",
            "a previously missing parent config invalidates the key"
        );
        std::fs::write(&output, r#"{"VALUE":"third"}"#).unwrap();
        std::fs::write(project.path().join("project.env"), "changed-size").unwrap();
        assert_eq!(run(&provider), "third");
        provider.watch.clear();
        std::fs::write(&output, r#"{"VALUE":"uncached"}"#).unwrap();
        assert_eq!(run(&provider), "uncached");
    }

    #[test]
    fn shell_env_provider_lru_has_64_entries_and_tracks_base_and_config() {
        let project = tempfile::tempdir().unwrap();
        let output = project.path().join("output");
        let provider = fake(ShellEnvProviderKind::Mise, &output);
        let base = BTreeMap::from([("PATH".into(), OsString::from("base"))]);
        let first = cache_key(&provider, project.path(), &base).unwrap();
        let mut changed_base = base.clone();
        changed_base.insert("PATH".into(), "changed".into());
        assert_ne!(
            first,
            cache_key(&provider, project.path(), &changed_base).unwrap()
        );
        let mut changed_provider = provider.clone();
        changed_provider.timeout_ms = 3000;
        assert_ne!(
            first,
            cache_key(&changed_provider, project.path(), &base).unwrap()
        );
        let mut lru = Cache::default();
        for timeout in 100..164 {
            let mut key = first.clone();
            key.provider.timeout_ms = timeout;
            lru.insert(key, Export::new());
        }
        let mut oldest = first.clone();
        oldest.provider.timeout_ms = 100;
        let mut second = first.clone();
        second.provider.timeout_ms = 101;
        assert!(lru.get(&oldest).is_some());
        lru.insert(first, Export::new());
        assert_eq!(lru.0.len(), 64);
        assert!(lru.get(&oldest).is_some());
        assert!(lru.get(&second).is_none());
    }

    #[test]
    fn shell_env_provider_failures_retry_and_runon_never_runs_or_changes_env() {
        let project = tempfile::tempdir().unwrap();
        let output = project.path().join("output");
        std::fs::write(&output, "bad output").unwrap();
        let provider = fake(ShellEnvProviderKind::Mise, &output);
        let mut env = HashMap::from([("KEEP".into(), "original".into())]);
        let original = env.clone();
        assert!(apply(
            Some(&provider),
            &mut SpawnPlan::Unsandboxed,
            project.path(),
            &mut env,
            false
        )
        .unwrap()
        .contains("not a JSON object"));
        assert_eq!(env, original);
        std::fs::write(&output, r#"{"VALUE":"recovered"}"#).unwrap();
        assert!(apply(
            Some(&provider),
            &mut SpawnPlan::Unsandboxed,
            project.path(),
            &mut env,
            false
        )
        .is_none());
        assert_eq!(env["VALUE"], "recovered");
        let mut env = original.clone();
        std::fs::remove_file(&output).unwrap();
        assert!(apply(
            Some(&provider),
            &mut SpawnPlan::Unsandboxed,
            project.path(),
            &mut env,
            true
        )
        .is_none());
        assert_eq!(env, original, "runon retains the original forwarding map");
    }

    #[test]
    fn shell_env_provider_null_command_adapter_and_forged_control() {
        let project = tempfile::tempdir().unwrap();
        let output = project.path().join("output");
        std::fs::write(&output, r#"{"REMOVE":"was here","LD_PRELOAD":"bad"}"#).unwrap();
        let mut env = HashMap::from([("REMOVE".into(), "inherited override".into())]);
        std::fs::write(&output, r#"{"REMOVE":null}"#).unwrap();
        assert!(apply(
            Some(&fake(ShellEnvProviderKind::Direnv, &output)),
            &mut SpawnPlan::Unsandboxed,
            project.path(),
            &mut env,
            false
        )
        .is_none());
        assert!(!env.contains_key("REMOVE"));
        let mut command = std::process::Command::new("unused");
        command.env("REMOVE", "inherited");
        crate::agent_child_env::apply_to_command(&mut command, &env);
        assert!(command
            .get_envs()
            .any(|(key, value)| key == "REMOVE" && value.is_none()));
        assert!(command
            .get_envs()
            .any(|(key, value)| key == REMOVALS_ENV && value.is_none()));
        let mut forged = HashMap::from([(REMOVALS_ENV.into(), r#"["PATH"]"#.into())]);
        apply(
            None,
            &mut SpawnPlan::Unsandboxed,
            project.path(),
            &mut forged,
            false,
        );
        assert!(forged.is_empty());
        assert!(
            parse_export(format!(r#"{{"{REMOVALS_ENV}":"[]"}}"#).as_bytes())
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn shell_env_provider_command_pty_and_sandbox_children_consume_removals() {
        use std::io::Read;
        let mut env = HashMap::new();
        encode_removals(&mut env, vec!["PATH".into()]);
        let assert_removed = |bytes: &[u8]| {
            let text = String::from_utf8_lossy(bytes);
            assert!(!text.lines().any(|line| line.starts_with("PATH=")));
            assert!(!text.contains(REMOVALS_ENV));
        };
        for plan in [SpawnPlan::Unsandboxed, SpawnPlan::RelaxedUnenforced] {
            let mut command = std::process::Command::new("/usr/bin/env");
            crate::agent_child_env::apply_to_command(&mut command, &env);
            crate::sandbox_spawn::apply_sandbox_environment(&plan, &mut command, &env);
            let output = command.output().unwrap();
            assert!(output.status.success());
            assert_removed(&output.stdout);
        }
        let (builder, _) = crate::sandbox_spawn::pty_command_for_plan(
            &SpawnPlan::Unsandboxed,
            OsStr::new("/usr/bin/env"),
            &[],
            Path::new(""),
            Path::new("/"),
            &env,
        )
        .unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize::default())
            .unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let mut child = pair.slave.spawn_command(builder).unwrap();
        drop(pair.slave);
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => bytes.extend_from_slice(&buffer[..count]),
            }
        }
        assert!(child.wait().unwrap().success());
        assert_removed(&bytes);
        let snapshot = crate::sandbox_spawn::approved_payload_environment(&env, Path::new("/tmp"));
        assert!(!snapshot.contains_key(OsStr::new(REMOVALS_ENV)));
        assert!(!snapshot.contains_key(OsStr::new("PATH")));
        env.insert("PATH".into(), "/project/tools:/usr/bin".into());
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "printf '%s' \"$PATH\""]);
        crate::sandbox_spawn::apply_sandbox_environment(
            &SpawnPlan::RelaxedUnenforced,
            &mut command,
            &env,
        );
        assert_eq!(
            String::from_utf8(command.output().unwrap().stdout).unwrap(),
            "/project/tools:/usr/bin"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shell_env_provider_strict_grants_only_home_path_entries() {
        let home = std::fs::canonicalize(std::env::var_os("HOME").unwrap()).unwrap();
        let inside = tempfile::tempdir_in(&home).unwrap();
        let outside = Path::new("/usr/bin");
        let link = inside.path().join("escape");
        std::os::unix::fs::symlink(outside, &link).unwrap();
        let profile = crate::sandbox_profile::SandboxProfile::build(
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            inside.path().into(),
        )
        .unwrap();
        let mut plan = SpawnPlan::Launcher {
            profile,
            launcher_path: "/unused".into(),
            task_launcher: None,
        };
        let path = std::env::join_paths([inside.path(), outside, &link]).unwrap();
        crate::sandbox_spawn::allow_provider_path(&mut plan, path.to_str());
        let SpawnPlan::Launcher { profile, .. } = plan else {
            unreachable!()
        };
        assert!(profile
            .read_allow
            .contains(&inside.path().canonicalize().unwrap()));
        assert!(!profile
            .read_allow
            .contains(&outside.canonicalize().unwrap()));
        assert_eq!(profile.read_allow.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn shell_env_provider_timeout_trust_and_exit_reasons() {
        let project = tempfile::tempdir().unwrap();
        for (kind, script, reason) in [
            (
                ShellEnvProviderKind::Custom,
                "sleep 5",
                "timed out after 0.1 s",
            ),
            (
                ShellEnvProviderKind::Mise,
                "echo 'mise ERROR Config files are not trusted.' >&2; exit 1",
                "mise: config not trusted, run 'mise trust'",
            ),
            (
                ShellEnvProviderKind::Direnv,
                "echo '.envrc is blocked. Run direnv allow' >&2; exit 1",
                "direnv: .envrc is blocked, run 'direnv allow'",
            ),
            (
                ShellEnvProviderKind::Custom,
                "printf 'first stderr line\\nsecond stderr line\\n' >&2; exit 1",
                "exit 1: first stderr line",
            ),
        ] {
            let provider = ShellEnvProvider {
                kind,
                command: Some(vec!["sh".into(), "-c".into(), script.into()]),
                timeout_ms: 100,
                watch: vec![],
            };
            assert_eq!(
                run(
                    &provider,
                    &SpawnPlan::Unsandboxed,
                    project.path(),
                    &HashMap::new()
                )
                .unwrap_err()
                .to_string(),
                reason
            );
        }
        let script = format!("printf '{}\\nsecond' >&2; exit 1", "é".repeat(200));
        let provider = ShellEnvProvider {
            kind: ShellEnvProviderKind::Custom,
            command: Some(vec!["sh".into(), "-c".into(), script]),
            timeout_ms: 2000,
            watch: vec![],
        };
        assert_eq!(
            run(
                &provider,
                &SpawnPlan::Unsandboxed,
                project.path(),
                &HashMap::new()
            )
            .unwrap_err()
            .to_string(),
            format!("exit 1: {}", "é".repeat(100))
        );
    }

    #[test]
    fn shell_env_provider_failure_reason_forms() {
        let reasons = [
            (
                Failure::MiseUntrusted,
                "mise: config not trusted, run 'mise trust'",
            ),
            (
                Failure::DirenvBlocked,
                "direnv: .envrc is blocked, run 'direnv allow'",
            ),
            (Failure::Timeout(2000), "timed out after 2 s"),
            (
                Failure::Exit(1, "first stderr line".into()),
                "exit 1: first stderr line",
            ),
            (
                Failure::Spawn("not found".into()),
                "could not start provider: not found",
            ),
            (
                Failure::InvalidOutput("not a JSON object".into()),
                "invalid provider output: not a JSON object",
            ),
            (
                Failure::InvalidOutput("non-string value for FOO".into()),
                "invalid provider output: non-string value for FOO",
            ),
            (
                Failure::InvalidOutput("invalid variable name".into()),
                "invalid provider output: invalid variable name",
            ),
            (
                Failure::InvalidOutput("over 256 KiB".into()),
                "invalid provider output: over 256 KiB",
            ),
            (
                Failure::InvalidOutput("NUL byte in value for FOO".into()),
                "invalid provider output: NUL byte in value for FOO",
            ),
        ];
        for (failure, reason) in reasons {
            assert_eq!(failure.to_string(), reason);
            assert_eq!(failure_line(&failure), format!("Project environment not loaded ({reason}); this command used AFT's default PATH."));
        }
    }

    #[test]
    fn shell_env_provider_output_contract_and_shared_filter() {
        let export = parse_export(br#"{"PATH":"/tools","GIT_AUTHOR_NAME":"Author","GIT_COMMITTER_EMAIL":"a@b","REMOVE":null,"LD_PRELOAD":"bad","GIT_SSH_COMMAND":"bad","SUBC_DAEMON_PID":"bad","AFT_INTERNAL":"bad"}"#).unwrap();
        assert_eq!(
            export,
            BTreeMap::from([
                ("PATH".into(), Some("/tools".into())),
                ("GIT_AUTHOR_NAME".into(), Some("Author".into())),
                ("GIT_COMMITTER_EMAIL".into(), Some("a@b".into())),
                ("REMOVE".into(), None),
            ])
        );
        for (output, reason) in [
            ("[]", "not a JSON object"),
            ("not json", "not a JSON object"),
            (r#"{"A":true}"#, "non-string value for A"),
            (r#"{"A=B":"x"}"#, "invalid variable name"),
            (r#"{"A":"x\u0000y"}"#, "NUL byte in value for A"),
        ] {
            assert_eq!(
                parse_export(output.as_bytes()).unwrap_err().to_string(),
                format!("invalid provider output: {reason}")
            );
        }
        assert_eq!(
            parse_export(&vec![b' '; OUTPUT_LIMIT + 1])
                .unwrap_err()
                .to_string(),
            "invalid provider output: over 256 KiB"
        );
    }

    #[cfg(unix)]
    #[test]
    fn shell_env_provider_rejects_invalid_utf8_inside_json_strings() {
        let project = tempfile::tempdir().unwrap();
        let mut provider = ShellEnvProvider {
            kind: ShellEnvProviderKind::Custom,
            command: Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                r#"printf '{"VALUE":"\377"}'"#.into(),
            ]),
            timeout_ms: 2000,
            watch: vec![],
        };
        assert_eq!(
            run(
                &provider,
                &SpawnPlan::Unsandboxed,
                project.path(),
                &HashMap::new()
            )
            .unwrap_err()
            .to_string(),
            "invalid provider output: not a JSON object"
        );
        provider.command.as_mut().unwrap()[2] = "printf '{\"VALUE\":\"�\"}'".into();
        assert_eq!(
            run(
                &provider,
                &SpawnPlan::Unsandboxed,
                project.path(),
                &HashMap::new()
            )
            .unwrap()["VALUE"],
            Some("�".into())
        );
    }
}
