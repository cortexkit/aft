use super::*;

#[cfg(unix)]
#[derive(serde::Serialize, serde::Deserialize)]
enum SavedSpawnPolicy {
    Unsandboxed,
    Host {
        shell_path: PathBuf,
    },
    Launcher {
        profile: SandboxProfile,
        launcher_path: PathBuf,
        #[cfg(target_os = "linux")]
        #[serde(default)]
        launcher_identity: Option<super::launcher_image::LauncherIdentity>,
    },
}

/// Private recovery data, never serialized into agent-readable task metadata.
/// The environment is exactly the one already in the verified private payload;
/// short-lived authorization tokens and daemon credentials are added only in
/// memory, not stored in the snapshot.
#[cfg(unix)]
#[derive(serde::Serialize, serde::Deserialize)]
struct SavedLocalLaunch {
    policy: SavedSpawnPolicy,
    root: PathBuf,
    cwd: PathBuf,
    principal: AuthenticatedPrincipal,
    shell_path: PathBuf,
    environment: Vec<(OsString, OsString)>,
    command: Vec<u8>,
    payload_digest: [u8; 32],
    linux_scope: bool,
    #[serde(default = "default_scope_max_runtime_secs")]
    linux_scope_max_runtime_secs: u64,
    // Snapshots without a stored ceiling represent originally uncapped launches.
    #[serde(default)]
    memory_limit_mib: Option<u64>,
}

#[cfg(unix)]
pub(crate) const REMOTE_LOCAL_LAUNCH: &str = "remote-local-launch";

#[cfg(unix)]
const SNAPSHOT_DIGEST_MISMATCH: &str = "local fallback snapshot digest mismatch";

#[cfg(unix)]
fn default_scope_max_runtime_secs() -> u64 {
    crate::config::DEFAULT_LINUX_SCOPE_MAX_RUNTIME_SECS
}

#[cfg(unix)]
pub(crate) fn save_local_launch(
    plan: &SpawnPlan,
    root: &Path,
    cwd: &Path,
    principal: &AuthenticatedPrincipal,
    shell: &Path,
    linux_scope: bool,
    memory_limit_mib: Option<u64>,
    linux_scope_max_runtime_secs: u64,
) -> Result<String, String> {
    let prepared = plan
        .prepared_task()
        .ok_or("local fallback payload absent")?;
    let policy = match plan.policy() {
        SpawnPlan::Unsandboxed => SavedSpawnPolicy::Unsandboxed,
        SpawnPlan::RelaxedUnenforced => {
            return Err("an unenforced Windows plan cannot be saved on Unix".into());
        }
        SpawnPlan::Host { shell_path, .. } => SavedSpawnPolicy::Host {
            shell_path: shell_path.clone(),
        },
        SpawnPlan::Launcher {
            profile,
            launcher_path,
            ..
        } => SavedSpawnPolicy::Launcher {
            profile: profile.clone(),
            launcher_path: launcher_path.clone(),
            #[cfg(target_os = "linux")]
            launcher_identity: Some(
                super::launcher_image::LauncherIdentity::capture(launcher_path)
                    .ok_or(SNAPSHOT_DIGEST_MISMATCH)?,
            ),
        },
        _ => return Err("refused local launch cannot be saved".into()),
    };
    let snapshot = SavedLocalLaunch {
        policy,
        root: root.into(),
        cwd: cwd.into(),
        principal: principal.clone(),
        shell_path: plan.host_shell_path().unwrap_or(shell).into(),
        environment: prepared
            .environment()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        command: prepared.0.command_bytes.as_ref().clone(),
        payload_digest: *prepared.0.digest.as_bytes(),
        linux_scope,
        memory_limit_mib,
        linux_scope_max_runtime_secs,
    };
    let bytes = serde_json::to_vec(&snapshot).map_err(|e| e.to_string())?;
    let mut file = crate::bash_background::persistence::create_control_file(
        &prepared.0.dirs,
        REMOTE_LOCAL_LAUNCH,
        &bytes,
    )
    .map_err(|e| e.to_string())?;
    use std::io::Write;
    file.flush().map_err(|e| e.to_string())?;
    aft_store::sync::sync_all(
        &file,
        &prepared.0.dirs.control.path().join(REMOTE_LOCAL_LAUNCH),
    )
    .map_err(|e| e.to_string())?;
    let mut hash = blake3::Hasher::new();
    hash.update(prepared.0.paths.task_id.as_bytes());
    hash.update(&bytes);
    Ok(hash.finalize().to_hex().to_string())
}

