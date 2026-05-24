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
    let after_spaces = skip_horizontal_space(command, after_operator);
    let (delimiter, delimiter_end) = read_unquoted_word(command, after_spaces)?;
    if delimiter.is_empty() {
        return None;
    }

    let line_start = match command[delimiter_end..].find('\n') {
        Some(offset) => delimiter_end + offset + 1,
        None => return None,
    };

    let body = &command[line_start..];
    let terminator = format!("\n{delimiter}");
    let (content, rest_start) = if body == delimiter {
        ("", line_start + delimiter.len())
    } else if let Some(stripped) = body.strip_prefix(&format!("{delimiter}\n")) {
        ("", command.len() - stripped.len())
    } else if let Some(offset) = body.find(&terminator) {
        let content = &body[..offset + 1];
        let rest_start = line_start + offset + terminator.len();
        (content, rest_start)
    } else {
        return None;
    };

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
                '$' if matches!(chars.peek(), Some((_, '(' | '{'))) => return None,
                '\\' => {
                    chars.next();
                }
                _ => {}
            },
            Quote::None => match ch {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '`' => return None,
                '$' if is_unsupported_variable_start(chars.peek().map(|(_, c)| *c)) => return None,
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
                '$' if is_unsupported_variable_start(chars.peek().map(|(_, c)| *c)) => return None,
                '\\' => match chars.next() {
                    Some((_, escaped)) => token.push(escaped),
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
                '$' if is_unsupported_variable_start(chars.peek().map(|(_, c)| *c)) => return None,
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
    let mut token = String::new();
    let mut quote = Quote::None;
    let mut consumed = 0;
    let mut chars = input.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
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
                '$' if is_unsupported_variable_start(chars.peek().map(|(_, c)| *c)) => return None,
                '\\' => match chars.next() {
                    Some((next_idx, escaped)) => {
                        consumed = next_idx + escaped.len_utf8();
                        token.push(escaped);
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
                        token.push(escaped);
                    }
                    None => token.push('\\'),
                },
                '|' | ';' | '<' | '>' | '`' => return None,
                '&' if matches!(chars.peek(), Some((_, '&'))) => return None,
                '$' if is_unsupported_variable_start(chars.peek().map(|(_, c)| *c)) => return None,
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

fn read_unquoted_word(input: &str, start: usize) -> Option<(String, usize)> {
    let mut end = start;
    let mut word = String::new();
    for (offset, ch) in input[start..].char_indices() {
        if ch.is_whitespace() {
            break;
        }
        if matches!(ch, '\'' | '"' | '`' | '$' | '|' | ';' | '&' | '<' | '>') {
            return None;
        }
        word.push(ch);
        end = start + offset + ch.len_utf8();
    }
    Some((word, end))
}

fn is_unsupported_variable_start(next: Option<char>) -> bool {
    matches!(next, Some('(' | '{')) || next.is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
    None,
    Single,
    Double,
}
