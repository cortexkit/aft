use super::*;

#[derive(Clone, Debug)]
pub struct StoredAuthority {
    pub(crate) record: AuthorityRecord,
    pub(crate) ticket_digest: Option<Vec<u8>>,
    pub(crate) phase: String,
    pub(crate) generation: i64,
}

#[derive(Debug)]
pub enum GateError {
    BeforeSend(Refusal),
    AfterSend,
}
