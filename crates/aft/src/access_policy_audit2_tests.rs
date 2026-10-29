use super::*;
use crate::config::Config;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use serde_json::{json, Value};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
    ctx: AppContext,
    owner: std::sync::Arc<()>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        let mut config = Config {
            project_root: Some(root.clone()),
            storage_dir: Some(dir.path().join("store")),
            ..Config::default()
        };
        config.backup.enabled = Some(false);
        Self {
            _dir: dir,
            root,
            outside,
            ctx: AppContext::new(Box::new(crate::language::StubProvider), config),
            owner: std::sync::Arc::new(()),
        }
    }

    fn plugin(&self) -> AskRouteGuard {
        ask_route_guard(Some(AskRoute::Plugin {
            scope: plugin::Scope::new(&self.owner, "audit-session"),
            once: Vec::new(),
        }))
    }
}

fn request(command: &str, params: Value) -> RawRequest {
    RawRequest {
        id: "audit2".into(),
        command: command.into(),
        session_id: Some("audit-session".into()),
        lsp_hints: None,
        params,
    }
}

#[test]
fn audit2_batch_delete_authorizes_every_member_before_unlink() {
    let f = Fixture::new();
    let inside = f.root.join("inside.txt");
    let outside = f.outside.join("outside.txt");
    std::fs::write(&inside, "inside").unwrap();
    std::fs::write(&outside, "outside").unwrap();
    let _route = f.plugin();
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"files":[inside,outside]})),
        &f.ctx,
    );
    assert_eq!(
        response.data["code"], "permission_required",
        "{:?}",
        response.data
    );
    assert!(response.data["asks"][0]["grant_id"].is_string());
    assert!(
        inside.exists(),
        "an inside member must not mutate before outside approval"
    );
    assert!(outside.exists());
}

#[test]
fn audit2_hashline_preserves_structured_outside_permission() {
    use crate::hashline::integration::RegistrationRequest;
    let f = Fixture::new();
    let outside = f.outside.join("outside.txt");
    std::fs::write(&outside, "before\n").unwrap();
    assert!(
        f.ctx
            .hashline_bindings()
            .register(
                &f.root,
                "audit-session",
                RegistrationRequest {
                    configured_enabled: true,
                    edit_slot_survives: true,
                    read_slot_survives: true,
                }
            )
            .effective
    );
    let read = crate::commands::read::handle_read(
        &request(
            "read",
            json!({"file":outside,"_hashline_requested_path":outside}),
        ),
        &f.ctx,
    );
    let tag = read.data["hashline_tag"].as_str().unwrap();
    let patch = format!("[{}#{tag}]\nPUT 1:\n+after\n", outside.display());
    let _route = f.plugin();
    let response =
        crate::commands::hashline::handle_edit(&request("edit", json!({"patch":patch})), &f.ctx);
    assert_eq!(
        response.data["code"], "permission_required",
        "{:?}",
        response.data
    );
    assert!(response.data["asks"][0]["grant_id"].is_string());
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "before\n");
}

#[test]
fn audit2_every_once_route_rechecks_file_kind_for_read_and_recursive_delete() {
    for consent in [true, false] {
        for write in [false, true] {
            let f = Fixture::new();
            let target = f.outside.join("changing");
            std::fs::write(&target, "file before prompt").unwrap();
            let route = |once| {
                if consent {
                    AskRoute::Consent { once, launch: None }
                } else {
                    AskRoute::Elicitation { once }
                }
            };
            let grant: AccessGrant = {
                let _route = ask_route_guard(Some(route(Vec::new())));
                let response = if write {
                    f.ctx.validate_write_location("audit2", &target)
                } else {
                    f.ctx.validate_read_path("audit2", "audit-session", &target)
                }
                .unwrap_err();
                serde_json::from_value(response.data["ask"].clone()).unwrap()
            };
            assert_eq!(grant.kind, GrantKind::File);
            std::fs::remove_file(&target).unwrap();
            std::fs::create_dir(&target).unwrap();
            let child = target.join("child.txt");
            std::fs::write(&child, "protected descendant").unwrap();
            let _route = ask_route_guard(Some(route(vec![grant])));
            let response: Response = if write {
                crate::commands::delete_file::handle_delete_file(
                    &request("delete_file", json!({"file":target,"recursive":true})),
                    &f.ctx,
                )
            } else {
                crate::commands::read::handle_read(&request("read", json!({"file":target})), &f.ctx)
            };
            assert!(
                !response.success,
                "file approval widened on consent={consent}, write={write}: {:?}",
                response.data
            );
            assert_eq!(response.data["code"], ACCESS_ASK_REQUIRED);
            assert!(child.exists());
        }
    }
}

#[test]
fn audit2_foreign_registered_scratch_cannot_be_read_written_or_deleted() {
    use crate::bash_background::persistence::{task_paths, write_task, PersistedTask};
    use crate::bash_background::BgTaskStatus;
    let f = Fixture::new();
    let storage = f.ctx.storage_dir();
    let paths = task_paths(&storage, "foreign-session", "bash-a2f0123456789012").unwrap();
    std::fs::create_dir_all(&paths.dir).unwrap();
    #[cfg(unix)]
    let scratch = crate::sandbox_spawn::create_task_temp_dir(&paths.dir).unwrap();
    #[cfg(not(unix))]
    let scratch_dir = tempfile::Builder::new()
        .prefix(".sandbox-tmp-audit2-")
        .tempdir()
        .unwrap();
    #[cfg(not(unix))]
    let scratch = std::fs::canonicalize(scratch_dir.path()).unwrap();
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = crate::sandbox_spawn::cleanup_task_temp_dir(&self.0);
        }
    }
    let _cleanup = Cleanup(scratch.clone());
    let mut metadata = PersistedTask::starting(
        "bash-a2f0123456789012".into(),
        "foreign-session".into(),
        "fixture".into(),
        f.root.clone(),
        Some(f.root.clone()),
        None,
        false,
        false,
    );
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &metadata).unwrap();
    f.ctx
        .bash_background()
        .replay_session(&storage, "foreign-session")
        .unwrap();
    // Attach after replay so terminal-task recovery does not schedule cleanup
    // while this fixture is checking access to the still-registered scratch.
    f.ctx
        .bash_background()
        .task_for_test("bash-a2f0123456789012")
        .unwrap()
        .lock_state()
        .unwrap()
        .metadata
        .sandbox_temp_dir = Some(scratch.clone());
    let target = scratch.join("foreign.txt");
    std::fs::write(&target, "foreign secret").unwrap();
    let _route = f.plugin();
    let read = crate::commands::read::handle_read(&request("read", json!({"file":target})), &f.ctx);
    assert_eq!(read.data["code"], "unsafe_temp_path", "{:?}", read.data);
    let write = crate::commands::write::handle_write(
        &request("write", json!({"file":target,"content":"not authorized"})),
        &f.ctx,
    );
    assert_eq!(write.data["code"], "unsafe_temp_path", "{:?}", write.data);
    let delete = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file":target})),
        &f.ctx,
    );
    assert_eq!(delete.data["code"], "unsafe_temp_path", "{:?}", delete.data);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "foreign secret");
    let _owner = ask_route_guard(Some(AskRoute::Plugin {
        scope: plugin::Scope::new(&f.owner, "foreign-session"),
        once: Vec::new(),
    }));
    let mut own = request("read", json!({"file":target}));
    own.session_id = Some("foreign-session".into());
    assert!(crate::commands::read::handle_read(&own, &f.ctx).success);
}

#[test]
fn audit2_temp_sqlite_files_refuse_before_read_write_or_delete() {
    let f = Fixture::new();
    let temp = tempfile::tempdir().unwrap();
    let _route = f.plugin();
    for name in [
        "data.db",
        "data.sqlite",
        "data.sqlite4",
        "data.sqlite.backup",
        "data-wal",
        "data-shm",
        "data-journal",
    ] {
        let path = temp.path().join(name);
        std::fs::write(&path, "database-shaped temporary file").unwrap();
        assert_eq!(
            f.ctx
                .validate_read_path("audit2", "audit-session", &path)
                .unwrap_err()
                .data["code"],
            "unsafe_temp_path",
            "{name}"
        );
        assert_eq!(
            f.ctx.validate_path("audit2", &path).unwrap_err().data["code"],
            "unsafe_temp_path",
            "{name}"
        );
        let response = crate::commands::delete_file::handle_delete_file(
            &request("delete_file", json!({"file":path})),
            &f.ctx,
        );
        assert_eq!(
            response.data["code"], "unsafe_temp_path",
            "{name}: {:?}",
            response.data
        );
        assert!(path.exists());
    }
}

