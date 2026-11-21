use super::*;
use crate::subc::rich_ui::notices::pull;
use cortexkit_role_tool_display::{NoticeBody, NoticeWrite};
use serde_json::json;
use subc_protocol::Principal;

use super::super::{announce_retained, read_for_reader, tests::test_writer};

#[tokio::test]
async fn notice_records_no_write_or_announcement_queued_before_views_publish() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let writer = test_writer(path.clone());
    let conn = crate::db::open(&path).unwrap();
    let record = || {
        pull(
            "broca",
            "session",
            "aft:bash:bash-gated",
            NoticeBody::new("bash_finished:1", None, Default::default()),
        )
        .unwrap()
    };
    let id = record().id;
    let ready = NoticeViewsReady::default();
    stage_after_views(&writer, record(), &ready, Duration::ZERO).await;
    assert!(
        crate::db::display_records::read(&conn, &id, crate::db::display_records::now_ms())
            .unwrap()
            .is_none()
    );
    assert!(writer
        .outbox
        .lock()
        .unwrap()
        .reads
        .due(Instant::now())
        .is_none());

    let ready = NoticeViewsReady::default();
    let waiting = stage_after_views(&writer, record(), &ready, Duration::from_secs(5));
    tokio::pin!(waiting);
    std::future::poll_fn(|cx| {
        use std::future::Future;
        assert!(waiting.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    assert!(
        crate::db::display_records::read(&conn, &id, crate::db::display_records::now_ms())
            .unwrap()
            .is_none()
    );
    assert!(writer
        .outbox
        .lock()
        .unwrap()
        .reads
        .due(Instant::now())
        .is_none());
    ready.confirm();
    waiting.await;
    assert_eq!(
        writer
            .outbox
            .lock()
            .unwrap()
            .reads
            .due(Instant::now())
            .as_deref(),
        Some(id.as_str())
    );
}

#[tokio::test]
async fn notice_record_is_announced_when_contended_outbox_unlocks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let writer = test_writer(path.clone());
    let conn = crate::db::open(&path).unwrap();
    let record = pull(
        "broca",
        "session",
        "aft:bash:bash-contended",
        NoticeBody::new("bash_finished:1", None, Default::default()),
    )
    .unwrap();
    let id = record.id.clone();
    let expected = record.bytes.clone();
    let mut held = writer.outbox.lock().unwrap();
    stage_with_contention(&writer, record, Duration::from_secs(5), || {
        assert!(held.reads.due(Instant::now()).is_none());
        assert_eq!(
            crate::db::display_records::read(&conn, &id, crate::db::display_records::now_ms())
                .unwrap(),
            Some(expected.clone()),
            "the write finished while the outbox was locked",
        );
        drop(held);
    })
    .await;

    let (due, bytes) = {
        let now = Instant::now();
        let mut outbox = writer.outbox.lock().unwrap();
        let due = outbox.reads.due(now).expect("notice wake must be queued");
        let bytes = outbox.reads.read(&due, now).unwrap();
        (due, bytes)
    };
    assert_eq!(due, id);
    assert_eq!(bytes, expected);
    announce_retained(&writer, due, bytes, async { Ok(()) })
        .await
        .unwrap();
    let mut outbox = writer.outbox.lock().unwrap();
    outbox.reads.published(&id, true, Instant::now());
    assert!(outbox.reads.due(Instant::now()).is_none());
}

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
