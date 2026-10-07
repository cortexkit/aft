#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub args: Vec<String>,
    pub heredoc: Option<String>,
    pub appends_to: Option<String>,
}

pub fn parse(command: &str) -> Option<ParsedCommand> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }

    let (header, heredoc) = split_heredoc(command)?;
    let parsed = tokenize(header, heredoc)?;

    if parsed.args.is_empty() {
        return None;
    }

    Some(parsed)
}

fn split_heredoc(command: &str) -> Option<(&str, Option<String>)> {
    let Some(op_start) = find_heredoc_operator(command)? else {
        return Some((command, None));
    };

    let after_operator = op_start + 2;
    // Tab-stripping heredocs need different body and terminator handling.
    if command[after_operator..].starts_with('-') {
        return None;
    }
    let after_spaces = skip_horizontal_space(command, after_operator);
    let (delimiter, consumed) = tokenize_word(&command[after_spaces..])?;
    let delimiter_end = after_spaces + consumed;
    let quoted = command[after_spaces..delimiter_end].contains(['\'', '"', '\\']);
    if delimiter.is_empty() {
        return None;
    }

    let line_start = match command[delimiter_end..].find('\n') {
        Some(offset) => delimiter_end + offset + 1,
        None => return None,
    };

    if !command[delimiter_end..line_start].trim().is_empty() || delimiter.contains('\n') {
        return None;
    }
    let body = &command[line_start..];
    // A terminator must occupy the whole line, not just a line prefix.
    let mut offset = 0;
    let mut terminator = None;
    for line in body.split_inclusive('\n') {
        if line.strip_suffix('\n').unwrap_or(line) == delimiter {
            terminator = Some(offset);
            break;
        }
        offset += line.len();
    }
    let offset = terminator?;
    let content = &body[..offset];
    let rest_start = line_start + offset + delimiter.len();
    // Quote removal on any part of the delimiter disables body expansion.
    if !quoted && content.contains(['$', '`', '\\']) {
        return None;
    }

    let rest = &command[rest_start..];
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    if !rest.trim().is_empty() {
        return None;
    }

    Some((&command[..op_start], Some(content.to_string())))
}

fn find_heredoc_operator(command: &str) -> Option<Option<usize>> {
    let mut quote = Quote::None;
    let mut chars = command.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '`' => return None,
                ch if !word_character_is_literal(ch, quote) => return None,
                '\\' => {
                    chars.next();
                }
                _ => {}
            },
            Quote::None => match ch {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '`' => return None,
                ch if !word_character_is_literal(ch, quote) => return None,
                '\\' => {
                    chars.next();
                }
                '<' if matches!(chars.peek(), Some((_, '<'))) => return Some(Some(idx)),
                _ => {}
            },
        }
    }

    if quote == Quote::None {
        Some(None)
    } else {
        None
    }
}

fn tokenize(header: &str, heredoc: Option<String>) -> Option<ParsedCommand> {
    let mut args = Vec::new();
    let mut token = String::new();
    let mut quote = Quote::None;
    let mut appends_to = None;
    let mut chars = header.char_indices().peekable();

    while let Some((_, ch)) = chars.next() {
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    token.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '`' => return None,
                ch if !word_character_is_literal(ch, quote) => return None,
                '\\' => match chars.next() {
                    Some((_, escaped)) => push_double_quoted_backslash(&mut token, escaped),
                    None => token.push('\\'),
                },
                _ => token.push(ch),
            },
            Quote::None => match ch {
                c if c.is_whitespace() => push_token(&mut args, &mut token),
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '\\' => match chars.next() {
                    Some((_, escaped)) => token.push(escaped),
                    None => token.push('\\'),
                },
                '`' => return None,
                ch if !word_character_is_literal(ch, quote) => return None,
                '|' | ';' => return None,
                '&' if matches!(chars.peek(), Some((_, '&'))) => return None,
                '>' if matches!(chars.peek(), Some((_, '>'))) => {
                    chars.next();
                    push_token(&mut args, &mut token);
                    if appends_to.is_some() {
                        return None;
                    }
                    appends_to = Some(read_next_redirect_target(header, &mut chars)?);
                    if has_non_space_remainder(&mut chars) {
                        return None;
                    }
                    break;
                }
                '>' | '<' => return None,
                _ => token.push(ch),
            },
        }
    }

    if quote != Quote::None {
        return None;
    }
    push_token(&mut args, &mut token);

    if heredoc.is_some() && appends_to.is_none() {
        return None;
    }

    Some(ParsedCommand {
        args,
        heredoc,
        appends_to,
    })
}

