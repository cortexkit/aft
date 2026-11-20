use super::*;
use crate::subc::rich_ui::notices::pull;
use cortexkit_role_tool_display::{NoticeBody, NoticeWrite};
use serde_json::json;
use subc_protocol::Principal;

use super::super::{announce_retained, read_for_reader, tests::test_writer};

#[tokio::test]
async fn notice_records_persist_before_wake_and_display_read_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let writer = test_writer(path.clone());
    let record = pull(
        "broca",
        "session",
        "aft:bash:bash-demo",
        NoticeBody::new(
            "bash_finished:1",
            Some("aft:bash-task:bash-demo".into()),
            Default::default(),
        ),
    )
    .unwrap();
    let id = record.id.clone();
    let expected = record.bytes.clone();
    stage(&writer, record, Duration::from_secs(5)).await;
    let now = Instant::now();
    let pulled = writer.outbox.lock().unwrap().reads.read(&id, now).unwrap();
    assert_eq!(pulled, expected);
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        crate::db::display_records::read(&conn, &id, crate::db::display_records::now_ms()).unwrap(),
        Some(expected.clone())
    );
    announce_retained(&writer, id.clone(), pulled, async {
        assert_eq!(
            crate::db::display_records::read(&conn, &id, crate::db::display_records::now_ms())
                .unwrap(),
            Some(expected.clone())
        );
        Ok(())
    })
    .await
    .unwrap();
    drop(conn);
    drop(writer);
    let restarted = test_writer(path);
    let principal = Principal::Reserved {
        module_id: "reader".into(),
    };
    let args = json!({"record_id":id});
    let bytes = read_for_reader(
        &restarted,
        Some(&principal),
        Some("reader"),
        args.clone(),
        now,
        crate::db::display_records::now_ms(),
    )
    .await
    .unwrap();
    assert_eq!(bytes, expected);
    assert_eq!(
        serde_json::from_slice::<NoticeWrite>(&bytes)
            .unwrap()
            .send_id,
        "aft:bash:bash-demo"
    );
    assert_eq!(
        read_for_reader(
            &restarted,
            Some(&principal),
            None,
            args,
            now,
            crate::db::display_records::now_ms()
        )
        .await
        .unwrap_err()
        .code,
        cortexkit_role_tool_display::DISPLAY_READ_DENIED
    );
}