/// Reopen the frozen launch policy and verify the original payload. A missing
/// or changed snapshot refuses recovery; current configuration is never used.
#[cfg(unix)]
pub(crate) fn restore_local_launch(
    task: &crate::bash_background::persistence::ResolvedTask,
    expected: &str,
) -> Result<
    (
        SpawnPlan,
        PathBuf,
        HashMap<String, String>,
        bool,
        Option<u64>,
        u64,
    ),
    String,
> {
    let mut file =
        crate::bash_background::persistence::open_control_file(task, REMOTE_LOCAL_LAUNCH)
            .map_err(|e| e.to_string())?;
    let bytes = read_held_payload(&mut file)?;
    let mut hash = blake3::Hasher::new();
    hash.update(task.paths.task_id.as_bytes());
    hash.update(&bytes);
    if hash.finalize().to_hex().as_str() != expected {
        return Err(SNAPSHOT_DIGEST_MISMATCH.into());
    }
    let snapshot: SavedLocalLaunch = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let environment: ChildEnvironment = snapshot.environment.into_iter().collect();
    let plan = match snapshot.policy {
        SavedSpawnPolicy::Unsandboxed => SpawnPlan::Unsandboxed,
        SavedSpawnPolicy::Host { shell_path } => SpawnPlan::Host {
            shell_path,
            environment: environment.clone(),
        },
        #[cfg(target_os = "linux")]
        SavedSpawnPolicy::Launcher {
            profile,
            launcher_path,
            launcher_identity,
        } => {
            super::launcher_image::LauncherIdentity::validate_saved(
                launcher_identity,
                &launcher_path,
            )
            .map_err(|reason| {
                log::warn!(
                    "refusing Linux local fallback for {}: {reason}",
                    task.paths.task_id
                );
                SNAPSHOT_DIGEST_MISMATCH
            })?;
            SpawnPlan::Launcher {
                profile,
                launcher_path,
                task_launcher: None,
            }
        }
        #[cfg(not(target_os = "linux"))]
        SavedSpawnPolicy::Launcher {
            profile,
            launcher_path,
        } => SpawnPlan::Launcher {
            profile,
            launcher_path,
            task_launcher: None,
        },
    };
    let prepared = verify_payload(
        crate::bash_background::persistence::ResolvedTask {
            paths: task.paths.clone(),
            dirs: task.dirs.clone(),
        },
        &snapshot.command,
        &snapshot.root,
        &snapshot.cwd,
        &snapshot.principal,
        &snapshot.shell_path,
        &environment,
        Some(blake3::Hash::from_bytes(snapshot.payload_digest)),
        false,
    )?;
    let env = environment
        .into_iter()
        .map(|(k, v)| {
            Ok((
                k.into_string()
                    .map_err(|_| "non-Unicode fallback environment key")?,
                v.into_string()
                    .map_err(|_| "non-Unicode fallback environment value")?,
            ))
        })
        .collect::<Result<HashMap<_, _>, String>>()?;
    Ok((
        plan.with_prepared_task(prepared),
        snapshot.shell_path,
        env,
        snapshot.linux_scope,
        snapshot.memory_limit_mib,
        snapshot.linux_scope_max_runtime_secs,
    ))
}

#[cfg(all(test, target_os = "linux"))]
#[path = "local_launch_pin_tests.rs"]
mod pin_tests;
