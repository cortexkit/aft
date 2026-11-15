#![cfg_attr(test, allow(hidden_glob_reexports))]
pub use aft_db::db::call_ledger::*;

#[cfg(test)]
#[allow(unused_imports)]
use cortexkit_role_tool_provider::late_results::{check_since, Cursor};
#[cfg(test)]
#[allow(unused_imports)]
use cortexkit_role_tool_provider::withdraw::{CompletedResult, StartedOutcome, WithdrawAnswer};
#[cfg(test)]
#[allow(unused_imports)]
use rusqlite::{params, Connection, OptionalExtension};
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use serde_json::{json, Value};
#[cfg(test)]
#[allow(unused_imports)]
use subc_protocol::{ErrorBody, FrameType};

#[cfg(test)]
mod tests {
    use aft_db::db::call_ledger::tests::*;

    use super::*;

    #[test]
    fn runner_restart_held_shell_recovers_exact_terminal_bytes_or_explicit_unknown() {
        crate::subc::lower_hooks::register();
        let conn = database();
        let scope = scope("core");
        let held = serde_json::to_vec(&json!({"content":[{"type":"text","text":""}],"held":{"reason":"running","yielded":true}})).unwrap();
        for (carrier, task) in [("broca", "live-task"), ("lost", "missing-task")] {
            let key = key(carrier);
            admit(&conn, &key, "digest", Some(&scope), State::DispatchStarted).unwrap();
            link_task(&conn, &key, task).unwrap();
            hold_running(&conn, &key, task, &held, false).unwrap();
        }
        let key = key("broca");
        conn.execute("INSERT INTO bash_tasks(harness,session_id,task_id,project_key,command,cwd,status,started_at,metadata) VALUES('h','s','live-task','p','command','cwd','running',1,?1)", [json!({"call_key":{"requester":key.carrier,"key":key.call_key}}).to_string()]).unwrap();
        recover(&conn).unwrap();
        assert_eq!(
            get(&conn, &key).unwrap().unwrap().state,
            State::DispatchStarted
        );
        let lost = get(&conn, &super::tests::key("lost")).unwrap().unwrap();
        assert_eq!(lost.state, State::Settled);
        assert_eq!(
            lost.frame.unwrap().body,
            unknown_bash_result("missing-task", None, "").body
        );
        let ordinary = RecordedFrame {
            ty: FrameType::Response,
            body: serde_json::to_vec(
                &json!({"content":[{"type":"text","text":"real command output"}],"isError":false}),
            )
            .unwrap(),
        };
        assert!(observe_task(
            &conn,
            &key.carrier,
            &key.call_key,
            "live-task",
            Some(&ordinary),
            "ok"
        )
        .unwrap());
        assert!(!observe_task(
            &conn,
            &key.carrier,
            &key.call_key,
            "live-task",
            Some(&ordinary),
            "ok"
        )
        .unwrap());
        let row = get(&conn, &key).unwrap().unwrap();
        assert_eq!(row.frame.unwrap().body, ordinary.body);
        assert_eq!(row.seq, Some(1));
        assert_eq!(
            page(&conn, "core", None, None, now_ms(), usize::MAX)["entries"],
            json!([])
        );
        assert_eq!(
            page(&conn, "broca", None, None, now_ms(), usize::MAX)["entries"][0]["result"],
            serde_json::from_slice::<Value>(&ordinary.body).unwrap()
        );
    }
}
