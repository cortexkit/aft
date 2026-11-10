use serde::Serialize;

/// Original request text retained before lane-specific normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawQuery(String);

impl RawQuery {
    pub fn new(query: impl Into<String>) -> Self {
        Self(query.into())
    }

    pub fn original_query(&self) -> &str {
        &self.0
    }
}

/// Byte range of a delimiter's verbatim interior in [`RawQuery`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn extract<'a>(&self, raw_query: &'a str) -> Option<&'a str> {
        raw_query.get(self.start..self.end)
    }
}

/// The five classification facts passed unchanged to lane planning: the first
/// embedded span, qualifying exact-token count, path presence, log-marker
/// presence, and identifier-shaped-token presence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueryFacts {
    pub embedded_span: Option<Span>,
    pub exact_input_tokens: usize,
    pub has_path_token: bool,
    pub has_timestamp_or_pid: bool,
    pub has_identifier_token: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub index: usize,
    pub text: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenVariant {
    pub token_index: usize,
    pub text: String,
}
