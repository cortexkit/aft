use super::*;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunSelector {
    pub(crate) owner: String,
    #[serde(rename = "ref")]
    pub(crate) scope_ref: String,
    pub(crate) epoch: u64,
}

pub struct QueuedBotResponse {
    pub(super) response: Pin<Box<dyn Future<Output = Result<Value, QueuedResponseError>> + Send>>,
}

#[derive(Debug)]
pub enum QueuedResponseError {
    NotSent(Refusal),
    Unknown(String),
}

/// The closure may only be built around the partner's real reserved queue slot.
pub struct ReservedBotRequest {
    pub(super) commit: Box<dyn FnOnce() -> Result<QueuedBotResponse, Refusal> + Send>,
}