#[test]
fn audit2_temp_hard_links_cannot_bypass_outside_authorization() {
    let f = Fixture::new();
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.txt");
    let link = temp.path().join("alias.txt");
    std::fs::write(&source, "shared inode").unwrap();
    std::fs::hard_link(&source, &link).unwrap();
    let _route = f.plugin();
    assert_eq!(
        f.ctx
            .validate_read_path("audit2", "audit-session", &link)
            .unwrap_err()
            .data["code"],
        "unsafe_temp_path"
    );
    assert_eq!(
        f.ctx.validate_path("audit2", &link).unwrap_err().data["code"],
        "unsafe_temp_path"
    );
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file":link})),
        &f.ctx,
    );
    assert_eq!(response.data["code"], "unsafe_temp_path");
    assert_eq!(std::fs::read_to_string(source).unwrap(), "shared inode");
}

#[test]
fn audit2_temp_roots_cannot_be_recursively_deleted() {
    let f = Fixture::new();
    let _route = f.plugin();
    let mut policy = f.ctx.access_policy("audit2").unwrap();
    let controlled = tempfile::tempdir().unwrap();
    let root = policy_path(controlled.path());
    policy.standalone_temp_guard_roots = vec![root.clone()];
    policy.standalone_temp_roots = vec![root.clone()];
    policy.standalone_managed_temp_roots.clear();
    assert_eq!(
        policy.decide_tree_write(&root),
        AccessDecision::Deny(AccessDenial::UnsafeTemp)
    );
    #[cfg(unix)]
    {
        // Point this thread's temp guards at a directory the test owns, so a
        // broken guard can only ever remove the test's own fixture.
        let _temp = temp::override_roots_for_test(vec![controlled.path().to_path_buf()]);
        let sentinel = controlled.path().join("keep.txt");
        std::fs::write(&sentinel, "must survive").unwrap();
        let floor = f.root.join("floor");
        std::fs::write(&floor, "floor").unwrap();
        f.ctx
            .update_config(|config| config.sandbox.read_deny.push(floor.clone()));
        let response = crate::commands::delete_file::handle_delete_file(
            &request(
                "delete_file",
                json!({"files":[controlled.path(),floor],"recursive":true}),
            ),
            &f.ctx,
        );
        assert_eq!(
            response.data["code"], "unsafe_temp_path",
            "{:?}",
            response.data
        );
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "must survive");
        assert!(floor.exists());
    }
}

/// macOS gives each user a private temp directory under /var/folders. The
/// selector for macOS roots must exempt only the caller's own TMPDIR, never a
/// sibling user's directory or /var/folders as a whole. The selector is plain
/// path logic, so the boundary is checked on every platform with a macOS-style
/// layout under an absolute fixture directory (a bare `/var/folders` is not an
/// absolute path on Windows and would be dropped before the rule applied).
#[test]
fn audit2_macos_exemption_is_only_the_callers_tmpdir() {
    let f = Fixture::new();
    let mut policy = f.ctx.access_policy("audit2").unwrap();
    let folders = f.outside.join("var").join("folders");
    let own = folders.join("audit2").join("own").join("T");
    let foreign = folders.join("audit2").join("foreign").join("T");
    policy.standalone_temp_roots = temp::roots_for(Some(own.clone()), true);
    assert_eq!(
        policy.decide(
            AccessOperation::Read,
            &policy_path(&own.join("file.txt")),
            false
        ),
        AccessDecision::Allow
    );
    assert_eq!(
        policy.decide(
            AccessOperation::Read,
            &policy_path(&foreign.join("file.txt")),
            false
        ),
        AccessDecision::Ask
    );
    assert!(!policy
        .standalone_temp_roots
        .iter()
        .any(|root| root == &policy_path(&folders)));
}

#[test]
fn audit3_missing_route_keeps_temp_guards_for_in_root_paths() {
    let f = Fixture::new();
    let source = f.root.join("source.txt");
    let database = f.root.join("alias.txt");
    std::fs::write(&source, "shared inode inside the project").unwrap();
    std::fs::hard_link(&source, &database).unwrap();
    f.ctx.mark_standalone_plugin_host();
    let temp_root = f.root.parent().unwrap().to_path_buf();
    // A worker thread has no installed route, as when a host's dispatch
    // loses its thread-local route. The project root sits under a directory
    // this thread treats as system temp.
    let response = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let _temp = temp::override_roots_for_test(vec![temp_root]);
                crate::commands::read::handle_read(
                    &request("read", json!({"file":database})),
                    &f.ctx,
                )
            })
            .join()
            .unwrap()
    });
    assert!(!response.success, "{:?}", response.data);
    assert_eq!(
        response.data["code"], "unsafe_temp_path",
        "{:?}",
        response.data
    );
}

#[test]
fn audit2_missing_plugin_thread_route_fails_closed() {
    let f = Fixture::new();
    let target = f.outside.join("secret.txt");
    std::fs::write(&target, "must not leak through a default route").unwrap();
    f.ctx.mark_standalone_plugin_host();
    let response = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                crate::commands::read::handle_read(&request("read", json!({"file":target})), &f.ctx)
            })
            .join()
            .unwrap()
    });
    assert_eq!(
        response.data["code"], "access_route_missing",
        "{:?}",
        response.data
    );
    assert!(!response.success);
}

#[test]
fn audit2_native_token_cannot_cross_sessions() {
    let f = Fixture::new();
    let target = f.outside.join("secret.txt");
    std::fs::write(&target, "session A contents").unwrap();
    f.ctx
        .update_config(|config| config.harness = Some(crate::harness::Harness::Opencode));
    let send = |session: &str, tokens: Vec<String>| {
        let mut req = request("read", json!({"file":target,"permissions_granted":tokens}));
        req.session_id = Some(session.into());
        with_native_route(req, &f.ctx, |req, ctx| {
            crate::commands::read::handle_read(&req, ctx)
        })
    };
    let ask = send("A", Vec::new());
    assert_eq!(ask.data["code"], "permission_required");
    let token = ask.data["asks"][0]["grant_id"].as_str().unwrap().to_owned();
    let foreign = send("B", vec![token.clone()]);
    assert_eq!(
        foreign.data["code"], "permission_required",
        "{:?}",
        foreign.data
    );
    let own = send("A", vec![token]);
    assert!(
        own.success,
        "wrong-session redemption must not consume the valid token: {:?}",
        own.data
    );
}

/// Removes a registered task's scratch directory when the test ends.
#[cfg(unix)]
struct ScratchCleanup(PathBuf);
#[cfg(unix)]
impl Drop for ScratchCleanup {
    fn drop(&mut self) {
        let _ = crate::sandbox_spawn::cleanup_task_temp_dir(&self.0);
    }
}

/// Registers a completed task of `session` whose recorded scratch directory
/// is a real managed task temp dir, so the registry reports it as owned.
#[cfg(unix)]
fn registered_scratch(f: &Fixture, session: &str, task: &str) -> (PathBuf, ScratchCleanup) {
    use crate::bash_background::persistence::{task_paths, write_task, PersistedTask};
    use crate::bash_background::BgTaskStatus;
    let storage = f.ctx.storage_dir();
    let paths = task_paths(&storage, session, task).unwrap();
    std::fs::create_dir_all(&paths.dir).unwrap();
    let scratch = crate::sandbox_spawn::create_task_temp_dir(&paths.dir).unwrap();
    let mut metadata = PersistedTask::starting(
        task.into(),
        session.into(),
        "fixture".into(),
        f.root.clone(),
        Some(f.root.clone()),
        None,
        false,
        false,
    );
    metadata.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    write_task(&paths.json, &metadata).unwrap();
    f.ctx
        .bash_background()
        .replay_session(&storage, session)
        .unwrap();
    f.ctx
        .bash_background()
        .task_for_test(task)
        .unwrap()
        .lock_state()
        .unwrap()
        .metadata
        .sandbox_temp_dir = Some(scratch.clone());
    (scratch.clone(), ScratchCleanup(scratch))
}

