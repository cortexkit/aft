use super::*;

#[tokio::test]
async fn bus_views_without_hooks_put_no_keys() {
    let mut writer = RecordingWriter::default();
    Publication::new(&NoticeViewsReady::default())
        .sync_with_hooks(&mut writer, None)
        .await
        .unwrap();
    assert!(writer.0.is_empty());
}

#[tokio::test]
async fn bus_views_oversized_is_skipped_not_truncated_and_changes_republish() {
    let mut writer = RecordingWriter::default();
    let ready = NoticeViewsReady::default();
    let mut publication = Publication::new(&ready);
    let cap = declaration().max_value_bytes as usize;
    publication
        .sync(
            &mut writer,
            &[
                ("oversize".into(), vec![b'x'; cap + 1]),
                ("at-limit".into(), vec![b'y'; cap]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(publication.skipped.len(), 1);
    assert_eq!(writer.0, vec![(key("at-limit"), vec![b'y'; cap])]);
    publication
        .sync(&mut writer, &[("at-limit".into(), b"revised".to_vec())])
        .await
        .unwrap();
    assert_eq!(writer.0[1], (key("at-limit"), b"revised".to_vec()));
}
