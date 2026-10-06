//! Narrow, syntax-based detection of obvious synthetic CPU load.

use tree_sitter::Node;

use super::scan::{collect_commands, parse_tree};

type Words = [Option<String>];

/// This is not a shell sandbox: dynamic command names and unparseable syntax
/// retain their existing handling. Literal words are decoded by the rewrite
/// parser, never searched inside arguments, scripts, or quoted prose.
pub(crate) fn is_synthetic_load(source: &str) -> bool {
    let Some(tree) = parse_tree(source) else {
        return false;
    };
    let mut commands = Vec::new();
    collect_commands(tree.root_node(), &mut commands);
    commands.into_iter().any(|node| {
        let words = command_words(source, node);
        let Some((words, indirect)) = executable_words(&words) else {
            return false;
        };
        match command_name(&words) {
            Some("stress" | "stress-ng") => true,
            // Only a directly connected, tiny head is an exception for yes.
            // One gigabyte is bounded too, but is still substantial CPU load.
            Some("yes") => indirect || !small_head_pipeline(source, node),
            Some("cat" | "dd" | "pv") => {
                let reads_device = words.iter().skip(1).any(|word| {
                    word.as_deref().is_some_and(|word| {
                        is_load_device(word.strip_prefix("if=").unwrap_or(word))
                    })
                }) || redirects(source, node, |op, fd, target| {
                    op == "<" && (fd.is_empty() || fd == "0") && is_load_device(target)
                });
                let discards_output = words
                    .iter()
                    .skip(1)
                    .any(|word| word.as_deref() == Some("of=/dev/null"))
                    || redirects(source, node, |op, fd, target| {
                        target == "/dev/null"
                            && matches!(op, ">" | ">>" | "&>" | "&>>")
                            && (fd.is_empty() || fd == "1" || op.starts_with('&'))
                    });
                reads_device
                    && (discards_output
                        || (!redirects(source, node, |op, fd, _| {
                            matches!(op, ">" | ">>" | ">&" | "&>" | "&>>" | "<>")
                                && (fd.is_empty() || fd == "1" || op.starts_with('&'))
                        }) && pipes_to_device_reader(source, node)))
            }
            _ => false,
        }
    })
}

fn is_load_device(word: &str) -> bool {
    matches!(word, "/dev/zero" | "/dev/urandom")
}

fn text<'a>(source: &'a str, node: Node<'_>) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

fn literal_word(source: &str, node: Node<'_>) -> Option<String> {
    let word = text(source, node);
    // Empty braces are literal in bash, not brace expansion. The rewrite
    // parser deliberately declines all braces, but xargs/parallel commonly
    // use `{}` as their replacement marker. Escape only this literal pair
    // on the fallback path; quoted words were already decoded above it.
    let mut parsed = crate::bash_rewrite::parser::parse(word)
        .or_else(|| crate::bash_rewrite::parser::parse(&word.replace("{}", "\\{\\}")))?;
    (parsed.args.len() == 1 && parsed.appends_to.is_none() && parsed.heredoc.is_none())
        .then(|| parsed.args.remove(0))
}

fn command_words(source: &str, node: Node<'_>) -> Vec<Option<String>> {
    let mut words = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "command_elements" {
            words.extend(command_words(source, child));
        } else if !matches!(
            child.kind(),
            "variable_assignment"
                | "file_redirect"
                | "heredoc_redirect"
                | "herestring_redirect"
                | "redirection"
        ) {
            // Keep unknown words in place so a dynamic command name or option
            // value cannot make a later argument look like the executable.
            words.push(literal_word(source, child));
        }
    }
    words
}

fn command_name(words: &Words) -> Option<&str> {
    words.first()?.as_deref()?.rsplit('/').next()
}

