use super::test_writer::RecordingWriter;
use super::*;
use aft_base::runner_status_state::Sessions;
use sha2::{Digest, Sha256};
use std::cell::Cell;

thread_local! {
    pub(super) static KEY_RENDERS: Cell<usize> = const { Cell::new(0) };
    pub(super) static VALUE_RENDERS: Cell<usize> = const { Cell::new(0) };
}

fn session(start: u64) -> Sessions {
    let mut sessions = Sessions::new(start);
    sessions.bound(1, "runner:session/one");
    assert!(sessions.publish("runner:session/one", "[AFT E0 W1 | T2]"));
    sessions
}

#[test]
fn status_kv_key_is_lowercase_hex_of_utf8() {
    assert_eq!(key("session-1"), "73657373696f6e2d31");
    assert_eq!(key("runner:one/two"), "72756e6e65723a6f6e652f74776f");
    assert_eq!(key("会話:é/🦀"), "e4bc9ae8a9b13ac3a92ff09fa680");
}

#[test]
fn status_kv_shape_and_seeded_generation_track_changes_not_refreshes() {
    let mut sessions = session(123_000);
    let first = sessions.snapshot().pop().unwrap().1;
    assert_eq!(first.generation, 123_001);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&encode(&first.text, first.generation))
            .unwrap(),
        serde_json::json!({"v": 1, "segment": "[AFT E0 W1 | T2]", "generation": 123_001})
    );
    assert!(!sessions.publish("runner:session/one", "[AFT E0 W1 | T2]"));
    assert!(sessions.publish("runner:session/one", "[AFT E1 W1 | T2]"));
    assert!(sessions.publish("runner:session/one", "[AFT E0 W1 | T2]"));
    assert_eq!(sessions.snapshot()[0].1.generation, 123_003);
    sessions.unbound(1, "runner:session/one");
    sessions.bound(2, "runner:session/one");
    sessions.publish("runner:session/one", "new lifetime");
    assert_eq!(sessions.snapshot()[0].1.generation, 123_004);
    let restarted = session(124_000);
    assert!(restarted.snapshot()[0].1.generation > sessions.snapshot()[0].1.generation);
}

