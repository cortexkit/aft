use super::*;
use crate::subc::rich_ui::notices::pull;
use aft_engine::test_bridge::display_data::late_result_bus::views::{
    confirm_notice_views, fail_notice_views,
};
use cortexkit_role_tool_display::{NoticeBody, NoticeWrite};
use futures_util::FutureExt;
use serde_json::json;
use subc_protocol::Principal;

use super::super::{announce_retained, read_for_reader, tests::test_writer};

#[tokio::test]
async fn tool_provider_awaited_finish_announces_its_record_once_across_restart() {
    use crate::bash_background::{
        persistence::PersistedTask, BgTaskRegistry, BgTaskStatus, TaskIdentity,
    };
    use crate::protocol::{BashCompletedFrame, PushFrame};
    use crate::subc::rich_ui::notices::for_awaited_work;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let writer = test_writer(path.clone());
    let key = "bash-0000000000000001";
    let mut task = PersistedTask::starting(
        key.into(),
        "worker".into(),
        "printf hello".into(),
        dir.path().into(),
        None,
        None,
        true,
        false,
    );
    task.launching_identity = Some(TaskIdentity {
        principal: Some("reserved:broca".into()),
        harness: "runner".into(),
        session: "worker".into(),
    });
    task.mark_terminal(BgTaskStatus::Completed, Some(0), None);
    let frame = PushFrame::BashCompleted(BashCompletedFrame::new(
        key,
        "worker",
        BgTaskStatus::Completed,
        Some(0),
        "printf hello",
        "hello",
        false,
        None,
        None,
        false,
    ));
    let registry = BgTaskRegistry::new(Default::default());
    let record = || for_awaited_work(&registry, &frame, &task, task.started_at).unwrap();
    let id = record().id;
    assert_eq!(
        stage_for_display(&writer, record(), Duration::from_secs(5))
            .await
            .as_deref(),
        Some(id.as_str())
    );
    let bytes = writer
        .store
        .read(id.clone(), crate::db::display_records::now_ms())
        .await
        .unwrap();
    let notice: NoticeWrite = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(notice.send_id, id);
    assert_eq!(notice.session, "worker");
    assert_eq!(notice.kind, "notice");
    assert_eq!(notice.body.view, "bash_finished:1");
    // The pending announcement survives losing AFT's in-memory queues.
    drop(writer);
    let writer = test_writer(path.clone());
    assert_eq!(writer.store.restore_awaited_count().await.unwrap(), 1);
    let first = publish_awaited_with(&writer, |published| {
        assert_eq!(published, id);
        std::future::ready(Ok(()))
    })
    .await
    .unwrap();
    assert_eq!(first.as_deref(), Some(id.as_str()));

    // Rebuilding the record for a replay must not clear the published receipt.
    drop(writer);
    let writer = test_writer(path);
    assert_eq!(writer.store.restore_awaited_count().await.unwrap(), 0);
    assert_eq!(
        stage_for_display(&writer, record(), Duration::from_secs(5)).await,
        Some(id)
    );
    assert_eq!(
        publish_awaited_with(&writer, |_| async {
            panic!("duplicate display_record_ready.v1")
        })
        .await
        .unwrap(),
        None
    );
    assert!(writer
        .outbox
        .lock()
        .unwrap()
        .reads
        .due(Instant::now())
        .is_none());
}

#[tokio::test(start_paused = true)]
async fn awaited_notice_record_waits_for_storage_after_the_initial_budget() {
    let (commit, written) = tokio::sync::oneshot::channel();
    let wait = confirm_linked_write(written, Duration::from_millis(50));
    tokio::pin!(wait);
    assert!(futures_util::poll!(&mut wait).is_pending());
    tokio::time::advance(Duration::from_millis(51)).await;
    assert!(
        futures_util::poll!(&mut wait).is_pending(),
        "do not freeze an absent pointer while the write can still commit"
    );
    commit.send(true).unwrap();
    assert_eq!(wait.await, Ok(true));
}

