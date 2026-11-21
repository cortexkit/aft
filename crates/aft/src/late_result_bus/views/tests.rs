use super::*;

#[tokio::test]
async fn bus_views_without_hooks_put_no_keys() {
    let mut writer = RecordingWriter::default();
    Publication::default()
        .sync_with_hooks(&mut writer, None)
        .await
        .unwrap();
    assert!(writer.0.is_empty());
}

#[tokio::test]
async fn bus_views_bytes_match_every_served_pin_and_full_pin_key() {
    let mut writer = RecordingWriter::default();
    let mut publication = Publication::default();
    publication
        .sync(&mut writer, crate::subc::tool_views::bus_snapshot())
        .await
        .unwrap();
    assert_eq!(writer.0.len(), crate::subc::tool_views::SHIPPED.len());
    for (pin, _) in crate::subc::tool_views::SHIPPED.iter() {
        let request = cortexkit_role_tool_provider::views::ViewsRequest::new(vec![pin.clone()]);
        let reply = cortexkit_role_tool_provider::views::ViewsReply::new(
            std::collections::BTreeMap::from([(
                pin.clone(),
                cortexkit_role_tool_provider::views::ViewEntry::Manifest(
                    crate::subc::tool_views::view(pin).unwrap(),
                ),
            )]),
        );
        reply.validate_for(&request).unwrap();
        let route_bytes = serde_json::to_vec(&serde_json::to_value(reply).unwrap()).unwrap();
        assert_eq!(
            writer
                .0
                .iter()
                .find(|(stored, _)| stored == &key(pin))
                .unwrap()
                .1,
            route_bytes
        );
        assert!(key(pin)
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)));
    }
    let pin = "tp1:apply_patch:fa62c9797e5d14436714f5630d15815d2adc1185023a3dcba3f3ed20fc9f0548:1";
    assert!(crate::subc::tool_views::SHIPPED.contains_key(pin));
    assert_eq!(key(pin), "7470313a6170706c795f70617463683a666136326339373937653564313434333637313466353633306431353831356432616463313138353032336133646362613366336564323066633966303534383a31");
    assert_ne!(key(pin), key(&pin.replace(":1", ":2")));
    let writes = writer.0.len();
    publication
        .sync(&mut writer, crate::subc::tool_views::bus_snapshot())
        .await
        .unwrap();
    assert_eq!(writer.0.len(), writes);
}

#[tokio::test]
async fn bus_views_oversized_is_skipped_not_truncated_and_changes_republish() {
    let mut writer = RecordingWriter::default();
    let mut publication = Publication::default();
    let cap = declaration().max_value_bytes as usize;
    publication
        .sync(
            &mut writer,
            vec![
                ("oversize".into(), vec![b'x'; cap + 1]),
                ("at-limit".into(), vec![b'y'; cap]),
            ],
        )
        .await
        .unwrap();
    assert_eq!(publication.skipped.len(), 1);
    assert_eq!(writer.0, vec![(key("at-limit"), vec![b'y'; cap])]);
    publication
        .sync(&mut writer, vec![("at-limit".into(), b"revised".to_vec())])
        .await
        .unwrap();
    assert_eq!(writer.0[1], (key("at-limit"), b"revised".to_vec()));
}