fn push_token(args: &mut Vec<String>, token: &mut String) {
    if !token.is_empty() {
        args.push(std::mem::take(token));
    }
}

fn read_next_redirect_target(
    header: &str,
    chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>,
) -> Option<String> {
    while matches!(chars.peek(), Some((_, c)) if c.is_whitespace()) {
        chars.next();
    }

    let start = chars.peek().map(|(idx, _)| *idx).unwrap_or(header.len());
    let remainder = &header[start..];
    let mut parsed = tokenize_word(remainder)?;
    if parsed.0.is_empty() {
        return None;
    }
    while let Some((idx, _)) = chars.peek() {
        if *idx < start + parsed.1 {
            chars.next();
        } else {
            break;
        }
    }
    Some(std::mem::take(&mut parsed.0))
}

fn tokenize_word(input: &str) -> Option<(String, usize)> {
    tokenize_literal_word(input, false)
}

fn tokenize_literal_word(input: &str, top_level: bool) -> Option<(String, usize)> {
    let mut token = String::new();
    let mut quote = Quote::None;
    let mut consumed = 0;
    let mut chars = input.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        if top_level && quote == Quote::None {
            if ch.is_whitespace() || matches!(ch, ';' | '&' | '|' | '>') {
                consumed = idx;
                break;
            }
            if matches!(ch, '(' | ')' | '<') {
                return None;
            }
        }
        consumed = idx + ch.len_utf8();
        match quote {
            Quote::Single => {
                if ch == '\'' {
                    quote = Quote::None;
                } else {
                    token.push(ch);
                }
            }
            Quote::Double => match ch {
                '"' => quote = Quote::None,
                '`' => return None,
                ch if !word_character_is_literal(ch, quote) => return None,
                '\\' => match chars.next() {
                    Some((next_idx, escaped)) => {
                        consumed = next_idx + escaped.len_utf8();
                        push_double_quoted_backslash(&mut token, escaped);
                    }
                    None => token.push('\\'),
                },
                _ => token.push(ch),
            },
            Quote::None => match ch {
                c if c.is_whitespace() => {
                    consumed = idx;
                    break;
                }
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '\\' => match chars.next() {
                    Some((next_idx, escaped)) => {
                        consumed = next_idx + escaped.len_utf8();
                        // Bash removes an unquoted backslash-newline before
                        // word recognition, including inside option spellings.
                        if escaped != '\n' {
                            token.push(escaped);
                        }
                    }
                    None => token.push('\\'),
                },
                '|' | ';' | '<' | '>' | '`' => return None,
                '&' if matches!(chars.peek(), Some((_, '&'))) => return None,
                ch if !word_character_is_literal(ch, quote) => return None,
                _ => token.push(ch),
            },
        }
    }

    if quote == Quote::None {
        Some((token, consumed))
    } else {
        None
    }
}

fn has_non_space_remainder(chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>) -> bool {
    chars.any(|(_, ch)| !ch.is_whitespace())
}

fn skip_horizontal_space(input: &str, start: usize) -> usize {
    input[start..]
        .char_indices()
        .find_map(|(offset, ch)| (!matches!(ch, ' ' | '\t')).then_some(start + offset))
        .unwrap_or(input.len())
}