/// An entry outside the project that is a symlink into the session's own
/// scratch is not itself scratch. Deleting or replacing it acts on the outside
/// link, so ownership of the link's target must not admit it without an ask.
#[cfg(unix)]
#[test]
fn audit3_outside_symlink_into_owned_scratch_still_asks() {
    let f = Fixture::new();
    let (scratch, _cleanup) = registered_scratch(&f, "audit-session", "bash-a3f0123456789012");
    let owned = scratch.join("owned.txt");
    std::fs::write(&owned, "owned scratch").unwrap();
    let link = f.outside.join("alias.txt");
    std::os::unix::fs::symlink(&owned, &link).unwrap();
    let _route = f.plugin();

    // The scratch file itself is owned: deleting it needs no ask. This shows
    // the registration above is live, so the refusals below are about the link.
    let own_copy = scratch.join("own-copy.txt");
    std::fs::write(&own_copy, "owned").unwrap();
    assert!(f.ctx.validate_write_location("own", &own_copy).is_ok());

    let delete = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file":link})),
        &f.ctx,
    );
    assert_eq!(
        delete.data["code"], "permission_required",
        "{:?}",
        delete.data
    );
    let source = f.root.join("source.txt");
    std::fs::write(&source, "moved").unwrap();
    let replace = crate::commands::move_file::handle_move_file(
        &request("move_file", json!({"file":source,"destination":link})),
        &f.ctx,
    );
    assert_eq!(
        replace.data["code"], "permission_required",
        "{:?}",
        replace.data
    );
    // A write follows the link into the scratch tree. The entry the tool was
    // given is not owned scratch, so the scratch guard refuses outright: an
    // approval would name the scratch file and still hit that guard, so there
    // is nothing useful to ask for.
    let write = crate::commands::write::handle_write(
        &request("write", json!({"file":link,"content":"through the link"})),
        &f.ctx,
    );
    assert!(!write.success, "{:?}", write.data);
    assert_eq!(write.data["code"], "unsafe_temp_path", "{:?}", write.data);
    assert!(std::fs::symlink_metadata(&link)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_to_string(&owned).unwrap(), "owned scratch");
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "moved");
}

/// Files for the parent-swap delete tests: `root/sub/victim.txt` is the
/// authorized target and `outside/victim.txt` is what a swapped parent would
/// expose under the same name.
#[cfg(unix)]
struct SwapFixture {
    f: Fixture,
    sub: PathBuf,
    moved: PathBuf,
    outside_victim: PathBuf,
}

#[cfg(unix)]
impl SwapFixture {
    fn new() -> Self {
        let f = Fixture::new();
        let sub = f.root.join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("victim.txt"), "inside").unwrap();
        let outside_victim = f.outside.join("victim.txt");
        std::fs::write(&outside_victim, "outside must survive").unwrap();
        let moved = f.root.join("sub-moved");
        Self {
            f,
            sub,
            moved,
            outside_victim,
        }
    }

    /// Rename `sub` away and put a symlink to the outside directory in its
    /// place, once, at `stage`.
    fn swap_at(
        &self,
        stage: crate::commands::delete_file::UnlinkStage,
    ) -> Box<dyn FnMut(crate::commands::delete_file::UnlinkStage)> {
        let (sub, moved, outside) = (self.sub.clone(), self.moved.clone(), self.f.outside.clone());
        let mut done = false;
        Box::new(move |at| {
            if at == stage && !done {
                done = true;
                std::fs::rename(&sub, &moved).unwrap();
                std::os::unix::fs::symlink(&outside, &sub).unwrap();
            }
        })
    }
}

/// A parent swapped after the parent directory is held open cannot move the
/// unlink: it removes the authorized entry (now under the moved directory),
/// never the outside file of the same name.
#[cfg(unix)]
#[test]
fn audit3_single_delete_unlinks_inside_the_pinned_parent() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let s = SwapFixture::new();
    let _route = s.f.plugin();
    let _seam = install_unlink_seam_for_test(s.swap_at(UnlinkStage::BeforeUnlink));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": s.sub.join("victim.txt")})),
        &s.f.ctx,
    );
    assert!(response.success, "{:?}", response.data);
    assert_eq!(
        std::fs::read_to_string(&s.outside_victim).unwrap(),
        "outside must survive"
    );
    assert!(!s.moved.join("victim.txt").exists());
}

/// A parent swapped between authorization and the unlink is authorized again
/// at the location the opened directory actually has, so an outside target
/// still needs the host's approval and nothing is removed meanwhile.
#[cfg(unix)]
#[test]
fn audit3_single_delete_reauthorizes_a_swapped_parent() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let s = SwapFixture::new();
    let _route = s.f.plugin();
    let _seam = install_unlink_seam_for_test(s.swap_at(UnlinkStage::BeforePin));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": s.sub.join("victim.txt")})),
        &s.f.ctx,
    );
    assert_eq!(
        response.data["code"], "permission_required",
        "{:?}",
        response.data
    );
    assert_eq!(
        std::fs::read_to_string(&s.outside_victim).unwrap(),
        "outside must survive"
    );
    assert_eq!(
        std::fs::read_to_string(s.moved.join("victim.txt")).unwrap(),
        "inside"
    );
}

/// A batch member refused when it is authorized again right before its own
/// delete stops the batch with that refusal as the top-level response,
/// marked incomplete, instead of being folded into `skipped_files` of a
/// successful batch.
#[cfg(unix)]
#[test]
fn audit3_batch_delete_reports_a_later_refusal_as_incomplete() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let s = SwapFixture::new();
    let first = s.f.root.join("first.txt");
    let last = s.f.root.join("last.txt");
    std::fs::write(&first, "first").unwrap();
    std::fs::write(&last, "last").unwrap();
    let _route = s.f.plugin();
    // The swap happens while the first member is being deleted, after the
    // whole batch was admitted, so only the second member's own check sees it.
    let _seam = install_unlink_seam_for_test(s.swap_at(UnlinkStage::BeforeUnlink));
    let response = crate::commands::delete_file::handle_delete_file(
        &request(
            "delete_file",
            json!({"files": [first, s.sub.join("victim.txt"), last]}),
        ),
        &s.f.ctx,
    );
    assert!(!response.success, "{:?}", response.data);
    assert_eq!(
        response.data["code"], "permission_required",
        "{:?}",
        response.data
    );
    assert!(response.data["asks"][0]["grant_id"].is_string());
    assert_eq!(response.data["complete"], false);
    assert_eq!(response.data["deleted"].as_array().unwrap().len(), 1);
    assert_eq!(response.data["not_attempted"], json!([last]));
    assert!(!first.exists());
    assert_eq!(std::fs::read_to_string(&last).unwrap(), "last");
    assert_eq!(
        std::fs::read_to_string(&s.outside_victim).unwrap(),
        "outside must survive"
    );
}

/// Layout for swaps around the pinned unlink's authorization:
/// `root/sub/victim.txt` is requested, `root/real` is an in-root directory
/// with a decoy of the same name, and `outside/dir/victim.txt` must survive.
#[cfg(unix)]
struct AuthorizeSwap {
    f: Fixture,
    sub: PathBuf,
    real: PathBuf,
    outside_dir: PathBuf,
    outside_moved: PathBuf,
}

#[cfg(unix)]
impl AuthorizeSwap {
    fn new() -> Self {
        let f = Fixture::new();
        let sub = f.root.join("sub");
        let real = f.root.join("real");
        let outside_dir = f.outside.join("dir");
        for (dir, contents) in [
            (&sub, "inside"),
            (&real, "decoy"),
            (&outside_dir, "outside"),
        ] {
            std::fs::create_dir(dir).unwrap();
            std::fs::write(dir.join("victim.txt"), contents).unwrap();
        }
        let outside_moved = f.outside.join("dir-moved");
        Self {
            f,
            sub,
            real,
            outside_dir,
            outside_moved,
        }
    }

    /// Before the parent is opened, `root/sub` becomes a symlink to the
    /// outside directory, so the held directory is outside. Before its
    /// location is authorized, that outside directory is moved aside and its
    /// path becomes a symlink to `root/real`, so the policy judges an in-root
    /// directory. With `restore_after`, the outside directory is put back
    /// right after authorization, so the located path leads to the held
    /// directory again.
    fn seam(
        &self,
        restore_after: bool,
    ) -> Box<dyn FnMut(crate::commands::delete_file::UnlinkStage)> {
        use crate::commands::delete_file::UnlinkStage;
        let (sub, real) = (self.sub.clone(), self.real.clone());
        let (outside, moved) = (self.outside_dir.clone(), self.outside_moved.clone());
        let gone = self.f.root.join("sub-gone");
        Box::new(move |stage| match stage {
            UnlinkStage::BeforePin => {
                std::fs::rename(&sub, &gone).unwrap();
                std::os::unix::fs::symlink(&outside, &sub).unwrap();
            }
            UnlinkStage::BeforeAuthorize => {
                std::fs::rename(&outside, &moved).unwrap();
                std::os::unix::fs::symlink(&real, &outside).unwrap();
            }
            UnlinkStage::AfterAuthorize if restore_after => {
                std::fs::remove_file(&outside).unwrap();
                std::fs::rename(&moved, &outside).unwrap();
            }
            _ => {}
        })
    }

