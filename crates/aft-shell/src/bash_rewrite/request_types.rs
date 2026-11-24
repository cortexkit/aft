#[derive(Debug, Clone, PartialEq)]
pub struct RewriteRequest {
    pub request_id: String,
    pub command: String,
    pub session_id: Option<String>,
    pub rule_id: &'static str,
    pub branch_id: &'static str,
    pub decision_class_id: &'static str,
    pub params: Value,
    /// The caller's names for AFT's command and patch tools, which the footer
    /// of a rewritten command names (see [`crate::host_tool_names`]).
    pub host_tool_names: crate::host_tool_names::HostToolNames,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclineReason {
    pub rule_id: Option<&'static str>,
    pub branch_id: &'static str,
    pub decision_class_id: &'static str,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RewriteDecision {
    Accept(RewriteRequest),
    Decline(DeclineReason),
}

/// A rewrite rule owns both the pre-execution shape decision and the typed
/// request it emits. Implementations keep parsing in the decision phase so an
/// accepted request has one stable request ID and one decision class.
pub trait RewriteRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn decide(
        &self,
        command: &str,
        request_id: &str,
        session_id: Option<&str>,
        ctx: &AppContext,
    ) -> RewriteDecision;
    fn execute(&self, request: &RewriteRequest, ctx: &AppContext) -> Response;
}
