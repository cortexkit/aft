use std::collections::HashSet;

use serde_json::Value;

use super::client::LspEvent;
use super::registry::ServerKind;

pub(super) const ANALYSIS_TOKEN_LIMIT: usize = 256;

pub(super) fn uses_analysis_progress(kind: &ServerKind) -> bool {
    matches!(kind, ServerKind::Python)
        || matches!(kind, ServerKind::Custom(id) if id.as_ref() == "basedpyright")
}

#[derive(Clone, Copy)]
pub(super) enum Transition {
    Announce,
    Begin,
    End,
}

impl Transition {
    pub(super) fn from_event(event: &LspEvent) -> Option<(&Value, Self)> {
        let (params, transition) = match event {
            LspEvent::Notification {
                method,
                params: Some(params),
                ..
            } if method == "$/progress" => {
                let transition = match params.pointer("/value/kind")?.as_str()? {
                    "begin" => Self::Begin,
                    "end" => Self::End,
                    _ => return None,
                };
                (params, transition)
            }
            LspEvent::ServerRequest {
                method,
                params: Some(params),
                ..
            } if method == "window/workDoneProgress/create" => (params, Self::Announce),
            _ => return None,
        };
        Some((params.get("token")?, transition))
    }
}

#[derive(Clone, Default)]
pub(super) struct AnalysisTokens {
    active: HashSet<String>,
    announced: HashSet<String>,
}

impl AnalysisTokens {
    pub(super) fn running(&self) -> bool {
        !self.active.is_empty() || !self.announced.is_empty()
    }

    pub(super) fn record(&mut self, token: String, transition: Transition) -> bool {
        match transition {
            Transition::Announce => {
                self.announced.insert(token);
            }
            Transition::Begin => {
                self.announced.remove(&token);
                self.active.insert(token);
            }
            Transition::End => return self.active.remove(&token) && !self.running(),
        }
        false
    }

    fn needs_new_slot(&self, token: &str) -> bool {
        !self.active.contains(token) && !self.announced.contains(token)
    }

    fn at_limit(&self) -> bool {
        self.active.len() + self.announced.len() >= ANALYSIS_TOKEN_LIMIT
            && self.active.union(&self.announced).count() >= ANALYSIS_TOKEN_LIMIT
    }
}

/// Reader-side state for a lossy interval. The FIFO keeps exact ordering on
/// the ordinary path. During recovery no diagnostic publications are applied
/// between the missing transitions. Only the last start, the last END that
/// left no work running, and the tokens still live affect analysis state.
#[derive(Clone, Default)]
pub(super) struct AnalysisProgressShadow {
    pub(super) tokens: AnalysisTokens,
    pub(super) last_start: u64,
    pub(super) last_completion: u64,
    pub(super) last_transition: u64,
}

impl AnalysisProgressShadow {
    pub(super) fn record(&mut self, token: String, transition: Transition, order: u64) -> bool {
        if !matches!(transition, Transition::End) {
            if self.tokens.needs_new_slot(&token) && self.tokens.at_limit() {
                return false;
            }
            self.last_start = order;
        }
        if self.tokens.record(token, transition) {
            self.last_completion = order;
        }
        self.last_transition = order;
        true
    }
}
