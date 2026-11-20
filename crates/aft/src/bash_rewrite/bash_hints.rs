//! Native code-search steering, kept separate from result/runner delivery.

use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const HINT_PREFIX: &str = "DO NOT search code by running grep/rg in ";

/// Split a bash reply into its body and the closing lines AFT adds after the
/// command output: an optional `[task <id>]` line followed by `trailer` when
/// one is present. The tail is returned with its own line breaks.
pub(crate) fn split_reply_tail<'a>(output: &'a str, trailer: Option<&str>) -> (&'a str, &'a str) {
    let mut tail_start = output.len();
    if let Some(trailer) = trailer.filter(|trailer| !trailer.is_empty()) {
        if output.ends_with(trailer) {
            tail_start -= trailer.len();
        }
    }
    let before = output[..tail_start].trim_end_matches('\n');
    let last_line_start = before.rfind('\n').map_or(0, |index| index + 1);
    if crate::list_surfaces::bash::is_foreground_task_line(&before[last_line_start..]) {
        tail_start = last_line_start;
    }
    (&output[..tail_start], &output[tail_start..])
}

pub(crate) fn is_terminal_native_reply(response: &crate::protocol::Response) -> bool {
    response.data.get("task_id").is_some()
        && response.data.get("exit_code").is_some()
        && matches!(
            response.data["status"].as_str(),
            Some("completed" | "failed" | "killed" | "timed_out" | "fate_unknown")
        )
}

/// Standalone hosts carry the steer in the result. Runner routes instead send
/// the decision through session.send and must not call this delivery helper.
pub(crate) fn append_nudge(response: &mut crate::protocol::Response, nudge: &str) {
    let trailer = response
        .data
        .get(crate::list_surfaces::bash::WIRE_KEY)
        .and_then(|value| {
            serde_json::from_value::<crate::list_envelope::ListEnvelope>(value.clone()).ok()
        })
        .map(|envelope| crate::list_surfaces::bash::envelope_trailer(&envelope));
    if let Some(text) = response.data["output"].as_str() {
        let (body, tail) = split_reply_tail(text, trailer.as_deref());
        let combined = format!(
            "{}\n\n{nudge}{}{tail}",
            body.trim_end_matches('\n'),
            if tail.is_empty() { "" } else { "\n" }
        );
        response.data["output"] = serde_json::Value::String(combined);
    }
}

/// Decide whether a completed native shell call gets a nudge. Launch/status
/// replies are excluded by callers; rewritten output already carries its steer.
pub(crate) fn code_search_nudge(
    output: &str,
    command: &str,
    project_root: Option<&Path>,
    workdir: Option<&str>,
    search_tool: Option<&str>,
    grep_tool: Option<&str>,
    shell_tool: &str,
) -> Option<String> {
    if output.is_empty()
        || output.contains(HINT_PREFIX)
        || (search_tool.is_none() && grep_tool.is_none())
    {
        return None;
    }
    let windows = cfg!(windows);
    let statements = statements(command, windows)?;
    let stages: Vec<_> = statements
        .iter()
        .filter_map(|s| first_stage(s, windows))
        .collect();
    let searches: Vec<_> = stages
        .iter()
        .filter_map(|s| {
            let (head, end) = token(s, 0, windows)?;
            matches!(head.as_str(), "grep" | "rg").then(|| (*s, operands(s, end, windows)))
        })
        .collect();
    if searches.is_empty()
        || searches
            .iter()
            .all(|(_, paths)| !paths.is_empty() && paths.iter().all(|p| dynamic(p)))
    {
        return None;
    }
    if let Some(root) = project_root.filter(|root| !root.to_string_lossy().trim().is_empty()) {
        let root = root.to_str().map(str::trim).map(Path::new).unwrap_or(root);
        let root = resolve(Path::new(""), root);
        let mut cwd = Some(resolve(&root, Path::new(workdir.unwrap_or("."))));
        let mut suppress = true;
        for stage in stages {
            let Some((head, end)) = token(stage, 0, windows) else {
                continue;
            };
            if head == "cd" {
                cwd = next_cwd(cwd.as_deref(), stage, end, windows);
                continue;
            }
            if !matches!(head.as_str(), "grep" | "rg") {
                continue;
            }
            let Some(cwd) = cwd.as_deref().filter(|cwd| inside(&root, cwd)) else {
                continue;
            };
            let paths = operands(stage, end, windows);
            if paths.is_empty() {
                suppress = false;
                break;
            }
            let mut existing = false;
            let mut suppressed = false;
            for path in paths {
                if dynamic(&path) {
                    continue;
                }
                let path = resolve(cwd, Path::new(&path));
                if !inside(&root, &path) {
                    continue;
                }
                let Ok(metadata) = std::fs::metadata(path) else {
                    continue;
                };
                existing = true;
                if metadata.is_file() || metadata.modified().is_ok_and(fresh) {
                    suppressed = true;
                    break;
                }
            }
            if suppressed {
                break;
            }
            if existing {
                suppress = false;
                break;
            }
        }
        if suppress {
            return None;
        }
    }
    Some(crate::bash_rewrite::footer::grep_search_hint(
        search_tool,
        grep_tool.unwrap_or("grep"),
        shell_tool,
    ))
}