/// Peel only known execution wrappers and launcher options. In particular,
/// `command -v yes` is a lookup, and `xargs -Iyes echo yes` executes echo.
fn executable_words(words: &Words) -> Option<(Vec<Option<String>>, bool)> {
    let mut words = words.to_vec();
    let mut indirect = false;
    'wrappers: loop {
        let name = command_name(&words)?;
        let index = match name {
            "env" | "nice" | "timeout" | "command" | "xargs" | "parallel" => {
                let mut index = 1;
                while let Some(word) = words.get(index) {
                    let word = word.as_deref()?;
                    if word == "--" {
                        index += 1;
                        break;
                    }
                    if name == "env" {
                        // Unlike a quoted argument to echo, env -S explicitly
                        // splits its operand into executable words. Decode only
                        // literal split strings with the existing word parser.
                        let split = if matches!(word, "-S" | "--split-string") {
                            Some((words.get(index + 1)?.as_deref()?, index + 2))
                        } else {
                            word.strip_prefix("--split-string=")
                                .or_else(|| word.strip_prefix("-S"))
                                .map(|value| (value, index + 1))
                        };
                        if let Some((value, rest)) = split {
                            let parsed = crate::bash_rewrite::parser::parse(value)?;
                            let mut expanded = vec![Some("env".to_string())];
                            expanded.extend(parsed.args.into_iter().map(Some));
                            expanded.extend_from_slice(&words[rest..]);
                            words = expanded;
                            continue 'wrappers;
                        }
                        if word == "-" {
                            index += 1;
                            continue;
                        }
                    }
                    if name == "env" && word.contains('=') && !word.starts_with('-') {
                        index += 1;
                        continue;
                    }
                    if !word.starts_with('-') || word == "-" {
                        break;
                    }
                    if matches!(word, "--help" | "--version")
                        || (name == "command"
                            && !word.starts_with("--")
                            && (word.contains('v') || word.contains('V')))
                    {
                        return None;
                    }
                    index += 1;
                    if option_takes_value(name, word) {
                        words.get(index)?;
                        index += 1;
                    }
                }
                if name == "timeout" {
                    // timeout's first operand is a duration, not its command.
                    words.get(index)?;
                    index += 1;
                }
                if matches!(name, "xargs" | "parallel") {
                    indirect = true;
                }
                index
            }
            _ => return Some((words, indirect)),
        };
        words = words.get(index..)?.to_vec();
    }
}

fn option_takes_value(name: &str, option: &str) -> bool {
    match name {
        "env" => matches!(option, "-u" | "--unset" | "-C" | "--chdir"),
        "nice" => matches!(option, "-n" | "--adjustment"),
        "timeout" => matches!(option, "-s" | "--signal" | "-k" | "--kill-after"),
        "xargs" => matches!(
            option,
            "-a" | "--arg-file"
                | "-d"
                | "--delimiter"
                | "-E"
                | "-I"
                | "-L"
                | "-n"
                | "--max-args"
                | "-P"
                | "--max-procs"
                | "-s"
                | "--max-chars"
        ),
        "parallel" => matches!(
            option,
            "-j" | "--jobs"
                | "-P"
                | "--max-procs"
                | "-a"
                | "--arg-file"
                | "-S"
                | "--sshlogin"
                | "-I"
                | "--replace"
                | "-n"
                | "--max-args"
                | "-N"
                | "-L"
                | "--max-lines"
                | "-s"
                | "--max-chars"
                | "-d"
                | "--delimiter"
                | "--colsep"
                | "--timeout"
                | "--delay"
                | "--joblog"
                | "--results"
                | "--tmpdir"
                | "--workdir"
                | "--sshloginfile"
                | "--env"
                | "--filter"
                | "--header"
                | "--tagstring"
                | "--rpl"
                | "--halt"
                | "--load"
                | "--memfree"
                | "--block"
                | "--recstart"
                | "--recend"
                | "--retries"
        ),
        _ => false,
    }
}

