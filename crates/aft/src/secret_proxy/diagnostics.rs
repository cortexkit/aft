use super::State;

/// Only fixed codes cross the diagnostic boundary, never transport errors or
/// request data that could contain credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Reason {
    RequestLimit,
    Stopped,
    GenerationInvalid,
    HostMismatch,
    InvalidRequest,
    UnscannedLabel,
    BodyInvalid,
    UpstreamConnectTimeout,
    UpstreamConnect,
    UpstreamTls,
    UpstreamHttp,
    VaultUnavailable,
    LocationMismatch,
    MaterialInvalid,
    MaterialInstall,
    AuditFailed,
    UpstreamSend,
    ResponseInvalid,
    ScrubFailed,
    ConnectMethod,
    ProxyAuth,
    ConnectAuthority,
    TlsConfiguration,
}

impl Reason {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::RequestLimit => "request_limit",
            Self::Stopped => "stopped",
            Self::GenerationInvalid => "generation_invalid",
            Self::HostMismatch => "host_mismatch",
            Self::InvalidRequest => "invalid_request",
            Self::UnscannedLabel => "unscanned_label",
            Self::BodyInvalid => "body_invalid",
            Self::UpstreamConnectTimeout => "upstream_connect_timeout",
            Self::UpstreamConnect => "upstream_connect",
            Self::UpstreamTls => "upstream_tls",
            Self::UpstreamHttp => "upstream_http",
            Self::VaultUnavailable => "vault_unavailable",
            Self::LocationMismatch => "location_mismatch",
            Self::MaterialInvalid => "material_invalid",
            Self::MaterialInstall => "material_install",
            Self::AuditFailed => "audit_failed",
            Self::UpstreamSend => "upstream_send",
            Self::ResponseInvalid => "response_invalid",
            Self::ScrubFailed => "scrub_failed",
            Self::ConnectMethod => "connect_method",
            Self::ProxyAuth => "proxy_auth",
            Self::ConnectAuthority => "connect_authority",
            Self::TlsConfiguration => "tls_configuration",
        }
    }
}

pub(super) struct Refusal {
    pub(super) reason: Reason,
    pub(super) label: String,
}

impl Refusal {
    pub(super) fn new(reason: Reason) -> Self {
        Self::for_label(reason, "")
    }

    pub(super) fn for_label(reason: Reason, label: &str) -> Self {
        Self {
            reason,
            label: label.to_owned(),
        }
    }
}

pub(super) fn log_refusal(state: &State, reason: Reason, label: &str, host: &str) {
    // Even a syntactically valid label or host can spell an installed key.
    // Scrub the two permitted metadata fields and quote them to keep one line.
    let safe = |value: &str| {
        state
            .scrub(value.as_bytes())
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_else(|| "[redacted]".into())
    };
    let line = format!(
        "secret proxy refused reason={} label={:?} host={:?}",
        reason.code(),
        safe(label),
        safe(host),
    );
    #[cfg(test)]
    state.test_refusals.lock().push(line.clone());
    crate::slog_warn!("{line}");
}