    fn outside_victim(&self, restore_after: bool) -> PathBuf {
        if restore_after {
            self.outside_dir.join("victim.txt")
        } else {
            self.outside_moved.join("victim.txt")
        }
    }
}

/// The held directory is outside, but a swap makes the policy judge an
/// in-root directory. The judged directory is not the located one, so the
/// delete refuses and the outside file survives.
#[cfg(unix)]
#[test]
fn audit4_pinned_unlink_refuses_when_authorization_judged_another_directory() {
    use crate::commands::delete_file::install_unlink_seam_for_test;
    let s = AuthorizeSwap::new();
    let _route = s.f.plugin();
    let _seam = install_unlink_seam_for_test(s.seam(false));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": s.sub.join("victim.txt")})),
        &s.f.ctx,
    );
    assert_eq!(response.data["code"], "path_changed", "{:?}", response.data);
    assert_eq!(
        std::fs::read_to_string(s.outside_victim(false)).unwrap(),
        "outside"
    );
    assert_eq!(
        std::fs::read_to_string(s.real.join("victim.txt")).unwrap(),
        "decoy"
    );
}

/// The same swap, undone right after authorization so the located path leads
/// to the held directory again. Device and inode alone would match; the
/// judged directory still differs, so the delete refuses.
#[cfg(unix)]
#[test]
fn audit4_pinned_unlink_refuses_a_swap_undone_after_authorization() {
    use crate::commands::delete_file::install_unlink_seam_for_test;
    let s = AuthorizeSwap::new();
    let _route = s.f.plugin();
    let _seam = install_unlink_seam_for_test(s.seam(true));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": s.sub.join("victim.txt")})),
        &s.f.ctx,
    );
    assert_eq!(response.data["code"], "path_changed", "{:?}", response.data);
    assert_eq!(
        std::fs::read_to_string(s.outside_victim(true)).unwrap(),
        "outside"
    );
}

/// After authorization the held directory is moved away and its path made a
/// symlink to the outside directory. The located path no longer leads to the
/// held directory, so the delete refuses and nothing is removed anywhere.
#[cfg(unix)]
#[test]
fn audit4_pinned_unlink_refuses_a_held_directory_moved_after_authorization() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let s = AuthorizeSwap::new();
    let _route = s.f.plugin();
    let (sub, outside) = (s.sub.clone(), s.outside_dir.clone());
    let moved = s.f.root.join("sub-moved");
    let target = moved.clone();
    let _seam = install_unlink_seam_for_test(Box::new(move |stage| {
        if stage == UnlinkStage::AfterAuthorize {
            std::fs::rename(&sub, &target).unwrap();
            std::os::unix::fs::symlink(&outside, &sub).unwrap();
        }
    }));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": s.sub.join("victim.txt")})),
        &s.f.ctx,
    );
    assert_eq!(response.data["code"], "path_changed", "{:?}", response.data);
    assert_eq!(
        std::fs::read_to_string(s.outside_dir.join("victim.txt")).unwrap(),
        "outside"
    );
    assert_eq!(
        std::fs::read_to_string(moved.join("victim.txt")).unwrap(),
        "inside"
    );
}

/// On Windows the entry is deleted through a handle bound to the entry
/// itself. A parent replaced by a junction to an outside directory after the
/// caller's authorization makes the handle open the outside entry; its real
/// location is authorized again, needs the host's approval, and nothing is
/// deleted meanwhile. A delete by path would have followed the junction.
#[cfg(windows)]
#[test]
fn audit4_windows_delete_is_bound_to_the_authorized_entry() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let f = Fixture::new();
    let sub = f.root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("victim.txt"), "inside").unwrap();
    let outside = f.outside.join("dir");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("victim.txt"), "outside").unwrap();
    let gone = f.root.join("sub-gone");
    let _route = f.plugin();
    let (link, target, moved) = (sub.clone(), outside.clone(), gone.clone());
    let _seam = install_unlink_seam_for_test(Box::new(move |stage| {
        if stage == UnlinkStage::BeforePin {
            std::fs::rename(&link, &moved).unwrap();
            let status = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(&link)
                .arg(&target)
                .output()
                .unwrap();
            assert!(status.status.success(), "{status:?}");
        }
    }));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": sub.join("victim.txt")})),
        &f.ctx,
    );
    assert_eq!(
        response.data["code"], "permission_required",
        "{:?}",
        response.data
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("victim.txt")).unwrap(),
        "outside"
    );
    assert_eq!(
        std::fs::read_to_string(gone.join("victim.txt")).unwrap(),
        "inside"
    );
}

/// A context with undo backups and checkpoints enabled, rooted outside the
/// system temp directory.
fn backed_up_fixture() -> (tempfile::TempDir, PathBuf, AppContext) {
    let dir = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let config = Config {
        project_root: Some(root.clone()),
        storage_dir: Some(dir.path().join("store")),
        ..Config::default()
    };
    let ctx = AppContext::new(Box::new(crate::language::StubProvider), config);
    (dir, root, ctx)
}

/// Implicit undo restores the stored keys themselves. A stored key that is not
/// absolute would be authorized at the project root but written relative to
/// the daemon's working directory, so it is refused before anything is
/// restored.
#[test]
fn audit5_implicit_undo_refuses_a_relative_stored_key() {
    let (_dir, root, ctx) = backed_up_fixture();
    let file = root.join("kept.txt");
    std::fs::write(&file, "before\n").unwrap();
    let write = crate::commands::write::handle_write(
        &request("write", json!({"file": file, "content": "after\n"})),
        &ctx,
    );
    assert!(write.success, "{:?}", write.data);
    let stored = ctx
        .backup()
        .lock()
        .preview_last_operation_paths("audit-session")
        .unwrap()
        .remove(0);
    ctx.backup()
        .lock()
        .rekey_for_test("audit-session", &stored, PathBuf::from("kept.txt"));
    let undo = crate::commands::undo::handle_undo(&request("undo", json!({})), &ctx);
    assert_eq!(undo.data["code"], "invalid_backup_key", "{:?}", undo.data);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after\n");
}

/// Where the `cp` checkpoint of the fixture's only session is stored on disk.
fn restore_storage_path(ctx: &AppContext) -> PathBuf {
    let sessions = ctx.storage_dir().join("unbound").join("checkpoints");
    let session_dir = std::fs::read_dir(&sessions)
        .unwrap()
        .next()
        .expect("one checkpoint session")
        .unwrap()
        .path();
    session_dir.join("cp")
}

/// The same invariant for restoring a whole checkpoint: its stored keys are the
/// write targets, so a relative one is refused before anything is written.
#[test]
fn audit5_implicit_checkpoint_restore_refuses_a_relative_stored_key() {
    let (_dir, root, ctx) = backed_up_fixture();
    let file = root.join("kept.txt");
    std::fs::write(&file, "checkpointed\n").unwrap();
    let checkpoint = crate::commands::checkpoint::handle_checkpoint(
        &request("checkpoint", json!({"name": "cp", "files": [file]})),
        &ctx,
    );
    assert!(checkpoint.success, "{:?}", checkpoint.data);
    std::fs::write(&file, "changed\n").unwrap();
    // Rewrite the durable record so its only key is relative, as a damaged or
    // hand-edited checkpoint could be.
    let stored = ctx
        .checkpoint()
        .lock()
        .file_paths("audit-session", "cp")
        .unwrap()
        .remove(0);
    let storage = restore_storage_path(&ctx);
    let meta = storage.join("meta.json");
    let original = std::fs::read_to_string(&meta).unwrap();
    let spelled = serde_json::to_string(&stored).unwrap();
    assert!(original.contains(&spelled), "{original}");
    std::fs::write(&meta, original.replace(&spelled, "\"kept.txt\"")).unwrap();
    let restore = crate::commands::restore_checkpoint::handle_restore_checkpoint(
        &request("restore_checkpoint", json!({"name": "cp"})),
        &ctx,
    );
    assert_eq!(
        restore.data["code"], "invalid_checkpoint_key",
        "{:?}",
        restore.data
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "changed\n");
}

