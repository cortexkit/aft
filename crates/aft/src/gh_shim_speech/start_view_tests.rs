use super::*;
use crate::gh_shim_speech::start_view::{inject, START_VIEW_ENV};
use std::path::PathBuf;

#[tokio::test]
async fn provisioned_start_view_never_lists_root_and_same_ticket_views_are_independent() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let root = fixture._dir.path().join("view-only");
    let mut env = HashMap::from([("AFT_GH_SHIM_STATE_DIR".into(), root.display().to_string())]);
    let ticket = fixture.params["ticket"].as_str().unwrap();
    let before = crate::private_storage::root_listings_for_test();
    let first = inject(Some(ticket), &mut env, fixture._dir.path()).unwrap();
    let first_path = PathBuf::from(&env[START_VIEW_ENV]);
    let second = inject(Some(ticket), &mut env, fixture._dir.path()).unwrap();
    let second_path = PathBuf::from(&env[START_VIEW_ENV]);
    assert_eq!(crate::private_storage::root_listings_for_test(), before);
    assert_ne!(first_path, second_path);
    assert!(fs::metadata(&first_path).unwrap().len() > 0);
    assert!(fs::metadata(&second_path).unwrap().len() > 0);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&first_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(first);
    assert!(!first_path.exists());
    assert!(second_path.exists());
    fixture
        .ctx
        .bash_background()
        .kill(&fixture.task, &fixture.session)
        .unwrap();
    fixture
        .ctx
        .bash_background()
        .bind_start_view(&fixture.task, Some(second));
    assert!(!second_path.exists());
}

async fn launch(fixture: &Fixture, background: bool, refuse: bool) -> crate::protocol::Response {
    let ctx = fixture.ctx.clone();
    ctx.update_config(|config| config.experimental_bash_background = true);
    let project = ctx.config().project_root.clone().unwrap();
    let root = fixture._dir.path().join("store");
    let state = fixture._dir.path().join("shim");
    let seed = LaunchSeed {
        launch: Ok(fixture.launch.clone()),
        lifecycle: Some(fixture.head.clone()),
        session: fixture.session.clone(),
        harness: "opencode".into(),
        project_root: project.display().to_string(),
    };
    let scope = fixture.head.clone();
    let session = fixture.session.clone();
    tokio::task::spawn_blocking(move || {
        crate::gh_shim_speech::service::with_launch(Some(seed), &root, || {
            crate::bash_background::registry::with_scope_spawn(Some(scope), || {
                let plan = if refuse {
                    crate::sandbox_spawn::SpawnPlan::refused_for_test("sandbox_principal_unknown")
                } else {
                    crate::sandbox_spawn::SpawnPlan::Unsandboxed
                };
                crate::sandbox_spawn::with_spawn_plan_for_test(plan, || {
                    crate::bash_background::spawn(
                        "start-view-launch",
                        &session,
                        "printf '%s' \"$AFT_GH_SPEECH_START_VIEW\" > seen-view; while [ ! -e release-view ]; do sleep 0.02; done",
                        crate::bash_background::BashShell::Bash,
                        crate::bash_background::registry::resolve_posix_shell(),
                        Some(project),
                        Some(HashMap::from([("AFT_GH_SHIM_STATE_DIR".into(), state.display().to_string())])),
                        crate::bash_background::HardKill::After(Duration::from_secs(30)),
                        &ctx,
                        background,
                        false,
                        false,
                        false,
                        24,
                        80,
                        Vec::new(),
                        false,
                        None,
                        None,
                    )
                })
            })
        })
    }).await.unwrap()
}

async fn view_seen(fixture: &Fixture) -> PathBuf {
    let seen = fixture
        .ctx
        .config()
        .project_root
        .as_ref()
        .unwrap()
        .join("seen-view");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(path) = fs::read_to_string(&seen) {
            if !path.is_empty() {
                let path = PathBuf::from(path);
                assert!(
                    path.is_file(),
                    "view must remain readable while the command runs"
                );
                return path;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child never received a start view"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn complete(fixture: &Fixture, task: &str, view: &PathBuf) {
    let project = fixture.ctx.config().project_root.clone().unwrap();
    fs::write(project.join("release-view"), b"").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !fixture.ctx.bash_background().is_task_terminal(task) || view.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "completed command retained its view"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn foreground_start_view_is_removed_on_completion() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let response = launch(&fixture, false, false).await;
    assert!(response.success, "{response:?}");
    let task = response.data["task_id"].as_str().unwrap();
    let view = view_seen(&fixture).await;
    complete(&fixture, task, &view).await;
}

#[tokio::test]
async fn background_start_view_is_removed_on_completion() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let response = launch(&fixture, true, false).await;
    assert!(response.success, "{response:?}");
    let task = response.data["task_id"].as_str().unwrap();
    let view = view_seen(&fixture).await;
    complete(&fixture, task, &view).await;
}

#[tokio::test]
async fn killed_start_view_is_removed() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let response = launch(&fixture, true, false).await;
    assert!(response.success, "{response:?}");
    let task = response.data["task_id"].as_str().unwrap();
    let view = view_seen(&fixture).await;
    fixture
        .ctx
        .bash_background()
        .kill(task, &fixture.session)
        .unwrap();
    assert!(!view.exists());
}

#[tokio::test]
async fn scope_end_removes_start_view() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let response = launch(&fixture, true, false).await;
    assert!(response.success, "{response:?}");
    let view = view_seen(&fixture).await;
    fixture.ctx.bash_background().kill_scope(&fixture.head);
    assert!(!view.exists());
}

#[tokio::test]
async fn refused_launch_leaves_no_start_view() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    let response = launch(&fixture, true, true).await;
    assert!(!response.success);
    let root = fixture._dir.path().join("shim");
    assert!(fs::read_dir(root).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("speech-start-")
    }));
}
