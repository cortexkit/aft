//! Request policy decisions, not transport delivery receipts. Audit metadata is
//! checked against every held key before reaching the sink; an audit failure
//! prevents a newly authorized request from being enqueued.
use super::State;
use hyper::body::Incoming;
use hyper::Request;
use secret_rules::audit::{AuditReason, AuditRecordV1, Decision};
use secret_rules::location::{location_kind_token, LabelOutcome, LabelRule};
use secret_rules::target::ConnectAuthority;
use std::sync::atomic::{AtomicU64, Ordering};

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub(super) struct RequestAudit<'a> {
    state: &'a State,
    authority: ConnectAuthority,
    request_id: String,
    labels: Vec<(Option<usize>, String)>,
    rules: Vec<LabelRule>,
    done: bool,
}

impl<'a> RequestAudit<'a> {
    pub(super) fn new(
        state: &'a State,
        authority: &ConnectAuthority,
        request: &Request<Incoming>,
    ) -> Self {
        let target = request
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/");
        let mut labels = Vec::new();
        for label in &state.config.scanned_labels {
            if mentions(target.as_bytes(), label)
                || request
                    .headers()
                    .values()
                    .any(|v| mentions(v.as_bytes(), label))
            {
                labels.push((None, label.clone()));
            }
        }
        Self {
            state,
            authority: authority.clone(),
            request_id: REQUEST_ID.fetch_add(1, Ordering::Relaxed).to_string(),
            labels,
            rules: Vec::new(),
            done: false,
        }
    }

    pub(super) fn decide(
        &mut self,
        rules: &[LabelRule],
        outcomes: &[LabelOutcome],
    ) -> Result<(), String> {
        self.rules = rules.to_vec();
        for (index, label) in &mut self.labels {
            *index = rules.iter().position(|rule| &rule.label == label);
        }
        self.emit(Some(outcomes))
    }

    pub(super) fn mention(&mut self, label: &str) {
        if !self.labels.iter().any(|(_, known)| known == label) {
            self.labels.push((None, label.to_owned()));
        }
    }

    pub(super) fn refused_for(&mut self, label: &str) {
        if !self.done
            && !label.is_empty()
            && !self
                .state
                .config
                .scanned_labels
                .iter()
                .any(|known| known == label)
        {
            self.labels.clear();
            self.labels.push((None, label.to_owned()));
        }
        if !self.done {
            let _ = self.emit(None);
        }
    }

    fn emit(&mut self, outcomes: Option<&[LabelOutcome]>) -> Result<(), String> {
        if self.done {
            return Ok(());
        }
        self.done = true;
        let mut failure = None;
        for (index, label) in &self.labels {
            let (decision, reason) = match outcomes.and_then(|o| index.and_then(|i| o.get(i))) {
                Some(LabelOutcome::Substituted { .. }) => {
                    (Decision::Substituted, AuditReason::Matched)
                }
                Some(LabelOutcome::NotWholeValue) => {
                    (Decision::Literal, AuditReason::NotWholeValue)
                }
                Some(_) => (Decision::Literal, AuditReason::OutOfScope),
                None if outcomes.is_some() => (Decision::Literal, AuditReason::OutOfScope),
                None => (Decision::Refused, AuditReason::Refused),
            };
            let mut record = AuditRecordV1::new(
                crate::secret::vault::now_ms(),
                label,
                decision,
                reason,
                &self.request_id,
                &self.state.config.origin,
            )
            .with_scope(Some(self.state.config.authorization_scope.to_scope_ref()));
            if let Some(index) = index {
                let rule = &self.rules[*index];
                record = record.with_rule(
                    &self.authority.host,
                    self.authority.port,
                    &rule.path_prefix,
                    location_kind_token(&rule.location),
                );
            } else {
                record.host = Some(self.authority.host.clone());
                record.port = Some(self.authority.port);
            }
            let safe = {
                let patterns = self.state.patterns.lock();
                let keys = patterns
                    .iter()
                    .map(|(_, k)| k.as_slice())
                    .collect::<Vec<_>>();
                record.to_json_line_with_keys(&keys).is_ok()
            };
            if !safe || self.state.config.vault.audit(&record).is_err() {
                failure.get_or_insert_with(|| label.clone());
            }
        }
        match failure {
            Some(label) => Err(label),
            None => Ok(()),
        }
    }
}

impl Drop for RequestAudit<'_> {
    fn drop(&mut self) {
        if !self.done {
            let _ = self.emit(None);
        }
    }
}

pub(super) fn mentions(bytes: &[u8], label: &str) -> bool {
    let needle = secret_rules::placeholder::placeholder_text(label);
    for start in 0..bytes.len() {
        if bytes[start] != b'{' && bytes[start] != b'%' {
            continue;
        }
        let mut cursor = start;
        let mut matched = true;
        for expected in needle.bytes() {
            let byte = if bytes.get(cursor) == Some(&b'%') {
                match (
                    bytes.get(cursor + 1).and_then(|b| hex(*b)),
                    bytes.get(cursor + 2).and_then(|b| hex(*b)),
                ) {
                    (Some(a), Some(b)) => {
                        cursor += 3;
                        (a << 4) | b
                    }
                    _ => {
                        matched = false;
                        break;
                    }
                }
            } else {
                let Some(byte) = bytes.get(cursor) else {
                    matched = false;
                    break;
                };
                cursor += 1;
                *byte
            };
            if byte != expected {
                matched = false;
                break;
            }
        }
        if matched {
            return true;
        }
    }
    false
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
