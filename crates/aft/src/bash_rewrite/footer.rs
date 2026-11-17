/// Add the bash-rewrite footer to a tool's human-readable output.
///
/// We intentionally keep this a single short line. The agent already wrote
/// the original bash command in their tool call so we don't repeat it; the
/// only signal we need to surface is "next time, use the dedicated tool"
/// because that's what changes their behavior on the next turn.
///
/// `shell_tool` is the name the host registered AFT's command tool under
/// (`bash` everywhere except OpenCode 2, which calls it `shell`).
pub fn add_footer(response_output: &str, replacement_tool: &str, shell_tool: &str) -> String {
    format!("{response_output}\n\nPrefer `{replacement_tool}` tool over {shell_tool}.")
}

/// Add the code-search redirect footer to a rewritten grep/rg output.
///
/// Unlike the generic [`add_footer`], this is an enforced "DO NOT" steer
/// because running grep/rg through bash to find code is the antipattern we
/// most want to kill: it's unindexed, unranked, and serial. When the plugin
/// registered `aft_search` for this surface (`aft_search_registered`), the
/// footer points there (it auto-routes concepts, identifiers, regex, AND
/// literals); otherwise it points at the indexed `grep` tool. `shell_tool` is
/// the host's name for AFT's command tool, as in [`add_footer`].
pub fn add_grep_footer(
    response_output: &str,
    aft_search_registered: bool,
    shell_tool: &str,
) -> String {
    let hint = grep_search_hint(
        aft_search_registered.then_some("aft_search"),
        "grep",
        shell_tool,
    );
    format!("{response_output}\n\n{hint}")
}

/// Render the same search guidance for rewritten and native searches using the
/// tool names available to the caller.
pub(crate) fn grep_search_hint(
    search_tool: Option<&str>,
    grep_tool: &str,
    shell_tool: &str,
) -> String {
    let hint = if let Some(search_tool) = search_tool {
        format!("Use the `{search_tool}` tool instead (it auto-routes concepts, identifiers, regex, and literals).")
    } else {
        format!("Use the `{grep_tool}` tool instead (indexed and ranked).")
    };
    format!(
        "DO NOT search code by running grep/rg in {shell_tool} \u{2014} it is unindexed, unranked, and serial. {hint}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footers_name_the_hosts_command_tool() {
        assert_eq!(
            add_footer("out", "read", "bash"),
            "out\n\nPrefer `read` tool over bash."
        );
        assert_eq!(
            add_footer("out", "read", "shell"),
            "out\n\nPrefer `read` tool over shell."
        );
        assert_eq!(
            add_grep_footer("out", true, "bash"),
            "out\n\nDO NOT search code by running grep/rg in bash \u{2014} it is unindexed, unranked, and serial. Use the `aft_search` tool instead (it auto-routes concepts, identifiers, regex, and literals)."
        );
        assert_eq!(
            add_grep_footer("out", false, "shell"),
            "out\n\nDO NOT search code by running grep/rg in shell \u{2014} it is unindexed, unranked, and serial. Use the `grep` tool instead (indexed and ranked)."
        );
    }
}
