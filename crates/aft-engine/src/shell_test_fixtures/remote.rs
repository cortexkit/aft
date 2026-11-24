use crate::bash_background::persistence::{read_task, write_task, TaskPaths};
pub use aft_shell::bash_background::registry::test_support::remote::*;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

pub fn runon_restart_context(project: &Path, storage: &Path) -> Arc<crate::context::AppContext> {
    Arc::new(crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.into()),
            storage_dir: Some(storage.into()),
            harness: Some(crate::harness::Harness::Runner),
            experimental_bash_background: true,
            ..Default::default()
        },
    ))
}

pub fn restarted_context_with_runon(dir: &Path, enabled: bool) -> crate::context::AppContext {
    ensure_git_fixture(dir);
    // A fresh configuration would choose ordinary unsandboxed inheritance;
    // the saved Host plan must still clear ambient HOME before its command.
    crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(dir.into()),
            remote_exec: crate::config::RemoteExecConfig {
                enabled,
                ..Default::default()
            },
            sandbox: crate::config::SandboxConfig {
                ..Default::default()
            },
            ..Default::default()
        },
    )
}

#[cfg(unix)]
pub fn remote_test_context(root: &Path) -> crate::context::AppContext {
    ensure_git_fixture(root);
    restarted_context(root)
}

#[cfg(unix)]
pub fn remote_test_context_with_runon(root: &Path, enabled: bool) -> crate::context::AppContext {
    ensure_git_fixture(root);
    restarted_context_with_runon(root, enabled)
}

pub fn remote_job_id_context(dir: &Path) -> Arc<crate::context::AppContext> {
    ensure_git_fixture(dir);
    Arc::new(crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(dir.into()),
            storage_dir: Some(dir.into()),
            ..Default::default()
        },
    ))
}

pub fn grant_sibling_read(
    ctx: &crate::context::AppContext,
    sibling: &Path,
) -> (tempfile::TempDir, crate::access_policy::ScopeGuard) {
    use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};
    use crate::db::access_grants::{self as grants, ScopeKey};

    assert!(ctx
        .validate_read_path_without_approval(
            "sibling-read",
            "session",
            &sibling.canonicalize().unwrap()
        )
        .is_err());
    let storage = tempfile::tempdir().unwrap();
    let conn = crate::db::open(&storage.path().join("grants.db")).unwrap();
    let scope = ScopeKey {
        owner: "reserved:prefrontal-core".into(),
        scope_ref: "runon-siblings".into(),
        epoch: 1,
    };
    grants::register_scope(&conn, &scope, None, &ctx.config()).unwrap();
    grants::insert(
        &conn,
        &scope,
        &AccessGrant {
            path: sibling.to_path_buf(),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        },
    )
    .unwrap();
    ctx.set_db(std::sync::Arc::new(std::sync::Mutex::new(conn)));
    (storage, crate::access_policy::scope_guard(Some(scope)))
}

pub fn restarted_context(dir: &Path) -> crate::context::AppContext {
    restarted_context_with_runon(dir, true)
}

pub fn delivered_unknown_restart_fixture(
    dir: &Path,
    connection: PathBuf,
    expired: bool,
) -> (String, TaskPaths, crate::context::AppContext) {
    let storage = dir.join("store");
    let namespace = storage.join("opencode");
    let project = dir.join("project");
    fs::create_dir_all(&namespace).unwrap();
    fs::create_dir_all(&project).unwrap();
    let (registry, task, paths) =
        finished_remote_for_cancel(&namespace, connection, cancel_unknown_terminal());
    registry.detach();
    drop(registry);
    let mut metadata = read_task(&paths.json).unwrap();
    metadata.project_root = Some(project.clone());
    metadata.workdir = project.clone();
    metadata.harness = Some("opencode".into());
    metadata.scope.as_mut().unwrap().scope_epoch = 9;
    metadata.completion_delivered = true;
    if expired {
        metadata.started_at = 0;
    }
    write_task(&paths.json, &metadata).unwrap();
    let ctx = crate::context::AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(project.clone()),
            storage_dir: Some(storage),
            harness: Some(crate::harness::Harness::Opencode),
            ..Default::default()
        },
    );
    ctx.bash_background()
        .set_harness(crate::harness::Harness::Opencode);
    ctx.bash_background()
        .replay_session_for_project(&namespace, "session", &project)
        .unwrap();
    assert!(
        !ctx.bash_background().holds_task(&task),
        "project replay must skip the delivered terminal"
    );
    (task, paths, ctx)
}

/// Start an accepted remote task in a child process and leave its record for replay.
pub async fn runon_restart_reattach_drain_child() {
    let Some(root) = std::env::var_os("AFT_RUNON_RESTART_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let project = root.join("project");
    let ctx = runon_restart_context(&project, &root.join("storage"));
    ctx.bash_background()
        .set_harness(crate::harness::Harness::Runner);
    let storage = crate::bash_background::task_storage_dir(&ctx);
    let connection = PathBuf::from(std::env::var_os("AFT_RUNON_RESTART_CONNECTION").unwrap());
    let task_id =
        runon_restart_accept_fixture(ctx.bash_background(), project, storage, connection).await;
    fs::write(root.join("task-id"), task_id).unwrap();
}
