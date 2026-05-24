/// Add the bash-rewrite footer to a tool's human-readable output.
///
/// We intentionally keep this a single short line. The agent already wrote
/// the original bash command in their tool call so we don't repeat it; the
/// only signal we need to surface is "next time, use the dedicated tool"
/// because that's what changes their behavior on the next turn.
pub fn add_footer(response_output: &str, replacement_tool: &str) -> String {
    format!(
        "{}\n\nPrefer `{}` tool over bash.",
        response_output, replacement_tool
    )
}
