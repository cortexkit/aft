use super::*;
#[cfg(unix)]
use crate::access_policy::{AccessGrant, AccessOperation, GrantKind};

pub fn scope(name: &str, epoch: u64) -> ScopeKey {
    ScopeKey {
        owner: "owner".into(),
        scope_ref: name.into(),
        epoch,
    }
}

#[cfg(unix)]
pub fn wait_terminal(
    registry: &BgTaskRegistry,
    task_id: &str,
    session: &str,
) -> super::super::BgTaskSnapshot {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let snapshot = registry.status(task_id, session, None, None, 0).unwrap();
        if snapshot.info.status.is_terminal() {
            return snapshot;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{task_id} still {:?}",
            snapshot.info.status
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// aft.db with a registered worker scope holding a saved grant for each of
/// `paths`.
#[cfg(unix)]
pub fn grant_database(
    dir: &Path,
    scope: &ScopeKey,
    paths: &[&Path],
) -> Arc<Mutex<crate::db::TrackedConnection>> {
    std::fs::create_dir_all(dir).unwrap();
    let conn = crate::db::open(&dir.join("aft.db")).unwrap();
    crate::db::access_grants::register_scope(&conn, scope, None, &crate::config::Config::default())
        .unwrap();
    crate::db::access_grants::register_worker(&conn, scope).unwrap();
    for path in paths {
        crate::db::access_grants::insert(
            &conn,
            scope,
            &AccessGrant {
                path: path.to_path_buf(),
                kind: GrantKind::Subtree,
                operation: AccessOperation::Read,
            },
        )
        .unwrap();
    }
    Arc::new(Mutex::new(conn))
}

#[cfg(all(unix, feature = "test-support"))]
pub fn recorded_folder_grants(
    registry: &BgTaskRegistry,
    task: &str,
    session: &str,
) -> Option<TaskFolderGrants> {
    registry
        .task_for_session(task, session)
        .unwrap()
        .lock_state()
        .unwrap()
        .metadata
        .folder_grants
        .clone()
}
