use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Default)]
struct RecordingWriter(Arc<parking_lot::Mutex<Vec<(String, Vec<u8>)>>>);
impl Writer for RecordingWriter {
    async fn put(&mut self, key: &str, value: Vec<u8>) -> Result<(), &'static str> {
        self.0.lock().push((key.into(), value));
        Ok(())
    }
}

fn session(start: u64) -> Sessions {
    let mut sessions = Sessions::new(start);
    sessions.bound(1, "runner:session/one");
    assert!(sessions.publish("runner:session/one", "[AFT E0 W1 | T2]"));
    sessions
}

pub(crate) async fn put_session(session: &str) -> serde_json::Value {
    let snapshot = SESSIONS
        .lock()
        .snapshot()
        .into_iter()
        .filter(|(id, _)| id == session)
        .collect();
    let mut writer = RecordingWriter::default();
    Publication::default()
        .sync(&mut writer, snapshot, &mut StatusHealth::default())
        .await
        .unwrap();
    let values = writer.0.lock();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].0, key(session));
    serde_json::from_slice(&values[0].1).unwrap()
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
        .sync(&mut writer, sessions.snapshot(), &mut health)
        .await
        .unwrap();
    let first = writer.0.lock()[0].clone();
    assert_eq!(
        publication.next_refresh(),
        Instant::now() + Duration::from_secs(5)
    );
    tokio::time::advance(Duration::from_millis(4_999)).await;
    publication
        .sync(&mut writer, sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(writer.0.lock().len(), 1);
    tokio::time::advance(Duration::from_millis(1)).await;
    publication
        .sync(&mut writer, sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(
        writer.0.lock()[1],
        first,
        "refresh must preserve generation and exact bytes"
    );
    sessions.publish("runner:session/one", "changed now");
    publication
        .sync(&mut writer, sessions.snapshot(), &mut health)
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
        .sync(&mut writer, sessions.snapshot(), &mut health)
        .await
        .unwrap();
    sessions.unbound(1, "runner:session/one");
    tokio::time::advance(REFRESH).await;
    publication
        .sync(&mut writer, sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(
        writer.0.lock().len(),
        2,
        "another route keeps the session live"
    );
    let stale_snapshot = sessions.snapshot();
    sessions.unbound(2, "runner:session/one");
    assert!(!sessions.publish("runner:session/one", "late completion after close"));
    tokio::time::advance(REFRESH * 3).await;
    publication
        .sync(&mut writer, stale_snapshot, &mut health)
        .await
        .unwrap();
    publication
        .sync(&mut writer, sessions.snapshot(), &mut health)
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
        .sync(&mut writer, sessions.snapshot(), &mut health)
        .await
        .unwrap();
    let exact = "x".repeat(cap - encode("", 2).len());
    sessions.publish("runner:session/one", &exact);
    publication
        .sync(&mut writer, sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(writer.0.lock()[1].1.len(), cap);
    // JSON escaping, not the raw segment length, decides whether a value fits.
    let oversize = "\"".repeat(1_024);
    sessions.publish("runner:session/one", &oversize);
    for _ in 0..3 {
        tokio::time::advance(REFRESH).await;
        publication
            .sync(&mut writer, sessions.snapshot(), &mut health)
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
        .sync(&mut writer, sessions.snapshot(), &mut health)
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
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../test-vectors/aft-status-kv-v1");
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
        drive(&mut connector, || feed.lock().snapshot(), &Notify::new()).await
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
        async fn put(&mut self, _: &str, _: Vec<u8>) -> Result<(), &'static str> {
            Err("status_put_refused")
        }
    }
    let sessions = session(0);
    let mut publication = Publication::default();
    let mut health = StatusHealth::default();
    assert_eq!(
        publication
            .sync(&mut Refused, sessions.snapshot(), &mut health)
            .await,
        Err("status_put_refused")
    );
    assert!(publication.written.is_empty());
    assert_eq!((health.puts, health.refreshes), (0, 0));
    let mut writer = RecordingWriter::default();
    publication
        .sync(&mut writer, sessions.snapshot(), &mut health)
        .await
        .unwrap();
    assert_eq!(writer.0.lock().len(), 1);
}
