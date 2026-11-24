use super::*;

pub struct SpeechTicket {
    pub(crate) prepared: crate::gh_shim_speech::service::PreparedLaunch,
    pub(crate) incarnation: String,
    pub(crate) stored: AtomicBool,
    pub(crate) background: AtomicBool,
    pub(crate) active: AtomicBool,
    pub(crate) failure: Mutex<Option<crate::gh_shim_speech::Refusal>>,
}

impl SpeechTicket {
    #[doc(hidden)]
    pub fn database_path(&self) -> &std::path::Path {
        self.prepared.context.service.store.path()
    }
}
