/// The full result of a tool call: the COMPLETE dispatch Response carried VERBATIM,
/// plus the server-rendered agent-facing text (what the deleted TS formatters used to produce).
/// Oracle #1: carry the WHOLE Response — promote nothing, drop nothing (preview_diff, attachments,
/// status_bar, bg_completions, lsp_diagnostics, code, message, candidates, … all ride inside `response`).
#[derive(Debug)]
pub struct ToolCallResult {
    pub text: String,
    pub response: crate::protocol::Response,
}