/// After every check, the path to the held directory is replaced by a fresh
/// directory holding an entry of the same name. The unlink acts in the held
/// directory, so the authorized entry goes and the replacement survives.
#[cfg(unix)]
#[test]
fn audit5_pinned_unlink_acts_in_the_held_directory_after_a_late_swap() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let f = Fixture::new();
    let sub = f.root.join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("victim.txt"), "authorized").unwrap();
    let moved = f.root.join("sub-moved");
    let _route = f.plugin();
    let (path, target) = (sub.clone(), moved.clone());
    let _seam = install_unlink_seam_for_test(Box::new(move |stage| {
        if stage == UnlinkStage::BeforeUnlink {
            std::fs::rename(&path, &target).unwrap();
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("victim.txt"), "replacement").unwrap();
        }
    }));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": sub.join("victim.txt")})),
        &f.ctx,
    );
    assert!(response.success, "{:?}", response.data);
    assert!(!moved.join("victim.txt").exists());
    assert_eq!(
        std::fs::read_to_string(sub.join("victim.txt")).unwrap(),
        "replacement"
    );
}

/// Deleting a symlink entry removes the link, never what it points at.
#[cfg(unix)]
#[test]
fn audit5_deleting_a_symlink_entry_keeps_its_target() {
    let f = Fixture::new();
    let target = f.root.join("target.txt");
    std::fs::write(&target, "target").unwrap();
    let link = f.root.join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let _route = f.plugin();
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": link})),
        &f.ctx,
    );
    assert!(response.success, "{:?}", response.data);
    assert!(std::fs::symlink_metadata(&link).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "target");
}

/// Deleting one hard-link name leaves the other name and its content.
#[cfg(unix)]
#[test]
fn audit5_deleting_one_hard_link_name_keeps_the_other() {
    let f = Fixture::new();
    let first = f.root.join("first.txt");
    std::fs::write(&first, "shared").unwrap();
    let second = f.root.join("second.txt");
    std::fs::hard_link(&first, &second).unwrap();
    let _route = f.plugin();
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": second})),
        &f.ctx,
    );
    assert!(response.success, "{:?}", response.data);
    assert!(!second.exists());
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "shared");
}

/// A recursive delete binds its root like a single-entry delete. Here the
/// directory holding the requested tree is swapped for a symlink to an
/// outside directory with a tree of the same name before it is opened: the
/// held parent is outside, its real location needs the host's approval, and
/// nothing outside is deleted.
#[cfg(unix)]
#[test]
fn audit5_recursive_delete_authorizes_the_held_parent() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let f = Fixture::new();
    let sub = f.root.join("sub");
    std::fs::create_dir_all(sub.join("tree")).unwrap();
    std::fs::write(sub.join("tree/inside.txt"), "inside").unwrap();
    let outside = f.outside.join("dir");
    std::fs::create_dir_all(outside.join("tree")).unwrap();
    std::fs::write(outside.join("tree/inside.txt"), "outside").unwrap();
    let gone = f.root.join("sub-gone");
    let _route = f.plugin();
    let (path, target, aside) = (sub.clone(), outside.clone(), gone.clone());
    let _seam = install_unlink_seam_for_test(Box::new(move |stage| {
        if stage == UnlinkStage::BeforePin {
            std::fs::rename(&path, &aside).unwrap();
            std::os::unix::fs::symlink(&target, &path).unwrap();
        }
    }));
    let response = crate::commands::delete_file::handle_delete_file(
        &request(
            "delete_file",
            json!({"file": sub.join("tree"), "recursive": true}),
        ),
        &f.ctx,
    );
    assert_eq!(
        response.data["code"], "permission_required",
        "{:?}",
        response.data
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("tree/inside.txt")).unwrap(),
        "outside"
    );
    assert_eq!(
        std::fs::read_to_string(gone.join("tree/inside.txt")).unwrap(),
        "inside"
    );
}

/// The policy is made to judge an in-root directory while the held parent is
/// outside, and the swap is undone right after authorization. The judged
/// directory is not the held one, so the recursive delete refuses.
#[cfg(unix)]
#[test]
fn audit5_recursive_delete_refuses_a_swap_undone_after_authorization() {
    use crate::commands::delete_file::install_unlink_seam_for_test;
    let s = AuthorizeSwap::new();
    for dir in [&s.sub, &s.real, &s.outside_dir] {
        std::fs::create_dir(dir.join("tree")).unwrap();
        std::fs::write(dir.join("tree/kept.txt"), "kept").unwrap();
    }
    let _route = s.f.plugin();
    let _seam = install_unlink_seam_for_test(s.seam(true));
    let response = crate::commands::delete_file::handle_delete_file(
        &request(
            "delete_file",
            json!({"file": s.sub.join("tree"), "recursive": true}),
        ),
        &s.f.ctx,
    );
    assert_eq!(response.data["code"], "path_changed", "{:?}", response.data);
    assert_eq!(
        std::fs::read_to_string(s.outside_dir.join("tree/kept.txt")).unwrap(),
        "kept"
    );
    assert_eq!(
        std::fs::read_to_string(s.real.join("tree/kept.txt")).unwrap(),
        "kept"
    );
}

/// Make `link` a file symlink to `target`. Fails the test, rather than
/// skipping it unseen, when the host cannot create symlinks: Windows needs
/// Developer Mode or an administrator for that, and CI runners have one.
#[cfg(windows)]
fn create_windows_symlink(target: &Path, link: &Path) {
    if let Err(error) = std::os::windows::fs::symlink_file(target, link) {
        panic!(
            "cannot create a file symlink on this host ({error}); enable Developer Mode or run as administrator"
        );
    }
}

/// Make `link` a junction to the directory `target` (no privilege needed).
#[cfg(windows)]
fn junction(link: &Path, target: &Path) {
    let output = std::process::Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

/// The location the Windows delete authorizes names the entry the handle
/// opened: the link itself for a junction or symlink, and the name used to
/// open it for one of several hard links. It is never the object a link
/// resolves to.
#[cfg(windows)]
#[test]
fn audit5_windows_location_names_the_opened_entry() {
    use crate::commands::delete_tree::win::{location, open_entry};
    let f = Fixture::new();
    let root = crate::inspect::job::normalize_path(&std::fs::canonicalize(&f.root).unwrap());
    let target = f.root.join("target");
    std::fs::create_dir(&target).unwrap();
    let link = f.root.join("link");
    junction(&link, &target);
    assert_eq!(
        location(&open_entry(&link).unwrap()).unwrap(),
        root.join("link")
    );
    let first = f.root.join("first.txt");
    std::fs::write(&first, "shared").unwrap();
    let second = f.root.join("second.txt");
    std::fs::hard_link(&first, &second).unwrap();
    assert_eq!(
        location(&open_entry(&second).unwrap()).unwrap(),
        root.join("second.txt")
    );
    assert_eq!(
        location(&open_entry(&first).unwrap()).unwrap(),
        root.join("first.txt")
    );
    let file_link = f.root.join("file-link.txt");
    create_windows_symlink(&first, &file_link);
    assert_eq!(
        location(&open_entry(&file_link).unwrap()).unwrap(),
        root.join("file-link.txt")
    );
}

/// Deleting a junction entry removes the junction and leaves its target
/// directory and contents.
#[cfg(windows)]
#[test]
fn audit5_windows_deleting_a_junction_entry_keeps_its_target() {
    let f = Fixture::new();
    // Undo cannot recreate a Windows link yet, so a backed-up delete refuses
    // links; with nothing backed up the link entry itself is removed.
    f.ctx
        .backup()
        .lock()
        .set_policy(crate::backup::BackupPolicy {
            enabled: false,
            ..Default::default()
        });
    let target = f.root.join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("keep.txt"), "kept").unwrap();
    let link = f.root.join("link");
    junction(&link, &target);
    let _route = f.plugin();
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": link})),
        &f.ctx,
    );
    assert!(response.success, "{:?}", response.data);
    assert!(std::fs::symlink_metadata(&link).is_err());
    assert_eq!(
        std::fs::read_to_string(target.join("keep.txt")).unwrap(),
        "kept"
    );
}

