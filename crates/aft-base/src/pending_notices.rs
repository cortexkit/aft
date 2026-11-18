use serde_json::Value;
use std::sync::Arc;

/// Runner delivery supplied by the host; lower response types retain only its handle.
pub trait NoticeSink: std::fmt::Debug + Send + Sync {
    fn send(&self, notice: &Notice);
}

/// Queued decisions. An absent route uses standalone output delivery.
#[derive(Debug, Clone, Default)]
pub struct PendingNotices {
    pub route: Option<Arc<dyn NoticeSink>>,
    pub items: Vec<Notice>,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub text: String,
    pub template: Option<(&'static str, Value)>,
    pub target: Target,
}

#[derive(Debug, Clone, Copy)]
pub enum Target {
    RewriteFooter,
    BashOutput,
    Text,
}

impl Notice {
    pub fn template(id: &'static str, slots: Value, text: String, target: Target) -> Self {
        Self {
            text,
            template: Some((id, slots)),
            target,
        }
    }

    pub fn diagnostic(text: String) -> Self {
        Self {
            text,
            template: None,
            target: Target::Text,
        }
    }
}
