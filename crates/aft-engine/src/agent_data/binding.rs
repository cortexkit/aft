//! Entorhinal's registered ref is opaque. Resolve it to the exact AFT native
//! root/harness/session key without minting another container identity.

use super::participant::{BindingFailure, RefResolver, ResolvedSession};
use cortexkit_agent_identity::AgentSessionForRefReply;
use cortexkit_role_agent_data::refs::Ref;
use std::path::PathBuf;
use std::sync::Arc;

pub trait SessionLookup: Send + Sync {
    fn session_for_ref(&self, reference: &Ref) -> Result<AgentSessionForRefReply, BindingFailure>;
}

#[derive(Debug, Clone)]
pub struct NativeIdentity {
    pub root: PathBuf,
    pub harness: String,
    pub session: String,
    pub storage_harness: crate::harness::Harness,
}

/// A read of AFT's retained identities, not a new registration surface. The
/// resolver verifies the returned identity again instead of trusting aliases.
pub trait NativeIdentityLookup: Send + Sync {
    fn lookup(
        &self,
        root: &std::path::Path,
        harness: &str,
        session: &str,
    ) -> Result<NativeIdentity, BindingFailure>;
}

pub struct EntorhinalRefs {
    pub registry: Arc<dyn SessionLookup>,
    pub native: Arc<dyn NativeIdentityLookup>,
}

pub fn matches_native(registered: &AgentSessionForRefReply, native: &NativeIdentity) -> bool {
    let Some(root) = &registered.project_root else {
        return false;
    };
    let Ok(expected) = cortexkit_paths::ProjectRootId::from_path(root) else {
        return false;
    };
    let Ok(actual) = cortexkit_paths::ProjectRootId::from_path(&native.root) else {
        return false;
    };
    expected == actual
        && registered.harness.as_deref() == Some(native.harness.as_str())
        && registered.session_id.as_deref() == Some(native.session.as_str())
}

impl RefResolver for EntorhinalRefs {
    fn resolve(&self, reference: &Ref) -> Result<ResolvedSession, BindingFailure> {
        let registered = self.registry.session_for_ref(reference)?;
        let harness = registered
            .harness
            .as_deref()
            .ok_or(BindingFailure::NotBindable)?;
        let session = registered
            .session_id
            .as_deref()
            .ok_or(BindingFailure::NotBindable)?;
        if registered.session_ref != reference.r#ref
            || registered.ref_kind != reference.ref_kind
            || !matches!(
                (registered.ref_kind.as_str(), harness),
                ("opencode-session", "opencode") | ("pi-session", "pi")
            )
        {
            return Err(BindingFailure::NotBindable);
        }
        let root = registered
            .project_root
            .as_ref()
            .ok_or(BindingFailure::NotBindable)?;
        let native = self
            .native
            .lookup(std::path::Path::new(root), harness, session)?;
        if !matches_native(&registered, &native) {
            return Err(BindingFailure::NotBindable);
        }
        Ok(ResolvedSession {
            registered_ref: registered.session_ref.clone(),
            project_root: native.root,
            harness: harness.to_owned(),
            session: session.to_owned(),
            storage_harness: native.storage_harness,
        })
    }
}