/// Deleting a file symlink entry removes the link and leaves its target.
#[cfg(windows)]
#[test]
fn audit5_windows_deleting_a_symlink_entry_keeps_its_target() {
    let f = Fixture::new();
    // Undo cannot recreate a Windows link yet, so a backed-up delete refuses
    // links; with nothing backed up the link entry itself is removed.
    f.ctx
        .backup()
        .lock()
        .set_policy(crate::backup::BackupPolicy {
            enabled: false,
            ..Default::default()
        });
    let target = f.root.join("target.txt");
    std::fs::write(&target, "target").unwrap();
    let link = f.root.join("link.txt");
    create_windows_symlink(&target, &link);
    let _route = f.plugin();
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": link})),
        &f.ctx,
    );
    assert!(response.success, "{:?}", response.data);
    assert!(std::fs::symlink_metadata(&link).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "target");
}

/// Deleting one hard-link name leaves the other name and its content.
#[cfg(windows)]
#[test]
fn audit5_windows_deleting_one_hard_link_name_keeps_the_other() {
    let f = Fixture::new();
    let first = f.root.join("first.txt");
    std::fs::write(&first, "shared").unwrap();
    let second = f.root.join("second.txt");
    std::fs::hard_link(&first, &second).unwrap();
    let _route = f.plugin();
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": second})),
        &f.ctx,
    );
    assert!(response.success, "{:?}", response.data);
    assert!(!second.exists());
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "shared");
}

/// After authorization the open entry is moved out of the project. The
/// handle no longer sits at the authorized location, so nothing is deleted.
#[cfg(windows)]
#[test]
fn audit5_windows_delete_rechecks_the_handle_after_authorization() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let f = Fixture::new();
    let victim = f.root.join("victim.txt");
    std::fs::write(&victim, "moved").unwrap();
    let moved = f.outside.join("moved.txt");
    let _route = f.plugin();
    let (from, to) = (victim.clone(), moved.clone());
    let _seam = install_unlink_seam_for_test(Box::new(move |stage| {
        if stage == UnlinkStage::AfterAuthorize {
            std::fs::rename(&from, &to).unwrap();
        }
    }));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": victim})),
        &f.ctx,
    );
    assert_eq!(response.data["code"], "path_changed", "{:?}", response.data);
    assert_eq!(std::fs::read_to_string(&moved).unwrap(), "moved");
}

/// A recursive delete opens the tree root itself by handle. A root replaced
/// by a junction to an outside directory before it is opened is a link, not
/// the authorized directory, so the delete refuses and the outside files
/// stay.
#[cfg(windows)]
#[test]
fn audit5_windows_recursive_delete_binds_the_root() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let f = Fixture::new();
    let tree = f.root.join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("inside.txt"), "inside").unwrap();
    let outside = f.outside.join("tree");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("inside.txt"), "outside").unwrap();
    let gone = f.root.join("tree-gone");
    let _route = f.plugin();
    let (path, target, aside) = (tree.clone(), outside.clone(), gone.clone());
    let _seam = install_unlink_seam_for_test(Box::new(move |stage| {
        if stage == UnlinkStage::BeforePin {
            std::fs::rename(&path, &aside).unwrap();
            junction(&path, &target);
        }
    }));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": tree, "recursive": true})),
        &f.ctx,
    );
    assert_eq!(response.data["code"], "path_changed", "{:?}", response.data);
    assert_eq!(
        std::fs::read_to_string(outside.join("inside.txt")).unwrap(),
        "outside"
    );
    assert_eq!(
        std::fs::read_to_string(gone.join("inside.txt")).unwrap(),
        "inside"
    );
}

/// After authorization the open tree root is moved out of the project. Its
/// handle no longer sits at the judged location, so nothing is deleted.
#[cfg(windows)]
#[test]
fn audit5_windows_recursive_delete_rechecks_the_root_after_authorization() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let f = Fixture::new();
    let tree = f.root.join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("inside.txt"), "inside").unwrap();
    let moved = f.outside.join("tree-moved");
    let _route = f.plugin();
    let (from, to) = (tree.clone(), moved.clone());
    let _seam = install_unlink_seam_for_test(Box::new(move |stage| {
        if stage == UnlinkStage::AfterAuthorize {
            std::fs::rename(&from, &to).unwrap();
        }
    }));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": tree, "recursive": true})),
        &f.ctx,
    );
    assert_eq!(response.data["code"], "path_changed", "{:?}", response.data);
    assert_eq!(
        std::fs::read_to_string(moved.join("inside.txt")).unwrap(),
        "inside"
    );
}

/// Undo of a file creation removes the file and the directories the write
/// created. The restore authorizes exactly what it is about to touch, so a
/// recorded created directory outside the project still needs the host's
/// approval even though the previewed key (the file) is inside it.
#[test]
fn audit6_operation_undo_authorizes_the_directories_it_removes() {
    let (dir, root, ctx) = backed_up_fixture();
    let owner = std::sync::Arc::new(());
    let _route = ask_route_guard(Some(AskRoute::Plugin {
        scope: plugin::Scope::new(&owner, "audit-session"),
        once: Vec::new(),
    }));
    let file = root.join("new").join("file.txt");
    let write = crate::commands::write::handle_write(
        &request("write", json!({"file": file, "content": "created\n"})),
        &ctx,
    );
    assert!(write.success, "{:?}", write.data);
    let key = ctx
        .backup()
        .lock()
        .preview_last_operation_paths("audit-session")
        .unwrap()
        .remove(0);
    let outside = dir.path().join("outside-empty");
    std::fs::create_dir(&outside).unwrap();
    ctx.backup()
        .lock()
        .add_created_dir_for_test("audit-session", &key, outside.clone());
    let undo = crate::commands::undo::handle_undo(&request("undo", json!({})), &ctx);
    assert_eq!(undo.data["code"], "permission_required", "{:?}", undo.data);
    assert!(outside.is_dir(), "an unauthorized directory was removed");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "created\n");
}

/// Make `dir` a directory holding `a.txt` and `b.txt` with `contents`.
#[cfg(unix)]
fn fill(dir: &Path, contents: &str) {
    std::fs::create_dir_all(dir).unwrap();
    for name in ["a.txt", "b.txt"] {
        std::fs::write(dir.join(name), contents).unwrap();
    }
}

/// After every check on the held parent, the tree is moved aside and a
/// different tree with the same names is put under the root's name. Only the
/// recorded root may be deleted, so the delete refuses and nothing from the
/// replacement is removed, unbacked leaves included.
#[cfg(unix)]
#[test]
fn audit6_recursive_delete_refuses_a_replaced_root() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let f = Fixture::new();
    // Nothing is backed up, so every leaf is an unbacked entry whose only
    // binding is the identity the walk recorded.
    f.ctx
        .backup()
        .lock()
        .set_policy(crate::backup::BackupPolicy {
            enabled: false,
            ..Default::default()
        });
    let tree = f.root.join("tree");
    fill(&tree, "recorded");
    let aside = f.root.join("tree-aside");
    let _route = f.plugin();
    let (path, moved) = (tree.clone(), aside.clone());
    let _seam = install_unlink_seam_for_test(Box::new(move |stage| {
        if stage == UnlinkStage::BeforeUnlink {
            std::fs::rename(&path, &moved).unwrap();
            fill(&path, "replacement");
        }
    }));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": tree, "recursive": true})),
        &f.ctx,
    );
    assert_eq!(response.data["code"], "path_changed", "{:?}", response.data);
    for name in ["a.txt", "b.txt"] {
        assert_eq!(
            std::fs::read_to_string(tree.join(name)).unwrap(),
            "replacement"
        );
        assert_eq!(
            std::fs::read_to_string(aside.join(name)).unwrap(),
            "recorded"
        );
    }
}

/// The same for a directory inside the tree: once it is swapped for a
/// different directory with the same names, the delete stops there with a
/// typed partial result and leaves the replacement whole.
#[cfg(unix)]
#[test]
fn audit6_recursive_delete_refuses_a_replaced_interior_directory() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let f = Fixture::new();
    // Nothing is backed up, so every leaf is an unbacked entry whose only
    // binding is the identity the walk recorded.
    f.ctx
        .backup()
        .lock()
        .set_policy(crate::backup::BackupPolicy {
            enabled: false,
            ..Default::default()
        });
    let tree = f.root.join("tree");
    let sub = tree.join("sub");
    fill(&sub, "recorded");
    let aside = f.root.join("sub-aside");
    let _route = f.plugin();
    let (path, moved) = (sub.clone(), aside.clone());
    let _seam = install_unlink_seam_for_test(Box::new(move |stage| {
        if stage == UnlinkStage::BeforeUnlink {
            std::fs::rename(&path, &moved).unwrap();
            fill(&path, "replacement");
        }
    }));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": tree, "recursive": true})),
        &f.ctx,
    );
    assert_eq!(response.data["code"], "path_changed", "{:?}", response.data);
    assert_eq!(response.data["complete"], false, "{:?}", response.data);
    assert!(response.data["remaining_entries"].as_u64().unwrap() > 0);
    for name in ["a.txt", "b.txt"] {
        assert_eq!(
            std::fs::read_to_string(sub.join(name)).unwrap(),
            "replacement"
        );
    }
}

