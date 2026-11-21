use aft_engine::test_bridge::display_data::late_result_bus::views::{
    key, Publication, RecordingWriter,
};

#[tokio::test]
async fn bus_views_bytes_match_every_served_pin_and_full_pin_key() {
    let mut writer = RecordingWriter::default();
    let mut publication = Publication::default();
    publication
        .sync_for_test(&mut writer, crate::subc::tool_views::bus_snapshot())
        .await
        .unwrap();
    assert_eq!(
        writer.writes().len(),
        crate::subc::tool_views::SHIPPED.len()
    );
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
                .writes()
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
    let writes = writer.writes().len();
    publication
        .sync_for_test(&mut writer, crate::subc::tool_views::bus_snapshot())
        .await
        .unwrap();
    assert_eq!(writer.writes().len(), writes);
}