fn pipeline_next(mut node: Node<'_>) -> Option<Node<'_>> {
    loop {
        let parent = node.parent()?;
        if parent.kind() == "pipeline" {
            let mut cursor = parent.walk();
            let mut stages = parent.named_children(&mut cursor);
            stages.find(|child| child.id() == node.id())?;
            return stages.next();
        }
        if !matches!(parent.kind(), "redirected_statement" | "subshell") {
            return None;
        }
        node = parent;
    }
}

fn stage_command(mut node: Node<'_>) -> Option<Node<'_>> {
    while matches!(
        node.kind(),
        "redirected_statement" | "subshell" | "compound_statement"
    ) {
        if node.kind() != "redirected_statement" && node.named_child_count() != 1 {
            return None;
        }
        node = node.named_child(0)?;
    }
    (node.kind() == "command").then_some(node)
}

fn pipes_to_device_reader(source: &str, node: Node<'_>) -> bool {
    let Some(next) = pipeline_next(node).and_then(stage_command) else {
        return false;
    };
    let words = command_words(source, next);
    executable_words(&words)
        .is_some_and(|(words, _)| matches!(command_name(&words), Some("cat" | "dd" | "pv")))
}

/// Visit redirects on the command and enclosing groups, but not those on a
/// different pipeline stage or inside a command substitution.
fn redirects(
    source: &str,
    mut node: Node<'_>,
    mut matches: impl FnMut(&str, &str, &str) -> bool,
) -> bool {
    loop {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() != "file_redirect" {
                continue;
            }
            let mut redirect_cursor = child.walk();
            let mut op = "";
            let mut fd = "";
            let mut target = None;
            for part in child.children(&mut redirect_cursor) {
                if !part.is_named() {
                    op = part.kind();
                } else if part.kind() == "file_descriptor" {
                    fd = text(source, part);
                } else {
                    target = literal_word(source, part);
                }
            }
            if matches(op, fd, target.as_deref().unwrap_or("")) {
                return true;
            }
        }
        let Some(parent) = node.parent() else {
            return false;
        };
        // The grammar may wrap a whole pipeline for a trailing redirect.
        // Such a redirect belongs to its last stage, not every producer.
        if parent.kind() == "pipeline"
            && parent
                .named_child((parent.named_child_count() - 1) as u32)
                .is_some_and(|last| last.id() == node.id())
        {
            node = parent;
            continue;
        }
        if !matches!(
            parent.kind(),
            "redirected_statement" | "subshell" | "compound_statement" | "list"
        ) {
            return false;
        }
        node = parent;
    }
}

fn small_head_pipeline(source: &str, node: Node<'_>) -> bool {
    let Some(head) = pipeline_next(node).and_then(stage_command) else {
        return false;
    };
    // Redirects can sever the pipe: `yes > file | head -n 3` or
    // `yes | head -n 3 < file` does not bound the producer at all.
    if redirects(source, node, |_, _, _| true) || redirects(source, head, |_, _, _| true) {
        return false;
    }
    let words = command_words(source, head);
    let Some((words, indirect)) = executable_words(&words) else {
        return false;
    };
    if indirect || command_name(&words) != Some("head") {
        return false;
    }
    // Allow at most 100 lines or 64 KiB, with a literal positive decimal
    // count and no file operands. Default head emits ten lines. Larger or
    // negative counts, suffixes and dynamic bounds are deliberately refused.
    let (count, max) = match words.get(1).and_then(Option::as_deref) {
        None if words.len() == 1 => return true,
        Some("-n" | "--lines") if words.len() == 3 => (words[2].as_deref(), 100),
        Some("-c" | "--bytes") if words.len() == 3 => (words[2].as_deref(), 65_536),
        Some(option) if words.len() == 2 => {
            if let Some(count) = option
                .strip_prefix("-n")
                .or_else(|| option.strip_prefix("--lines="))
            {
                (Some(count), 100)
            } else if let Some(count) = option
                .strip_prefix("-c")
                .or_else(|| option.strip_prefix("--bytes="))
            {
                (Some(count), 65_536)
            } else {
                (option.strip_prefix('-'), 100)
            }
        }
        _ => return false,
    };
    count.is_some_and(|count| {
        !count.is_empty()
            && count.bytes().all(|byte| byte.is_ascii_digit())
            && count
                .parse::<u64>()
                .is_ok_and(|count| count > 0 && count <= max)
    })
}

#[cfg(test)]
mod tests {
    use super::is_synthetic_load;

    #[test]
    fn synthetic_load_refuses_obvious_load_forms() {
        let refused = [
            "yes > /dev/null &",
            "for i in $(seq 18); do yes > /dev/null & done",
            "/usr/bin/yes | head -c1G",
            "nice -n 19 stress-ng --cpu 8",
            "dd if=/dev/zero of=/dev/null",
            "cat /dev/urandom > /dev/null",
            "seq 8 | xargs -P8 -I{} yes",
            "stress --cpu 8",
            "(echo start; yes > /dev/null) &",
            "echo $(yes > /dev/null)",
            "echo `stress --cpu 8`",
            "while true; do /usr/bin/stress-ng --cpu 8; done",
            "if true; then yes; fi",
            "env FOO=bar /usr/bin/nice -n19 timeout -s KILL 10s command -- /usr/bin/yes",
            "env -i -u FOO nice --adjustment=19 timeout --kill-after=1 10 stress",
            "nice -19 yes",
            "command -p stress-ng --cpu 8",
            "FOO=bar yes > /dev/null",
            "nice -n \"$PRIORITY\" stress-ng --cpu 8",
            "env - yes > /dev/null",
            "env -S 'nice -n 19 yes'",
            "env --split-string=yes",
            "'yes' > '/dev/null'",
            "y\\es > /dev/null",
            "seq 8 | xargs -P 8 -I {} /usr/bin/yes",
            "seq 8 | xargs --max-procs=8 --replace={} env FOO=bar yes",
            "xargs --replace yes",
            "xargs --eof stress",
            "xargs --max-lines stress-ng --cpu 8",
            "parallel -j8 yes ::: 1 2 3",
            "parallel --jobs 8 nice -n 19 stress-ng --cpu 8 ::: 1 2",
            "parallel dd if=/dev/zero of=/dev/null ::: 1 2",
            "parallel --joblog log.txt yes ::: 1 2",
            "xargs -n1 cat /dev/zero > /dev/null",
            "cat < /dev/zero > /dev/null",
            "cat 0< /dev/zero 1> /dev/null",
            "(cat /dev/zero) > /dev/null",
            "{ cat /dev/zero; } > /dev/null",
            "pv /dev/zero > /dev/null",
            "pv < /dev/urandom | cat > /dev/null",
            "cat /dev/zero | dd of=/dev/null",
            "cat /dev/zero | pv | cat > /dev/null",
            "cat /dev/zero | cat",
            "dd if=/dev/urandom | pv",
            "cat '/dev/zero' > '/dev/null'",
            "cat /dev/zero | /usr/bin/cat",
            "cat /dev/zero | (cat)",
            "dd if=/dev/zero of=/dev/null count=1",
            "yes > /dev/null | head -n 3",
            "yes > file | head -n 3",
            "yes | head -n 3 < file",
            "yes | head -n 1000000",
            "yes | head -n 0",
            "yes | head -n -3",
            "yes | head -c 65537",
            "yes | head -n 3 /dev/zero",
            "yes | head -n \"$LIMIT\"",
        ];
        let missed: Vec<_> = refused
            .into_iter()
            .filter(|command| !is_synthetic_load(command))
            .collect();
        assert!(missed.is_empty(), "unrefused synthetic load: {missed:?}");
    }

    #[test]
    fn synthetic_load_allows_non_load_and_small_bounded_forms() {
        let allowed = [
            "echo yes",
            "grep -r stress src",
            "cargo test stress",
            "cargo test stress_",
            "git log --grep yes",
            "echo 'yes > /dev/null &'",
            "echo \"stress-ng --cpu 8\"",
            "cat <<'EOF'\nyes > /dev/null\nEOF",
            "env FOO=yes echo stress",
            "command -v yes",
            "command -V stress",
            "command -pv yes",
            "env -S 'echo yes'",
            "env --help yes",
            "nice --version stress",
            "timeout --help yes",
            "xargs --help yes",
            "xargs -Iyes echo yes",
            "xargs -I{} grep stress src",
            "parallel echo yes ::: 1 2 3",
            "parallel --jobs 8 echo stress ::: 1 2",
            "parallel --joblog stress echo yes ::: 1 2",
            "cat /dev/zero > fixture.bin",
            "dd if=/dev/urandom of=fixture.bin count=1",
            "cat input.txt > /dev/null",
            "cat /dev/zero 2> /dev/null",
            "cat /dev/zero > fixture.bin | cat",
            "echo /dev/zero > /dev/null",
            "cat /dev/zero | head -c 16",
            "cat /dev/zero | grep pattern",
            "echo cat /dev/zero | cat > /dev/null",
            "yes | head -n 3",
            "/usr/bin/yes | /usr/bin/head -n3",
            "env FOO=bar yes | head -c 1024",
            "yes | head",
            "yes | head -3",
            "yes | head --lines=100",
            "yes | head --bytes=65536",
            "echo $(yes | head -n 3)",
            // Do not substitute a load refusal for existing parse-failure behavior.
            "yes > /dev/null; if",
            "echo 'unterminated yes",
            "$COMMAND > /dev/null",
        ];
        let refused: Vec<_> = allowed
            .into_iter()
            .filter(|command| is_synthetic_load(command))
            .collect();
        assert!(refused.is_empty(), "unexpected load refusals: {refused:?}");
    }
}