/// After authorization the held tree root is renamed and a different tree is
/// put at its old path. The delete keeps acting on the held, authorized root
/// (allowed: it is the authorized object), opening its children through the
/// root's handle, so the replacement at the old path is never touched.
#[cfg(windows)]
#[test]
fn audit6_windows_recursive_delete_stays_with_the_held_root() {
    use crate::commands::delete_file::{install_unlink_seam_for_test, UnlinkStage};
    let f = Fixture::new();
    let tree = f.root.join("tree");
    std::fs::create_dir(&tree).unwrap();
    std::fs::write(tree.join("a.txt"), "recorded").unwrap();
    let moved = f.root.join("tree-moved");
    let _route = f.plugin();
    let (path, aside) = (tree.clone(), moved.clone());
    let _seam = install_unlink_seam_for_test(Box::new(move |stage| {
        if stage == UnlinkStage::BeforeUnlink {
            std::fs::rename(&path, &aside).unwrap();
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("a.txt"), "replacement").unwrap();
        }
    }));
    let response = crate::commands::delete_file::handle_delete_file(
        &request("delete_file", json!({"file": tree, "recursive": true})),
        &f.ctx,
    );
    assert!(response.success, "{:?}", response.data);
    assert!(!moved.exists(), "the authorized tree was deleted");
    assert_eq!(
        std::fs::read_to_string(tree.join("a.txt")).unwrap(),
        "replacement"
    );
}

fn request_with_id(id: &str, command: &str, params: Value) -> RawRequest {
    RawRequest {
        id: id.into(),
        ..request(command, params)
    }
}

