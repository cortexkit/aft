#[derive(Clone, Default)]
pub(super) struct RecordingWriter(pub(super) Arc<parking_lot::Mutex<Vec<(String, Bytes)>>>);
impl Writer for RecordingWriter {
    async fn put(&mut self, key: &str, value: Bytes) -> Result<(), &'static str> {
        self.0.lock().push((key.into(), value));
        Ok(())
    }
}

#[cfg(feature = "test-support")]
pub async fn put_session(session: &str) -> serde_json::Value {
    let mut snapshot = Snapshot::new();
    snapshot_segments(&mut snapshot);
    snapshot.retain(|(id, _)| id.as_ref() == session);
    let mut writer = RecordingWriter::default();
    Publication::default()
        .sync(&mut writer, &mut snapshot, &mut StatusHealth::default())
        .await
        .unwrap();
    let values = writer.0.lock();
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].0, key(session));
    serde_json::from_slice(&values[0].1).unwrap()
}
use super::*;
