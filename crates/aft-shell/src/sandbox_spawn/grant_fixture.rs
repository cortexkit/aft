use super::*;

#[cfg(all(target_os = "linux", feature = "test-support"))]
pub fn linux_profile_for_scope_test(
    strict: SandboxProfile,
    home: &Path,
) -> Result<SandboxProfile, String> {
    compute_linux_profile(strict, home, &[], &[], &mut SecureReadDirectoryLister)
}

#[cfg(feature = "test-support")]
pub fn relaxed_profile_for_test(
    host: &dyn SandboxHost,
    home: &Path,
    roots: &[PathBuf],
    checkout: &Path,
    storage: &Path,
    task: &Path,
    temp: &Path,
) -> Result<SandboxProfile, String> {
    build_relaxed_profile(
        host,
        home,
        roots,
        &[resolve_git_policy(checkout)?],
        storage,
        task,
        temp,
    )
}

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
