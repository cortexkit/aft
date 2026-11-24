use super::*;

#[cfg(feature = "test-support")]
impl ToolLaunchPolicy<'_> {
    #[doc(hidden)]
    pub fn plan_for_test(&self) -> &SpawnPlan {
        &self.launch().plan
    }
}

pub(crate) fn create(ctx: &dyn SandboxHost, ttl: Duration) -> (String, PathBuf) {
    let layout = crate::shell_env_provider::PreflightLayout::create().unwrap();
    let path = layout.path().to_path_buf();
    let id = "esc_provider_layout".to_owned();
    ctx.escalation_grants().inner_lock().grants.insert(
        id.clone(),
        EscalationGrant {
            principal: AuthenticatedPrincipal::FirstParty,
            root: PathBuf::from("/provider-root"),
            digest: blake3::hash(b"unused payload"),
            expires_at: Instant::now() + ttl,
            consumed: false,
            refused: false,
            payload_reclaimed: false,
            session_dir: path.join("nonexistent-session"),
            task_id: "nonexistent-task".into(),
            provider_snapshot: None,
        },
    );
    save_provider_approval(
        ctx,
        &id,
        ProviderApprovalSnapshot {
            input: HashMap::new(),
            environment: ChildEnvironment::new(),
            failure: None,
            preflight_layout: Some(layout),
        },
    );
    (id, path)
}
