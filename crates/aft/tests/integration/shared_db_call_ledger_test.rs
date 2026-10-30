//! A standalone `aft` process (a plugin bridge session, for example) can share
//! one `aft.db` with a running subc module. Only the module admits keyed
//! tool-provider calls, so only the module may run the call ledger's restart
//! recovery: a standalone process that recovered on opening the database
//! would settle the module's in-flight calls as not started or unknown.

use std::time::{Duration, Instant};

use aft::db::call_ledger::{self as ledger, Admission, Key, State};
use serde_json::json;

use super::helpers::AftProcess;

fn key(call_key: &str) -> Key {
    Key {
        carrier: "reserved:carrier".into(),
        call_key: call_key.into(),
    }
}

fn state_of(conn: &rusqlite::Connection, call_key: &str) -> State {
    ledger::get(conn, &key(call_key))
        .expect("read ledger row")
        .expect("ledger row exists")
        .state
}

#[test]
fn standalone_configure_leaves_a_running_module_s_unsettled_calls_alone() {
    let project = tempfile::tempdir().expect("project dir");
    let storage = tempfile::tempdir().expect("storage dir");
    let db_path = storage.path().join("aft.db");

    // Stand in for a module that is still running these calls: this test
    // process admits them and stays alive throughout.
    let module = aft::db::open(&db_path).expect("create shared aft.db");
    for (call_key, state) in [
        ("held-before-dispatch", State::Prepared),
        ("dispatch-in-flight", State::DispatchStarted),
    ] {
        let admission = ledger::admit(&module, &key(call_key), "digest", None, state)
            .expect("admit the module's call");
        assert!(matches!(admission, Admission::New));
    }
    drop(module);

    let mut aft = AftProcess::spawn();
    let response = aft.send(
        &json!({
            "id": "cfg",
            "command": "configure",
            "harness": "opencode",
            "project_root": project.path(),
            "storage_dir": storage.path(),
        })
        .to_string(),
    );
    assert_eq!(response["success"], true, "configure failed: {response:?}");
    // A persistence-requiring command succeeds only after the standalone
    // process has published its database, which is when it would recover.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let read = aft.send(
            &json!({
                "id": "probe",
                "command": "db_get_state",
                "params": { "key": "ledger-recovery-probe" },
            })
            .to_string(),
        );
        if read["success"] == true {
            break;
        }
        assert_eq!(read["code"], "database_initializing", "{read:?}");
        assert!(
            Instant::now() < deadline,
            "standalone database never opened: {read:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let check = aft::db::open(&db_path).expect("reopen shared aft.db");
    assert_eq!(state_of(&check, "held-before-dispatch"), State::Prepared);
    assert_eq!(
        state_of(&check, "dispatch-in-flight"),
        State::DispatchStarted
    );
    drop(check);
    let status = aft.shutdown();
    assert!(status.success(), "aft exited with {status}");

    // The module process itself still recovers after a restart: once this
    // process is marked as the provider, a fresh pool settles both rows.
    ledger::mark_provider_process();
    let pool = std::sync::Arc::new(std::sync::Mutex::new(
        aft::db::open(&db_path).expect("open as the restarted module"),
    ));
    ledger::recover_pool_once(&pool).expect("module recovery");
    let conn = pool.lock().unwrap();
    assert_eq!(state_of(&conn, "held-before-dispatch"), State::Settled);
    assert_eq!(state_of(&conn, "dispatch-in-flight"), State::Settled);
}