fn push_double_quoted_backslash(token: &mut String, escaped: char) {
    match escaped {
        '\n' => {}
        '$' | '`' | '"' | '\\' => token.push(escaped),
        _ => {
            token.push('\\');
            token.push(escaped);
        }
    }
}

fn word_character_is_literal(ch: char, quote: Quote) -> bool {
    // Escaped characters are consumed separately. Single quotes disable all
    // expansion; double quotes still allow parameter and command substitution.
    // History expansion (!) is disabled in non-interactive bash.
    match quote {
        Quote::Single => true,
        Quote::Double => !matches!(ch, '$' | '`'),
        Quote::None => !matches!(ch, '$' | '`' | '*' | '?' | '[' | '{' | '}' | '~'),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}

#[cfg(test)]
pub(super) mod tests {
    use super::parse;

    pub(crate) const LITERAL_APPEND_CASES: &[&str] = &[
        "echo plain words >> notes.txt",
        "echo '$? $$ $! $# $* $@ $- $0 $9 $HOME ${HOME} $(date) $((1+2)) `date`' >> notes.txt",
        "echo '* ? [abc] {a,b} {1..3} ~ ~root' >> notes.txt",
        "echo \"* ? [abc] {a,b} {1..3} ~ ~root\" >> notes.txt",
        "echo \\$HOME \\`date\\` \\* \\? \\[abc] \\{a,b\\} \\~root >> notes.txt",
        "echo \"\\$HOME \\`date\\`\" >> notes.txt",
        "echo hello! >> notes.txt",
        "echo hi >> 'notes.txt'",
        "echo hi >> \"notes.txt\"",
        "cat >> notes.txt <<EOF\nliteral text\nEOF",
        "cat >> notes.txt <<EOF\nEOF",
        "cat >> notes.txt <<'EOF'\n$HOME $(date) `date` \\literal\nEOF",
        "cat >> notes.txt <<\"EOF\"\n$HOME $(date) `date` \\literal\nEOF",
        "cat >> notes.txt <<\\EOF\n$HOME $(date) `date` \\literal\nEOF",
        "cat >> notes.txt <<E'OF'\n$HOME\nEOF",
        "cat >> notes.txt <<'EOF'\nEOFsuffix\nmore\nEOF",
    ];

    #[test]
    fn literal_append_table() {
        for command in LITERAL_APPEND_CASES {
            assert!(parse(command).is_some(), "must accept: {command}");
        }
        for command in [
            "cat >> notes.txt <<-EOF\n\ttext\n\tEOF",
            "cat >> notes.txt <<-'EOF'\n\t$HOME\n\tEOF",
            "cat >> notes.txt <<EOF ignored\ntext\nEOF",
        ] {
            assert!(parse(command).is_none(), "must decline: {command}");
        }
    }

    #[test]
    fn unquoted_heredoc_expansions_decline() {
        for body in ["$HOME $(date)", "`date`", "back\\slash"] {
            let command = format!("cat >> notes.txt <<EOF\n{body}\nEOF");
            assert!(parse(&command).is_none(), "must decline: {command}");
        }
    }

    #[test]
    fn expanding_words_decline() {
        let mut accepted = Vec::new();
        for word in [
            "$?",
            "$$",
            "$!",
            "$#",
            "$*",
            "$@",
            "$-",
            "$0",
            "$9",
            "$HOME",
            "${HOME}",
            "$(date)",
            "$((1 + 2))",
            "`date`",
            "$'hello'",
            "$\"hello\"",
            "*",
            "?",
            "[abc]",
            "{a,b}",
            "{1..3}",
            "~",
            "~root",
            "\"$?\"",
            "\"$$\"",
            "\"`date`\"",
        ] {
            for command in [
                format!("echo {word} >> notes.txt"),
                format!("echo hi >> {word}"),
            ] {
                if parse(&command).is_some() {
                    accepted.push(command);
                }
            }
        }
        assert!(accepted.is_empty(), "must decline: {accepted:?}");
    }
}
