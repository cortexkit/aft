//! The fake vault tests script.
//!
//! Compiled only under `cfg(test)` or the `secret-test-fake` feature, which no
//! release profile enables (`scripts/check-secret-test-fake.sh` checks that in
//! CI). Nothing in config, an environment variable or a project setting can
//! select it: a caller has to install it explicitly.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use secret_rules::audit::AuditRecordV1;
use secret_rules::location::LabelRule;

use super::{AuditError, Material, MaterialError, Resolution, SecretVault};
use crate::secret::wire::ApprovalRequest;
use crate::secret::SecretScope;

/// Everything a test makes the fake say, plus the record of what it was asked.
#[derive(Default)]
pub struct FakeVaultScript {
    /// The answer `resolve` gives, per label. A label with no entry answers
    /// `Unavailable`, which is also what an unconfigured vault says.
    pub resolutions: HashMap<String, Resolution>,
    /// Bytes `material` hands out, per label, taking one entry per call so a
    /// test can rotate a key under one grant. A label runs out of bytes and
    /// then repeats the last one.
    pub material: HashMap<String, Vec<Vec<u8>>>,
}

struct FakeVaultState {
    script: FakeVaultScript,
    /// One line per `resolve`, in call order.
    resolves: Vec<String>,
    /// One line per `material`, in call order.
    materials: Vec<String>,
    /// Material calls already served per label, so rotation advances.
    served: HashMap<String, usize>,
    /// Audit records the fake collected, in emission order.
    audit: Vec<AuditRecordV1>,
}

impl std::fmt::Debug for FakeVaultState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeVaultState")
            .field("resolves", &self.resolves.len())
            .field("materials", &self.materials.len())
            .field("audit_len", &self.audit.len())
            .finish()
    }
}

/// The test-only vault.
#[derive(Debug, Clone)]
pub struct FakeVault {
    state: Arc<Mutex<FakeVaultState>>,
    resolve_count: Arc<AtomicU64>,
    material_count: Arc<AtomicU64>,
    audit_count: Arc<AtomicU64>,
}

impl Default for FakeVault {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeVault {
    /// A vault that answers `Unavailable` to everything.
    pub fn new() -> Self {
        Self::with_script(FakeVaultScript::default())
    }

    /// A vault that answers from `script`.
    pub fn with_script(script: FakeVaultScript) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeVaultState {
                script,
                resolves: Vec::new(),
                materials: Vec::new(),
                served: HashMap::new(),
                audit: Vec::new(),
            })),
            resolve_count: Arc::new(AtomicU64::new(0)),
            material_count: Arc::new(AtomicU64::new(0)),
            audit_count: Arc::new(AtomicU64::new(0)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, FakeVaultState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Make `resolve` answer `label` with `resolution`.
    pub fn set_resolution(&self, label: &str, resolution: Resolution) {
        self.lock()
            .script
            .resolutions
            .insert(label.to_string(), resolution);
    }

    /// The bytes `material` hands out for `label`, in call order.
    pub fn set_material(&self, label: &str, values: Vec<Vec<u8>>) {
        self.lock()
            .script
            .material
            .insert(label.to_string(), values);
    }

    /// The labels `material` was asked about, in order.
    pub fn materials(&self) -> Vec<String> {
        self.lock().materials.clone()
    }

    /// How many times `resolve` was called.
    pub fn resolve_calls(&self) -> u64 {
        self.resolve_count.load(Ordering::SeqCst)
    }

    /// How many times `material` was called.
    pub fn material_calls(&self) -> u64 {
        self.material_count.load(Ordering::SeqCst)
    }

    /// How many times `audit` was called.
    pub fn audit_calls(&self) -> u64 {
        self.audit_count.load(Ordering::SeqCst)
    }

    /// The audit records the fake collected.
    pub fn audit_records(&self) -> Vec<AuditRecordV1> {
        self.lock().audit.clone()
    }

    /// The labels of the collected audit records with their decisions, in
    /// emission order.
    pub fn audit_summary(&self) -> Vec<(String, secret_rules::audit::Decision)> {
        self.lock()
            .audit
            .iter()
            .map(|record| (record.label.clone(), record.decision))
            .collect()
    }

    /// The next bytes for `label`, honouring rotation.
    fn next_material(&self, label: &str) -> Result<Material, MaterialError> {
        let mut state = self.lock();
        state.materials.push(label.to_string());
        let Some(count) = state.script.material.get(label).map(Vec::len) else {
            return Err(MaterialError::Refused {
                reason: "the fake vault has no material for this label".into(),
            });
        };
        if count == 0 {
            return Err(MaterialError::Refused {
                reason: "the fake vault's material list is empty".into(),
            });
        }
        let index = {
            // Rotation advances per label, so a second request under the same
            // grant can see different bytes.
            let served = state.served.entry(label.to_string()).or_insert(0);
            let index = (*served).min(count - 1);
            *served += 1;
            index
        };
        let bytes = state.script.material[label][index].clone();
        Ok(Material::new(bytes))
    }
}

impl SecretVault for FakeVault {
    fn resolve(&self, _scope: &SecretScope, label: &str) -> Resolution {
        self.resolve_count.fetch_add(1, Ordering::SeqCst);
        let mut state = self.lock();
        state.resolves.push(label.to_string());
        state
            .script
            .resolutions
            .get(label)
            .cloned()
            .unwrap_or(Resolution::Unavailable)
    }

    fn material(
        &self,
        _scope: &SecretScope,
        label: &str,
        _grant_id: &str,
    ) -> Result<Material, MaterialError> {
        self.material_count.fetch_add(1, Ordering::SeqCst);
        self.next_material(label)
    }

    fn audit(&self, record: &AuditRecordV1) -> Result<(), AuditError> {
        self.audit_count.fetch_add(1, Ordering::SeqCst);
        // The crate's writer runs the key check, so a record that reaches the
        // fake's collection has passed the same test a production sink would.
        if !record.scope.as_ref().is_none_or(|scope| scope.is_valid()) {
            return Err(AuditError::Invalid {
                detail: "scope is not plain text".into(),
            });
        }
        self.lock().audit.push(record.clone());
        Ok(())
    }
}

/// A granted resolution for tests.
pub fn granted(rule: LabelRule, grant_id: &str) -> Resolution {
    Resolution::Granted {
        rule,
        grant_id: grant_id.to_string(),
        expires_at_ms: 0,
    }
}

/// A pending approval for tests.
pub fn approval(request_id: &str) -> ApprovalRequest {
    ApprovalRequest {
        request_id: request_id.to_string(),
        statement_b64url: "c3RhdGVtZW50".into(),
        expires_at_ms: 0,
    }
}