#[tokio::test(start_paused = true)]
async fn a_failed_awaited_record_announcement_does_not_block_the_next_record() {
    let dir = tempfile::tempdir().unwrap();
    let writer = test_writer(dir.path().join("aft.db"));
    let record = |id: &str| PullRecord {
        id: id.into(),
        bytes: b"{}".to_vec(),
    };
    let first = "notice:awaited:01";
    let second = "notice:awaited:02";
    for id in [first, second] {
        assert!(
            stage_for_display(&writer, record(id), Duration::from_secs(5))
                .await
                .is_some()
        );
    }
    assert_eq!(
        publish_awaited_with(&writer, |id| async move {
            assert_eq!(id, first);
            Err("injected")
        })
        .await,
        Err("injected")
    );
    assert_eq!(
        publish_awaited_with(&writer, |id| async move {
            assert_eq!(id, second);
            Ok(())
        })
        .await
        .unwrap()
        .as_deref(),
        Some(second)
    );
    assert_eq!(
        publish_awaited_with(&writer, |_| async {
            panic!("record must wait for its backoff")
        })
        .await
        .unwrap(),
        None
    );
    tokio::time::advance(Duration::from_millis(100)).await;
    assert_eq!(
        publish_awaited_with(&writer, |id| async move {
            assert_eq!(id, first);
            Ok(())
        })
        .await
        .unwrap()
        .as_deref(),
        Some(first)
    );
}

#[tokio::test]
async fn no_awaited_announcement_needs_no_idle_sqlite_query() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("still-unopened.db");
    let writer = test_writer(path.clone());
    assert_eq!(
        publish_awaited_with(&writer, |_| async { panic!("no record") })
            .await
            .unwrap(),
        None
    );
    assert!(
        !path.exists(),
        "the empty in-memory count must bypass the store"
    );
}

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
    confirm_notice_views(&ready);
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
async fn notice_record_waits_in_memory_when_views_arrive_after_startup_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let writer = test_writer(path.clone());
    let conn = crate::db::open(&path).unwrap();
    let ready = NoticeViewsReady::default();
    let record = startup_record();
    let id = record.id.clone();
    let expected = record.bytes.clone();
    stage_after_views(&writer, record, &ready, Duration::ZERO).await;

    let read = || {
        crate::db::display_records::read(&conn, &id, crate::db::display_records::now_ms()).unwrap()
    };
    let due = || writer.outbox.lock().unwrap().reads.due(Instant::now());
    assert!(read().is_none());
    assert!(due().is_none());
    assert!(stage_pending(&writer, &ready, Duration::from_secs(5))
        .now_or_never()
        .is_none());

    confirm_notice_views(&ready);
    assert!(tokio::time::timeout(
        Duration::from_secs(10),
        stage_pending(&writer, &ready, Duration::from_secs(5)),
    )
    .await
    .expect("publication should release the deferred notice"));
    assert_eq!(read(), Some(expected));
    assert_eq!(due().as_deref(), Some(id.as_str()));
}

#[tokio::test]
async fn pending_notice_records_are_discarded_when_views_never_publish() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aft.db");
    let writer = test_writer(path.clone());
    let conn = crate::db::open(&path).unwrap();
    let ready = NoticeViewsReady::default();
    let record = startup_record();
    let id = record.id.clone();
    stage_after_views(&writer, record, &ready, Duration::ZERO).await;
    fail_notice_views(&ready, "bus_worker_stopped");
    confirm_notice_views(&ready);
    stage_after_views(&writer, startup_record(), &ready, Duration::ZERO).await;

    assert_eq!(
        stage_pending(&writer, &ready, Duration::from_secs(5)).now_or_never(),
        Some(false),
        "permanent failure discards queued records and closes further admission",
    );
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
}

fn startup_record() -> PullRecord {
    pull(
        "broca",
        "session",
        "aft:bash:bash-startup",
        NoticeBody::new("bash_finished:1", None, Default::default()),
    )
    .unwrap()
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
