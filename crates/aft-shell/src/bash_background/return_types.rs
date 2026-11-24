use super::*;

/// What happened to one requested path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnedPath {
    /// The path as the caller wrote it, relative to the command's workdir.
    pub path: String,
    /// The same path relative to the checkout, with `/` separators, as the
    /// runner names the files it reports as changed. `None` when the workdir
    /// is not in a checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_path: Option<String>,
    pub outcome: ReturnOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ReturnOutcome {
    /// The verified file now sits at the requested local path.
    Returned { bytes: u64 },
    /// Nothing was written for this path.
    NotReturned { reason: NotReturned },
}