#[tokio::test(start_paused = true)]
async fn status_kv_refreshes_at_five_seconds_and_writes_changes_immediately() {
    let mut sessions = session(0);
    let mut publication = Publication::default();
    let mut writer = RecordingWriter::default();
    let mut health = StatusHealth::default();
    publication
        .sync(&mut writer, &mut sessions.snapshot(), &mut health)
        .await
        .unwrap();
    let first = writer.0.lock()[0].clone();
    assert_eq!(
        publication.next_refresh(),
        Instant::now() + Duration::from_secs(5)
    );
    tokio::time::advance(Duration::from_millis(4_999)).await;
    publication
        .sync(&mut writer, &mut sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(writer.0.lock().len(), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    publication
        .sync(&mut writer, &mut sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(
        writer.0.lock()[1],
        first,
        "refresh must preserve generation and exact bytes"
    );
    sessions.publish("runner:session/one", "changed now");
    publication
        .sync(&mut writer, &mut sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(writer.0.lock().len(), 3);
    assert_eq!((health.puts, health.refreshes), (2, 1));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&writer.0.lock()[2].1).unwrap()["generation"],
        2
    );
}

#[tokio::test(start_paused = true)]
async fn status_kv_stops_only_after_last_session_route_and_ignores_unpublished_sessions() {
    let mut sessions = session(0);
    sessions.bound(2, "runner:session/one");
    sessions.bound(3, "never-published");
    assert!(!sessions.publish("not-bound", "must not appear"));
    let mut publication = Publication::default();
    let mut writer = RecordingWriter::default();
    let mut health = StatusHealth::default();
    publication
        .sync(&mut writer, &mut sessions.snapshot(), &mut health)
        .await
        .unwrap();
    sessions.unbound(1, "runner:session/one");
    tokio::time::advance(REFRESH).await;
    publication
        .sync(&mut writer, &mut sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(
        writer.0.lock().len(),
        2,
        "another route keeps the session live"
    );
    let mut stale_snapshot = sessions.snapshot();
    sessions.unbound(2, "runner:session/one");
    assert!(!sessions.publish("runner:session/one", "late completion after close"));
    tokio::time::advance(REFRESH * 3).await;
    publication
        .sync(&mut writer, &mut stale_snapshot, &mut health)
        .await
        .unwrap();
    publication
        .sync(&mut writer, &mut sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(writer.0.lock().len(), 2, "ended key must be left to expire");
    assert!(publication.written.is_empty());
}

#[tokio::test(start_paused = true)]
async fn status_kv_oversize_skips_whole_value_once_per_change_and_never_refreshes_old_text() {
    let cap = declaration().max_value_bytes as usize;
    assert_eq!(cap, 2_048);
    let mut sessions = session(0);
    let mut publication = Publication::default();
    let mut writer = RecordingWriter::default();
    let mut health = StatusHealth::default();
    publication
        .sync(&mut writer, &mut sessions.snapshot(), &mut health)
        .await
        .unwrap();
    let exact = "x".repeat(cap - encode("", 2).len());
    sessions.publish("runner:session/one", &exact);
    publication
        .sync(&mut writer, &mut sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(writer.0.lock()[1].1.len(), cap);
    // JSON escaping, not the raw segment length, decides whether a value fits.
    let oversize = "\"".repeat(1_024);
    sessions.publish("runner:session/one", &oversize);
    for _ in 0..3 {
        tokio::time::advance(REFRESH).await;
        publication
            .sync(&mut writer, &mut sessions.snapshot(), &mut health)
            .await
            .unwrap();
        assert!(
            publication.next_refresh() > Instant::now(),
            "skip cannot busy-loop on an old deadline"
        );
    }
    assert_eq!((writer.0.lock().len(), health.skips), (2, 1));
    sessions.publish("runner:session/one", "fits again");
    publication
        .sync(&mut writer, &mut sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(writer.0.lock().len(), 3);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&writer.0.lock()[2].1).unwrap()["generation"],
        4
    );
}

#[test]
fn status_kv_recorded_vectors_match_real_encoder() {
    let root = crate::test_support::manifest_dir().join("fixtures/status-kv");
    let vectors = [
        ("ascii", "session-1", "[AFT E0 W1 | T2]", 1),
        ("punctuation", "runner:one/two", "[AFT E2 W0 | T3]", 42),
        (
            "unicode",
            "会話:é/🦀",
            "[AFT E? W0 | T1]",
            1_700_000_000_001,
        ),
    ];
    let mut sums = String::new();
    for (name, session, segment, generation) in vectors {
        for (extension, bytes) in [
            ("key", key(session).into_bytes()),
            ("json", encode(segment, generation)),
        ] {
            let filename = format!("{name}.{extension}");
            if std::env::var_os("AFT_RECORD_STATUS_VECTORS").is_some() {
                std::fs::write(root.join(&filename), &bytes).unwrap();
            }
            assert_eq!(
                std::fs::read(root.join(&filename)).unwrap(),
                bytes,
                "{filename}"
            );
            sums.push_str(&format!("{:x}  {filename}\n", Sha256::digest(&bytes)));
        }
    }
    if std::env::var_os("AFT_RECORD_STATUS_VECTORS").is_some() {
        std::fs::write(root.join("SHA256SUMS"), &sums).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(root.join("SHA256SUMS")).unwrap(),
        sums
    );
}

struct RefusingConnector {
    attempts: Arc<parking_lot::Mutex<Vec<Instant>>>,
    writer: RecordingWriter,
}
impl Connector for RefusingConnector {
    type Writer = RecordingWriter;
    async fn attach(&mut self) -> Result<Self::Writer, &'static str> {
        let mut attempts = self.attempts.lock();
        attempts.push(Instant::now());
        if attempts.len() < 3 {
            Err("status_bucket_refused")
        } else {
            Ok(self.writer.clone())
        }
    }
}

#[tokio::test(start_paused = true)]
async fn status_kv_refused_bucket_retries_independently_then_recovers() {
    let sessions = Arc::new(parking_lot::Mutex::new(session(0)));
    let feed = sessions.clone();
    let attempts = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let writer = RecordingWriter::default();
    let mut connector = RefusingConnector {
        attempts: attempts.clone(),
        writer: writer.clone(),
    };
    let start = Instant::now();
    let task = tokio::spawn(async move {
        drive(
            &mut connector,
            |values| feed.lock().snapshot_into(values),
            &Notify::new(),
        )
        .await
    });
    tokio::task::yield_now().await;
    assert_eq!(*attempts.lock(), vec![start]);
    let other_service = tokio::spawn(async { "other bus services remain runnable" });
    assert_eq!(
        other_service.await.unwrap(),
        "other bus services remain runnable"
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert_eq!(attempts.lock().len(), 2);
    tokio::time::advance(Duration::from_secs(2)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        *attempts.lock(),
        vec![
            start,
            start + Duration::from_secs(1),
            start + Duration::from_secs(3)
        ]
    );
    assert_eq!(writer.0.lock().len(), 1);
    tokio::time::advance(REFRESH).await;
    tokio::task::yield_now().await;
    assert_eq!(writer.0.lock().len(), 2, "recovered writer still refreshes");
    sessions.lock().unbound(1, "runner:session/one");
    tokio::time::advance(REFRESH * 3).await;
    tokio::task::yield_now().await;
    assert_eq!(writer.0.lock().len(), 2);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn status_kv_failed_put_does_not_mark_value_written() {
    struct Refused;
    impl Writer for Refused {
        async fn put(&mut self, _: &str, _: Bytes) -> Result<(), &'static str> {
            Err("status_put_refused")
        }
    }
    let sessions = session(0);
    let mut publication = Publication::default();
    let mut health = StatusHealth::default();
    assert_eq!(
        publication
            .sync(&mut Refused, &mut sessions.snapshot(), &mut health)
            .await,
        Err("status_put_refused")
    );
    assert!(publication.written.is_empty());
    assert_eq!((health.puts, health.refreshes), (0, 0));
    let mut writer = RecordingWriter::default();
    publication
        .sync(&mut writer, &mut sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(writer.0.lock().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn status_kv_reuses_rendering_across_sixty_sessions() {
    let mut sessions = Sessions::new(0);
    for i in 0..60 {
        let id = format!("runner/session:{i}");
        sessions.bound(i, &id);
        assert!(sessions.publish(&id, "[AFT E0 W0 | T1]"));
    }
    let mut publication = Publication::default();
    let mut writer = RecordingWriter::default();
    let mut health = StatusHealth::default();
    let mut values = Snapshot::new();

    let mut old_value_renders = 0;
    let mut old_key_renders = 0;
    let mut value_renders = 0;
    let mut key_renders = 0;
    for (advance, all_due, change_one, puts, renders, keys) in [
        (Duration::ZERO, true, false, 60, 60, 60),
        (Duration::ZERO, false, false, 0, 0, 0),
        (REFRESH, true, false, 60, 0, 0),
        (Duration::ZERO, false, true, 1, 1, 0),
    ] {
        if !advance.is_zero() {
            tokio::time::advance(advance).await;
        }
        if change_one {
            assert!(sessions.publish("runner/session:0", "[AFT E1 W0 | T1]"));
        }
        sessions.snapshot_into(&mut values);

        // Replay the old rendering work: encode before checking whether a
        // write is due, then format the key for each due or changed value.
        KEY_RENDERS.set(0);
        VALUE_RENDERS.set(0);
        for (id, segment) in &values {
            let _ = encode(&segment.text, segment.generation);
            if all_due || (change_one && id.as_ref() == "runner/session:0") {
                let _ = key(id);
            }
        }
        old_value_renders += VALUE_RENDERS.get();
        old_key_renders += KEY_RENDERS.get();
        KEY_RENDERS.set(0);
        VALUE_RENDERS.set(0);
        let prior = writer.0.lock().len();
        publication
            .sync(&mut writer, &mut values, &mut health)
            .await
            .unwrap();
        assert!(values.is_empty());
        assert_eq!(writer.0.lock().len() - prior, puts);
        assert_eq!(VALUE_RENDERS.get(), renders);
        assert_eq!(KEY_RENDERS.get(), keys);
        value_renders += VALUE_RENDERS.get();
        key_renders += KEY_RENDERS.get();
    }
    assert_eq!((health.puts, health.refreshes), (61, 60));
    let writes = writer.0.lock();
    for (initial, refreshed) in writes[..60].iter().zip(&writes[60..120]) {
        assert_eq!(initial, refreshed);
        assert_eq!(initial.1.as_ptr(), refreshed.1.as_ptr(), "reuse the bytes");
    }
    eprintln!(
        "60 sessions, initial write + early wake + TTL refresh + one change: \
         value renders {old_value_renders} -> {value_renders}; \
         key renders {old_key_renders} -> {key_renders}; 121 writes in both cases"
    );
}
