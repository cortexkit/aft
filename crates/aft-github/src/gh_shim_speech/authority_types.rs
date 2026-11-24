use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSeal {
    pub(crate) child_pid: u32,
    pub(crate) child_start_time_ms: u64,
    pub(crate) birth_identity: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub(crate) app_handle_id: String,
    pub(crate) repo_binding_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySnapshot {
    pub(crate) artifact_id: String,
    pub(crate) version: u64,
    pub(crate) digest: Vec<u8>,
    pub(crate) signed_envelope: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorityRecord {
    pub(crate) incarnation: String,
    pub(crate) minting_run: String,
    pub(crate) harness: String,
    pub(crate) session_id: String,
    pub(crate) task_id: String,
    pub(crate) project_root: String,
    pub(crate) launch: LaunchAuthority,
    pub(crate) policy: Option<PolicySnapshot>,
    pub(crate) bindings: std::collections::BTreeMap<String, Binding>,
    pub(crate) seal: Option<ExecutionSeal>,
    pub(crate) ineligible_reason: Option<String>,
}
