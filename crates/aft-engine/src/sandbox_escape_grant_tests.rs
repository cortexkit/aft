use super::*;

#[test]
fn sandbox_escape_grant_outlives_the_ordinary_host_prompt_window() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("project");
    let storage = fixture.path().join("storage");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&storage).unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.clone());
    config.storage_dir = Some(storage.clone());
    let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let principal = AuthenticatedPrincipal::FirstParty;
    let environment = approved_payload_environment(&HashMap::new(), &fixture.path().join("tmp"));
    let id = mint_host_escalation_grant(
        &ctx,
        &principal,
        b"pwd",
        &root,
        &root,
        Path::new("/bin/sh"),
        &environment,
        &storage,
        "escape-ttl",
    )
    .unwrap();
    retain_host_escalation_for_escape(&ctx, &id).unwrap();
    let store = ctx.escalation_grants().lock();
    let expires = store.grants[&id].expires_at;
    assert!(expires > Instant::now() + ESCALATION_GRANT_TTL);
    assert!(
        expires >= Instant::now() + Duration::from_millis(crate::sandbox_escape::EXPIRES_IN_MS)
    );
}