/// An operation undo's authorization can wait for a person. While it waits,
/// no backup lock is held, so another undo on a different stack finishes.
#[test]
fn audit7_operation_undo_authorizes_without_holding_backup_locks() {
    let (_dir, root, ctx) = backed_up_fixture();
    let first = root.join("first.txt");
    let second = root.join("second.txt");
    std::fs::write(&first, "first before\n").unwrap();
    std::fs::write(&second, "second before\n").unwrap();
    for (file, content) in [(&first, "first after\n"), (&second, "second after\n")] {
        let write = crate::commands::write::handle_write(
            &request("write", json!({"file": file, "content": content})),
            &ctx,
        );
        assert!(write.success, "{:?}", write.data);
    }
    let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let entered_tx = std::sync::Mutex::new(entered_tx);
    let release_rx = std::sync::Mutex::new(release_rx);
    let _hook = crate::commands::undo::install_authorization_hook_for_test(
        "undo-blocking",
        std::sync::Arc::new(move || {
            let _ = entered_tx.lock().unwrap().send(());
            let _ = release_rx
                .lock()
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(20));
        }),
    );
    std::thread::scope(|scope| {
        let blocked = scope.spawn(|| {
            crate::commands::undo::handle_undo(
                &request_with_id("undo-blocking", "undo", json!({})),
                &ctx,
            )
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the operation undo reached its authorization");
        let other = scope.spawn(|| {
            crate::commands::undo::handle_undo(
                &request_with_id("undo-other", "undo", json!({"file": first})),
                &ctx,
            )
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !other.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let finished_meanwhile = other.is_finished();
        // Release before asserting so a red run never strands the threads.
        release_tx.send(()).unwrap();
        let other = other.join().unwrap();
        let blocked = blocked.join().unwrap();
        assert!(
            finished_meanwhile,
            "another undo waited for a pending authorization"
        );
        assert!(other.success, "{:?}", other.data);
        assert!(blocked.success, "{:?}", blocked.data);
    });
    assert_eq!(std::fs::read_to_string(&first).unwrap(), "first before\n");
    assert_eq!(std::fs::read_to_string(&second).unwrap(), "second before\n");
}

/// If the operation changes while its authorization is pending, the decision
/// is about something else and is discarded. When it keeps changing, the
/// undo gives up after a bounded number of plans with a typed, retryable
/// refusal and restores nothing.
#[test]
fn audit7_operation_undo_discards_an_authorization_for_a_changed_plan() {
    let (_dir, root, ctx) = backed_up_fixture();
    let file = root.join("kept.txt");
    std::fs::write(&file, "before\n").unwrap();
    let write = crate::commands::write::handle_write(
        &request("write", json!({"file": file, "content": "after\n"})),
        &ctx,
    );
    assert!(write.success, "{:?}", write.data);
    let ctx = std::sync::Arc::new(ctx);
    let passes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (hook_ctx, hook_root, hook_passes) = (ctx.clone(), root.clone(), passes.clone());
    let _hook = crate::commands::undo::install_authorization_hook_for_test(
        "undo-moving",
        std::sync::Arc::new(move || {
            // Another edit lands while this authorization is pending, so the
            // newest operation is a different one each time.
            let pass = hook_passes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let write = crate::commands::write::handle_write(
                &request(
                    "write",
                    json!({"file": hook_root.join(format!("new-{pass}.txt")), "content": "x\n"}),
                ),
                &hook_ctx,
            );
            assert!(write.success, "{:?}", write.data);
        }),
    );
    let undo = crate::commands::undo::handle_undo(
        &request_with_id("undo-moving", "undo", json!({})),
        &ctx,
    );
    assert_eq!(undo.data["code"], "undo_plan_changed", "{:?}", undo.data);
    assert_eq!(undo.data["retryable"], true);
    assert_eq!(passes.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after\n");
    for pass in 0..3 {
        assert!(root.join(format!("new-{pass}.txt")).exists());
    }
}

/// What the handler of a native plugin-host request receives as its tool
/// arguments, after the native route claimed and stripped approval metadata.
fn native_arguments(harness: crate::harness::Harness, params: Value) -> Value {
    let f = Fixture::new();
    f.ctx.update_config(|config| config.harness = Some(harness));
    let request = RawRequest {
        id: "native".into(),
        command: "tool_call".into(),
        session_id: Some("audit-session".into()),
        lsp_hints: None,
        params,
    };
    let seen = std::cell::RefCell::new(Value::Null);
    with_native_route(request, &f.ctx, |request, _| {
        *seen.borrow_mut() = request.params["arguments"].clone();
        Response::success("native", json!({}))
    });
    seen.into_inner()
}

/// A shell nested in a native `tool_call` keeps the approvals the host sent
/// in `permissions_granted`: its approved command patterns and escalation
/// grants are read from there.
#[test]
fn audit8_native_tool_call_shell_keeps_host_approvals() {
    for harness in [
        crate::harness::Harness::Opencode,
        crate::harness::Harness::Pi,
    ] {
        for shell in ["bash", "powershell"] {
            let arguments = native_arguments(
                harness.clone(),
                json!({"name": shell, "arguments": {
                    "command": "git status --short",
                    "permissions_granted": ["git status *"],
                }}),
            );
            assert_eq!(
                arguments["permissions_granted"],
                json!(["git status *"]),
                "{shell}: {arguments}"
            );
        }
    }
}

/// Every other nested tool still has `permissions_granted` removed, so a
/// value that is not a claimed approval never reaches its handler.
#[test]
fn audit8_native_tool_call_strips_approvals_from_other_tools() {
    for harness in [
        crate::harness::Harness::Opencode,
        crate::harness::Harness::Pi,
    ] {
        let arguments = native_arguments(
            harness,
            json!({"name": "read", "arguments": {
                "path": "/outside/file",
                "permissions_granted": ["outside:forged", "anything"],
            }}),
        );
        assert!(
            arguments.get("permissions_granted").is_none(),
            "{arguments}"
        );
    }
}

/// A host that approved an outside read after the token expired gets a new
/// ask for that read marked `approval_expired`; an ordinary ask carries no
/// reason.
#[test]
fn audit9_ask_after_an_expired_approval_says_so() {
    let f = Fixture::new();
    let target = f.outside.join("late.txt");
    std::fs::write(&target, "late").unwrap();
    let scope = plugin::Scope::new(&f.owner, "audit-session").for_tool("read", &Value::Null);
    let ask_for = |scope: plugin::Scope, once: Vec<AccessGrant>| {
        let _route = ask_route_guard(Some(AskRoute::Plugin { scope, once }));
        f.ctx
            .validate_read_path("audit9", "audit-session", &target)
            .unwrap_err()
    };
    let first = ask_for(scope.clone(), Vec::new());
    assert_eq!(
        first.data["code"], "permission_required",
        "{:?}",
        first.data
    );
    assert!(
        first.data["asks"][0].get("reason").is_none(),
        "{:?}",
        first.data
    );
    let token = first.data["asks"][0]["grant_id"]
        .as_str()
        .unwrap()
        .to_string();
    plugin::expire_for_test(&token);
    let mut late = scope.clone();
    let once = plugin::claim(&mut late, &[token]);
    assert!(once.is_empty());
    let again = ask_for(late, once);
    assert_eq!(
        again.data["code"], "permission_required",
        "{:?}",
        again.data
    );
    assert_eq!(again.data["asks"][0]["reason"], "approval_expired");
}

/// Runs `inspect` through one of its entry points, deferred or not, and
/// returns the final response.
fn run_inspect(ctx: &std::sync::Arc<AppContext>, req: &RawRequest, deferred: bool) -> Response {
    if !deferred {
        return crate::commands::inspect::handle_inspect_tool_call(req, ctx);
    }
    let mut pending =
        match crate::commands::inspect::handle_inspect_deferred(req, std::sync::Arc::clone(ctx)) {
            crate::response_finalize::DispatchOutcome::Deferred(pending) => pending,
            crate::response_finalize::DispatchOutcome::Immediate(response) => return response,
        };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if let Some(response) = (pending.poll)(ctx) {
            return response;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "deferred inspect did not answer"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A plugin host inspecting a file outside the project gets the engine's
/// outside-access ask back unchanged (code and asks), on the direct and the
/// deferred inspect entry points. Retried with the approval, the inspect runs.
#[test]
fn audit10_outside_inspect_asks_then_runs_with_the_approval() {
    let f = Fixture::new();
    let mut config = Config {
        project_root: Some(f.root.clone()),
        storage_dir: Some(f._dir.path().join("inspect-store")),
        ..Config::default()
    };
    config.backup.enabled = Some(false);
    let ctx = std::sync::Arc::new(AppContext::new(
        Box::new(crate::language::StubProvider),
        config,
    ));
    ctx.set_harness(crate::harness::Harness::Opencode);
    let target = f.outside.join("probe.rs");
    std::fs::write(&target, "// TODO: outside\nfn main() {}\n").unwrap();
    let scope = plugin::Scope::new(&f.owner, "audit-session").for_tool("inspect", &Value::Null);
    for deferred in [false, true] {
        let req = request(
            "inspect",
            json!({"scope": target, "sections": "todos", "topK": 5}),
        );
        let asked = {
            let _route = ask_route_guard(Some(AskRoute::Plugin {
                scope: scope.clone(),
                once: Vec::new(),
            }));
            run_inspect(&ctx, &req, deferred)
        };
        assert_eq!(
            asked.data["code"], "permission_required",
            "deferred={deferred}: {:?}",
            asked.data
        );
        assert_eq!(
            asked.data["asks"][0]["kind"], "external_directory",
            "{:?}",
            asked.data
        );
        assert_eq!(asked.data["asks"][0]["operation"], "read");
        let token = asked.data["asks"][0]["grant_id"]
            .as_str()
            .unwrap()
            .to_string();
        let mut approved = scope.clone();
        let once = plugin::claim(&mut approved, &[token]);
        assert!(!once.is_empty());
        let ran = {
            let _route = ask_route_guard(Some(AskRoute::Plugin {
                scope: approved,
                once,
            }));
            run_inspect(&ctx, &req, deferred)
        };
        assert!(ran.success, "deferred={deferred}: {:?}", ran.data);
        assert!(
            ran.data.get("code").is_none(),
            "deferred={deferred}: {:?}",
            ran.data
        );
    }
}

/// A zoom batch whose entry names a file outside the project returns the
/// engine's outside-access ask as the whole reply, not as one refused entry
/// inside a successful batch, so the host can ask. Retried with the
/// approval, every entry answers.
#[test]
fn audit11_outside_zoom_batch_entry_asks_then_answers_with_the_approval() {
    let f = Fixture::new();
    let mut config = Config {
        project_root: Some(f.root.clone()),
        storage_dir: Some(f._dir.path().join("zoom-store")),
        ..Config::default()
    };
    config.backup.enabled = Some(false);
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let inside = f.root.join("inside.rs");
    let outside = f.outside.join("outside.rs");
    std::fs::write(&inside, "pub fn inside_fn() {}\n").unwrap();
    std::fs::write(&outside, "pub fn outside_fn() {}\n").unwrap();
    let scope = plugin::Scope::new(&f.owner, "audit-session").for_tool("zoom", &Value::Null);
    let req = request(
        "zoom",
        json!({"targets": [
            {"file": inside, "symbols": "inside_fn"},
            {"file": outside, "symbols": "outside_fn"},
        ]}),
    );
    let asked = {
        let _route = ask_route_guard(Some(AskRoute::Plugin {
            scope: scope.clone(),
            once: Vec::new(),
        }));
        crate::commands::zoom::handle_zoom(&req, &ctx)
    };
    assert_eq!(
        asked.data["code"], "permission_required",
        "{:?}",
        asked.data
    );
    assert_eq!(asked.data["asks"][0]["operation"], "read");
    let token = asked.data["asks"][0]["grant_id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut approved = scope.clone();
    let once = plugin::claim(&mut approved, &[token]);
    assert!(!once.is_empty());
    let answered = {
        let _route = ask_route_guard(Some(AskRoute::Plugin {
            scope: approved,
            once,
        }));
        crate::commands::zoom::handle_zoom(&req, &ctx)
    };
    assert!(answered.success, "{:?}", answered.data);
    let text = answered.data.to_string();
    assert!(text.contains("inside_fn"), "{text}");
    assert!(text.contains("outside_fn"), "{text}");
    assert!(!text.contains("permission_required"), "{text}");
}

#[test]
fn outside_grep_grant_round_trip_plain_directory() {
    let f = Fixture::new();
    std::fs::create_dir(f.outside.join("packages")).unwrap();
    std::fs::write(f.outside.join("packages/probe.rs"), "outside_grep_canary\n").unwrap();
    assert_outside_grep_round_trip(&f, &f.outside);
}

#[cfg(unix)]
#[test]
fn outside_grep_grant_round_trip_symlinked_parent() {
    let f = Fixture::new();
    let scratch = f._dir.path().join("CortexKit-scratch");
    std::fs::create_dir(&scratch).unwrap();
    std::os::unix::fs::symlink(&f.outside, scratch.join("antigravity-auth-314-prs")).unwrap();
    std::fs::create_dir(f.outside.join("packages")).unwrap();
    std::fs::write(f.outside.join("packages/probe.rs"), "outside_grep_canary\n").unwrap();
    assert_outside_grep_round_trip(&f, &scratch.join("antigravity-auth-314-prs"));
}

fn assert_outside_grep_round_trip(f: &Fixture, folder: &Path) {
    f.ctx.set_harness(crate::harness::Harness::Opencode);
    let run = |path: &Path, tokens: Vec<String>| {
        crate::access_policy::with_native_route(
            request(
                "grep",
                json!({"path": path, "pattern": "outside_grep_canary", "permissions_granted": tokens}),
            ),
            &f.ctx,
            |req, ctx| crate::commands::grep::handle_grep(&req, ctx),
        )
    };
    for path in [folder.to_path_buf(), folder.join("packages")] {
        let asked = run(folder, Vec::new());
        assert_eq!(
            asked.data["code"], "permission_required",
            "{:?}",
            asked.data
        );
        assert_eq!(asked.data["asks"][0]["operation"], "read");
        let token = asked.data["asks"][0]["grant_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let answered = run(&path, vec![token]);
        assert!(answered.success, "{}: {:?}", path.display(), answered.data);
        assert!(
            answered.data.to_string().contains("outside_grep_canary"),
            "{:?}",
            answered.data
        );
    }
}