fn fresh(modified: SystemTime) -> bool {
    fresh_at(modified, SystemTime::now())
}

fn fresh_at(modified: SystemTime, now: SystemTime) -> bool {
    // Match Date.now()'s millisecond precision, including sub-ms mtime slack.
    let now_ms = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as f64;
    let modified_ms = modified
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
        * 1000.0;
    let age = now_ms - modified_ms;
    age > -1.0 && age < 60_000.0
}

fn resolve(base: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let absolute = if joined.is_absolute() {
        joined
    } else {
        std::env::current_dir().unwrap_or_default().join(joined)
    };
    let mut out = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn inside(root: &Path, path: &Path) -> bool {
    let mut path = path.components();
    root.components().all(|r| {
        path.next().is_some_and(|p| {
            if cfg!(windows) {
                r.as_os_str().to_string_lossy().to_lowercase()
                    == p.as_os_str().to_string_lossy().to_lowercase()
            } else {
                r == p
            }
        })
    })
}

fn next_cwd(cwd: Option<&Path>, stage: &str, mut at: usize, windows: bool) -> Option<PathBuf> {
    loop {
        at = skip_spaces(stage, at);
        if at == stage.len() {
            break;
        }
        let Some((part, end)) = token(stage, at, windows) else {
            break;
        };
        if end <= at {
            break;
        }
        at = end;
        if part == "-" {
            return None;
        }
        if part.starts_with('-') {
            continue;
        }
        if part.contains('$') || part.contains('`') {
            return None;
        }
        let expanded = if part == "~" || part.starts_with("~/") {
            std::env::home_dir()?.join(part.trim_start_matches('~').trim_start_matches('/'))
        } else {
            PathBuf::from(part)
        };
        return if expanded.is_absolute() {
            Some(resolve(Path::new(""), &expanded))
        } else {
            Some(resolve(cwd?, &expanded))
        };
    }
    std::env::home_dir().map(|home| resolve(Path::new(""), &home))
}

fn dynamic(path: &str) -> bool {
    path.starts_with('~') || path.contains('$') || path.contains('`')
}

fn operands(stage: &str, mut at: usize, windows: bool) -> Vec<String> {
    let mut paths = Vec::new();
    let mut pattern = false;
    let mut options_ended = false;
    let mut option_value = None;
    loop {
        at = skip_spaces(stage, at);
        if at == stage.len() {
            break;
        }
        let Some((part, end)) = token(stage, at, windows) else {
            break;
        };
        if end <= at {
            break;
        }
        at = end;
        if part.chars().all(|c| c.is_ascii_digit()) && stage[end..].starts_with(['<', '>']) {
            break;
        }
        if let Some(is_pattern) = option_value.take() {
            if is_pattern {
                pattern = true;
            }
            continue;
        }
        if !options_ended && part == "--" {
            options_ended = true;
            continue;
        }
        if !options_ended && part.starts_with('-') {
            if matches!(part.as_str(), "-e" | "--regexp" | "-f" | "--file") {
                option_value = Some(true);
            } else if part.starts_with("--regexp=")
                || part.starts_with("--file=")
                || (part.len() > 2 && (part.starts_with("-e") || part.starts_with("-f")))
            {
                pattern = true;
            } else if [
                "-A",
                "-B",
                "-C",
                "-m",
                "--after-context",
                "--before-context",
                "--context",
                "--max-count",
                "--include",
                "--exclude",
                "--exclude-dir",
                "-g",
                "--glob",
                "-t",
                "--type",
                "-T",
                "--type-not",
            ]
            .contains(&part.as_str())
            {
                option_value = Some(false);
            }
            continue;
        }
        if !pattern {
            pattern = true;
        } else {
            paths.push(part);
        }
    }
    paths
}

fn skip_spaces(input: &str, start: usize) -> usize {
    input[start..]
        .char_indices()
        .find(|(_, c)| !c.is_whitespace())
        .map_or(input.len(), |(at, _)| start + at)
}

#[derive(Clone, Copy, PartialEq)]
enum Quote {
    None,
    Single,
    Double,
}

fn escapes(next: Option<char>, quote: Quote, windows: bool) -> bool {
    if quote == Quote::Double {
        next.is_some_and(|c| "$`\"\\\n".contains(c))
    } else {
        !windows
    }
}

fn statements(command: &str, windows: bool) -> Option<Vec<&str>> {
    let chars: Vec<_> = command.char_indices().collect();
    let mut out = Vec::new();
    let (mut start, mut i, mut depth) = (0, 0, 0usize);
    let mut quote = Quote::None;
    let (mut escaped, mut backtick) = (false, false);
    while i < chars.len() {
        let (at, c) = chars[i];
        let next = chars.get(i + 1).map(|(_, c)| *c);
        i += 1;
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Quote::Single {
            if c == '\'' {
                quote = Quote::None;
            }
            continue;
        }
        if quote == Quote::Double {
            if c == '\\' && escapes(next, quote, windows) {
                escaped = true;
            } else if c == '"' {
                quote = Quote::None;
            }
            continue;
        }
        if backtick {
            if c == '`' {
                backtick = false;
            }
            continue;
        }
        if c == '\\' && escapes(next, quote, windows) {
            escaped = true;
            continue;
        }
        match c {
            '\'' => {
                quote = Quote::Single;
                continue;
            }
            '"' => {
                quote = Quote::Double;
                continue;
            }
            '`' => {
                backtick = true;
                continue;
            }
            '(' => {
                depth += 1;
                continue;
            }
            ')' => {
                depth = depth.saturating_sub(1);
                continue;
            }
            _ => {}
        }
        if depth > 0 {
            continue;
        }
        if (c == '&' && next == Some('&')) || (c == '|' && next == Some('|')) {
            out.push(&command[start..at]);
            i += 1;
            start = at + 2;
        } else if matches!(c, ';' | '\n' | '&') {
            out.push(&command[start..at]);
            start = at + 1;
        }
    }
    if quote != Quote::None || backtick || escaped {
        return None;
    }
    out.push(&command[start..]);
    Some(out)
}

fn first_stage(command: &str, windows: bool) -> Option<&str> {
    let mut chars = command.char_indices().peekable();
    let mut quote = Quote::None;
    let mut pipe = command.len();
    while let Some((at, c)) = chars.next() {
        if quote == Quote::Single {
            if c == '\'' {
                quote = Quote::None;
            }
            continue;
        }
        if c == '\\' && escapes(chars.peek().map(|(_, c)| *c), quote, windows) {
            chars.next();
            continue;
        }
        if c == '`' {
            return None;
        }
        if quote == Quote::Double {
            if c == '"' {
                quote = Quote::None;
            }
            continue;
        }
        match c {
            '\'' => quote = Quote::Single,
            '"' => quote = Quote::Double,
            '|' if chars.peek().is_some_and(|(_, c)| *c == '|') => {
                chars.next();
            }
            '|' if pipe == command.len() => pipe = at,
            _ => {}
        }
    }
    (quote == Quote::None).then(|| command[..pipe].trim())
}

fn token(command: &str, start: usize, windows: bool) -> Option<(String, usize)> {
    let mut chars = command[start..].char_indices().peekable();
    while chars.peek().is_some_and(|(_, c)| c.is_whitespace()) {
        chars.next();
    }
    let mut out = String::new();
    let mut quote = Quote::None;
    while let Some((at, c)) = chars.next() {
        if quote == Quote::Single {
            if c == '\'' {
                quote = Quote::None;
            } else {
                out.push(c);
            }
            continue;
        }
        if c == '\\' && escapes(chars.peek().map(|(_, c)| *c), quote, windows) {
            if let Some((_, c)) = chars.next() {
                if quote != Quote::Double || c != '\n' {
                    out.push(c);
                }
            } else {
                out.push('\\');
            }
            continue;
        }
        if c == '`' {
            return None;
        }
        if quote == Quote::Double {
            if c == '"' {
                quote = Quote::None;
            } else {
                out.push(c);
            }
            continue;
        }
        if c.is_whitespace() || "|;&<>".contains(c) {
            return Some((out, start + at));
        }
        match c {
            '\'' => quote = Quote::Single,
            '"' => quote = Quote::Double,
            _ => out.push(c),
        }
    }
    (quote == Quote::None).then_some((out, command.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SEARCH_HINT: &str = "DO NOT search code by running grep/rg in bash — it is unindexed, unranked, and serial. Use the `aft_search` tool instead (it auto-routes concepts, identifiers, regex, and literals).";
    const GREP_HINT: &str = "DO NOT search code by running grep/rg in shell — it is unindexed, unranked, and serial. Use the `grep` tool instead (indexed and ranked).";

    fn decide(
        output: &str,
        command: &str,
        root: Option<&Path>,
        cwd: Option<&str>,
    ) -> Option<String> {
        code_search_nudge(
            output,
            command,
            root,
            cwd,
            Some("aft_search"),
            Some("grep"),
            "bash",
        )
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/app.ts"), "foo\n").unwrap();
        old(&root.path().join("src"));
        root
    }

    fn old(path: &Path) {
        filetime::set_file_mtime(
            path,
            filetime::FileTime::from_system_time(SystemTime::now() - Duration::from_secs(61)),
        )
        .unwrap();
    }

    #[test]
    fn code_search_detection_matches_plugin_statements_and_filters() {
        for command in [
            r#"grep -nE "x" src/"#,
            "grep foo file.ts | head",
            "rg -n pat",
            "cd packages/x && grep -rn foo .",
            "cd \"my dir\" && rg 'p' .",
            "\"grep\" -n pat file",
            "grep pat file || true",
            "grep \"a|b\" file | head",
            "cd x; grep foo",
            "false || grep pat",
            "echo y & rg pat",
            "cd ~/proj && echo '=== marker ===' && grep -rn foo src/ | head -20",
            "cd ~/proj\necho '=== marker ==='\ngrep -rn foo src/ | head -20",
        ] {
            assert_eq!(
                decide("matches", command, None, None).as_deref(),
                Some(SEARCH_HINT),
                "{command}"
            );
        }
        for command in [
            "bun test | grep fail",
            "cargo build 2>&1 | rg error",
            "echo hi | grep h",
            "make test | grep -i pass",
            "ls -la",
            "FOO=1 grep pat file",
            "2>&1 grep pat",
            "cd \"unclosed && grep foo",
            "grep 'unclosed",
            "grep foo `unclosed",
            "cd x && bun test | grep fail",
            "echo 'grep is mentioned here' && ls",
            "echo $(echo hi; grep foo)",
            "echo 'a; grep foo'",
            "echo \"a && rg foo\"",
        ] {
            assert_eq!(decide("matches", command, None, None), None, "{command}");
        }
    }

    #[test]
    fn code_search_nudge_output_and_tool_names_are_pinned() {
        assert_eq!(decide("", "grep foo", None, None), None);
        assert_eq!(
            decide(&format!("matches\n\n{SEARCH_HINT}"), "grep foo", None, None),
            None
        );
        assert_eq!(decide(GREP_HINT, "grep foo", None, None), None);
        assert_eq!(
            code_search_nudge("matches", "rg foo", None, None, None, Some("grep"), "shell")
                .as_deref(),
            Some(GREP_HINT)
        );
        assert_eq!(
            code_search_nudge(
                "matches",
                "rg foo",
                None,
                None,
                Some("search"),
                None,
                "bash"
            )
            .unwrap(),
            SEARCH_HINT.replace("aft_search", "search")
        );
        assert_eq!(
            code_search_nudge("matches", "rg foo", None, None, None, None, "bash"),
            None
        );
        for root in [None, Some(Path::new("")), Some(Path::new("   "))] {
            assert_eq!(
                decide("matches", "rg foo missing.ts", root, None).as_deref(),
                Some(SEARCH_HINT)
            );
        }
    }

    #[test]
    fn code_search_nudge_standalone_appends_once_only_to_native_terminal_results() {
        let root = fixture();
        let context = crate::subc_format::BashCodeSearchHint {
            command: "cd src && echo marker && rg foo . | head".into(),
            workdir: None,
            search_tool: Some("aft_search".into()),
            grep_tool: Some("grep".into()),
            shell_tool: "bash".into(),
            runner_notice: None,
        };
        let mut response = crate::protocol::Response::success(
            "call",
            serde_json::json!({"task_id": "native", "status": "completed", "exit_code": 0, "output": "matches"}),
        );
        context.deliver(&mut response, root.path());
        assert_eq!(response.data["output"], format!("matches\n\n{SEARCH_HINT}"));
        let before = serde_json::to_vec(&response).unwrap();
        context.deliver(&mut response, root.path());
        assert_eq!(serde_json::to_vec(&response).unwrap(), before);
        for data in [
            serde_json::json!({"task_id": "native", "status": "running", "output": "background launch"}),
            serde_json::json!({"task_id": "native", "status": "completed", "output": "background launch"}),
            serde_json::json!({"output": "rewritten grep result"}),
        ] {
            let mut response = crate::protocol::Response::success("call", data.clone());
            context.deliver(&mut response, root.path());
            assert_eq!(response.data, data);
        }
    }

    #[test]
    fn code_search_suppression_matches_plugin_paths_options_and_redirections() {
        let root = fixture();
        let root = Some(root.path());
        for command in [
            "grep -rn foo",
            "grep foo ./src",
            "grep $PAT ./src",
            "cd src && grep -n foo .",
            "grep -f ~/pat.txt foo src/",
            "grep -e foo src",
            "rg --regexp=foo src",
            "rg -g '*.ts' -t typescript foo src",
            "grep -rn foo src 2>/dev/null",
            "grep -rn foo 2>/dev/null",
            "grep -rn foo missing.ts src | head -5",
        ] {
            assert_eq!(
                decide("hit", command, root, None).as_deref(),
                Some(SEARCH_HINT),
                "{command}"
            );
        }
        for command in [
            "grep foo /etc/hosts",
            "grep x ~/.config/opencode/aft.jsonc",
            "grep -rn foo $DIR",
            "L=~/.local/share/x/logs/a.log; grep -n abc123 $L | tail -8 | cut -c1-230",
            "grep x \"$HOME/foo\"",
            "grep x '$PROJECT/foo'",
            "cd x && echo y && grep z ~/outside/f",
            "cd /other/repo/src && grep -n foo tools/bash.ts",
            "cd /other/repo\necho scanning\ngrep -n foo tools/bash.ts",
            "cd $DIR && grep -n foo file.ts",
            "cd - && grep -n foo",
            "rg -n foo missing.ts",
            "grep -n foo src/missing.ts | head -5",
            "grep -n foo src/app.ts | head -5",
            "grep -n foo src/app.ts src/ | head -5",
            "grep '' src/app.ts",
            "grep -rn foo /etc/hosts 2>/dev/null",
        ] {
            assert_eq!(decide("hit", command, root, None), None, "{command}");
        }
        assert_eq!(decide("hit", "rg foo $DIR", None, None), None);
        assert_eq!(
            decide("hit", "cd '' && rg foo src", root, None).as_deref(),
            Some(SEARCH_HINT)
        );
    }

    #[test]
    fn code_search_suppression_resolves_workdir_separately_from_project() {
        let root = fixture();
        let outside = fixture();
        for cwd in [
            "src".to_string(),
            root.path().join("src").to_string_lossy().into_owned(),
        ] {
            assert!(decide("hit", "rg foo .", Some(root.path()), Some(&cwd)).is_some());
            assert_eq!(
                decide(
                    "hit",
                    "cd .. && rg foo src/app.ts",
                    Some(root.path()),
                    Some(&cwd)
                ),
                None
            );
        }
        assert_eq!(
            decide(
                "hit",
                "rg foo src",
                Some(root.path()),
                outside.path().to_str()
            ),
            None
        );
        std::fs::create_dir(root.path().join("fresh")).unwrap();
        assert_eq!(decide("hit", "rg foo fresh", Some(root.path()), None), None);
        old(&root.path().join("src/app.ts"));
        assert_eq!(
            decide("hit", "rg foo src/app.ts", Some(root.path()), None),
            None
        );
    }

    #[test]
    fn code_search_lexer_preserves_windows_backslashes() {
        for windows in [false, true] {
            for (input, expected) in [
                (r#""a\b""#, r"a\b"),
                (r#""a\"b""#, "a\"b"),
                (r#""a\$b""#, "a$b"),
                (r#""a\\b""#, r"a\b"),
                ("\"a\\\nb\"", "ab"),
            ] {
                assert_eq!(
                    token(input, 0, windows),
                    Some((expected.into(), input.len()))
                );
            }
        }
        assert_eq!(token(r"C:\a\b\", 0, true), Some((r"C:\a\b\".into(), 7)));
        assert_eq!(token(r"r\g foo .", 0, false).unwrap().0, "rg");
        assert_eq!(token(r"r\g foo .", 0, true).unwrap().0, r"r\g");
        for command in [
            r"rg foo C:\proj\src\| head",
            r"rg foo C:\proj\src\; echo done",
            r"rg foo C:\proj\src\&& echo done",
        ] {
            let parts = statements(command, true).unwrap();
            let stage = first_stage(parts[0], true).unwrap();
            assert_eq!(operands(stage, 2, true), vec![r"C:\proj\src\"]);
        }
    }

    #[test]
    fn code_search_freshness_matches_plugin_clock_slack() {
        let now = UNIX_EPOCH + Duration::from_millis(1_800_000_000_000);
        assert!(fresh_at(now + Duration::from_micros(600), now));
        assert!(!fresh_at(now + Duration::from_secs(5), now));
        assert!(fresh_at(now - Duration::from_millis(59_999), now));
        assert!(!fresh_at(now - Duration::from_secs(60), now));
    }
}
