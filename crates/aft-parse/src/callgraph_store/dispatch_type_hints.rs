use super::*;

pub(super) fn parse_method_dispatch(full_ref: &str) -> Option<(String, String, String, bool)> {
    let dot = full_ref.rfind('.').map(|index| (index, 1usize, false));
    let colon = full_ref.rfind("::").map(|index| (index, 2usize, true));
    let arrow = full_ref.rfind("->").map(|index| (index, 2usize, false));
    let (delimiter, delimiter_len, colon_dispatch) = [dot, colon, arrow]
        .into_iter()
        .flatten()
        .max_by_key(|(index, _, _)| *index)?;
    if delimiter == 0 {
        return None;
    }
    let member_start = delimiter + delimiter_len;
    if member_start >= full_ref.len() {
        return None;
    }
    let receiver_expression = full_ref[..delimiter].trim();
    let receiver = last_name_segment(receiver_expression).trim();
    let member = &full_ref[member_start..];
    if receiver.is_empty() || member.is_empty() {
        return None;
    }
    Some((
        receiver_expression.to_string(),
        receiver.to_string(),
        member.to_string(),
        colon_dispatch,
    ))
}

pub(super) fn last_name_segment(value: &str) -> &str {
    value
        .rsplit(['.', ':', '/', '\\', '-', '>'])
        .find(|segment| !segment.is_empty())
        .unwrap_or(value)
}

pub(super) fn node_text<'source>(
    node: tree_sitter::Node<'_>,
    source: &'source str,
) -> &'source str {
    &source[node.byte_range()]
}

pub(super) fn infer_cpp_receiver_type_from_scope(
    scope: tree_sitter::Node<'_>,
    source: &str,
    receiver: &str,
    call_line: u32,
) -> Option<String> {
    let lines = source.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    let scope_start = scope.start_position().row as usize;
    let call_index = (call_line as usize)
        .saturating_sub(1)
        .min(lines.len().saturating_sub(1));
    for index in (scope_start..=call_index).rev() {
        if let Some(receiver_type) = infer_cpp_receiver_type_from_line(lines[index], receiver) {
            return Some(receiver_type);
        }
    }
    None
}

pub(super) fn infer_cpp_receiver_type_from_line(line: &str, receiver: &str) -> Option<String> {
    for receiver_start in identifier_occurrences(line, receiver) {
        let after = line[receiver_start + receiver.len()..].trim_start();
        if after
            .chars()
            .next()
            .is_some_and(|ch| !matches!(ch, ';' | '=' | ',' | ')' | '[' | '{' | '('))
        {
            continue;
        }
        let type_text = cpp_type_before_receiver(&line[..receiver_start])?;
        let normalized = normalize_cpp_type_name(type_text)?;
        if normalized == "auto" {
            if let Some(rhs) = after.strip_prefix('=') {
                return infer_cpp_auto_receiver_type(rhs);
            }
            continue;
        }
        return Some(normalized);
    }
    None
}

pub(super) fn cpp_type_before_receiver(prefix: &str) -> Option<&str> {
    let candidate = prefix
        .rsplit([';', '{', '}', '('])
        .next()
        .unwrap_or(prefix)
        .trim();
    if candidate.is_empty() || candidate.ends_with(',') {
        None
    } else {
        Some(candidate)
    }
}

pub(super) fn normalize_cpp_type_name(type_text: &str) -> Option<String> {
    let without_templates = strip_angle_groups(type_text);
    let mut cleaned = String::with_capacity(without_templates.len());
    for token in without_templates.split_whitespace() {
        if matches!(
            token,
            "const" | "volatile" | "mutable" | "typename" | "class" | "struct"
        ) {
            continue;
        }
        if !cleaned.is_empty() {
            cleaned.push(' ');
        }
        cleaned.push_str(token);
    }
    let token = cleaned
        .split_whitespace()
        .last()
        .unwrap_or(cleaned.trim())
        .trim_matches(|ch: char| !(is_code_ident_char(ch) || ch == ':' || ch == '.'))
        .trim_matches(['*', '&']);
    let simple = token.rsplit("::").next().unwrap_or(token).trim();
    if simple.is_empty() || cpp_non_type_token(simple) {
        None
    } else {
        Some(simple.to_string())
    }
}

pub(super) fn infer_cpp_auto_receiver_type(rhs: &str) -> Option<String> {
    let rhs = rhs.trim_start();
    if let Some(after_new) = rhs.strip_prefix("new ") {
        return infer_cpp_constructor_type(after_new);
    }
    infer_cpp_make_template_type(rhs)
        .or_else(|| infer_cpp_constructor_type(rhs))
        .or_else(|| infer_cpp_factory_type(rhs))
}

pub(super) fn infer_cpp_constructor_type(rhs: &str) -> Option<String> {
    let (head, rest) = read_invocation_head(rhs.trim_start())?;
    let normalized = normalize_cpp_type_name(head)?;
    if !normalized
        .chars()
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_uppercase())
    {
        return None;
    }
    if matches!(rest.trim_start().chars().next(), Some('(' | '{')) {
        Some(normalized)
    } else {
        None
    }
}

pub(super) fn infer_cpp_make_template_type(rhs: &str) -> Option<String> {
    let (head, rest) = read_invocation_head(rhs.trim_start())?;
    if !rest.trim_start().starts_with('(') {
        return None;
    }
    let base = head.split('<').next().unwrap_or(head);
    let base_simple = base.rsplit("::").next().unwrap_or(base);
    if !matches!(base_simple, "make_unique" | "make_shared") {
        return None;
    }
    first_angle_arg(head).and_then(normalize_cpp_type_name)
}

pub(super) fn infer_cpp_factory_type(rhs: &str) -> Option<String> {
    let (head, rest) = read_invocation_head(rhs.trim_start())?;
    if !rest.trim_start().starts_with('(') {
        return None;
    }
    let simple = head
        .split('<')
        .next()
        .unwrap_or(head)
        .rsplit("::")
        .next()
        .unwrap_or(head);
    for prefix in ["make", "create", "build"] {
        if let Some(suffix) = simple.strip_prefix(prefix) {
            if suffix
                .chars()
                .next()
                .is_some_and(|ch| ch == '_' || ch.is_ascii_uppercase())
            {
                return normalize_cpp_type_name(suffix);
            }
        }
    }
    None
}

pub(super) fn read_invocation_head(value: &str) -> Option<(&str, &str)> {
    let value = value.trim_start();
    let mut end = 0usize;
    for (index, ch) in value.char_indices() {
        let allowed_separator = ch == ':' || ch == '.';
        if is_code_ident_char(ch) || allowed_separator {
            end = index + ch.len_utf8();
            continue;
        }
        break;
    }
    if end == 0 {
        return None;
    }
    let mut rest = &value[end..];
    if let Some(stripped) = rest.trim_start().strip_prefix('<') {
        let skipped = skip_balanced_angle(stripped)?;
        let rest_start = rest.len() - rest.trim_start().len();
        let angle_len = 1 + skipped;
        end += rest_start + angle_len;
        rest = &value[end..];
    }
    Some((value[..end].trim(), rest))
}

pub(super) fn skip_balanced_angle(value_after_open: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (index, ch) in value_after_open.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index + ch.len_utf8());
                }
            }
            _ => {}
        }
    }
    None
}

pub(super) fn first_angle_arg(value: &str) -> Option<&str> {
    let open = value.find('<')?;
    let inner_len = skip_balanced_angle(&value[open + 1..])?;
    let inner = &value[open + 1..open + inner_len];
    split_top_level_commas(inner).into_iter().next()
}

pub(super) fn strip_angle_groups(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut depth = 0usize;
    for ch in value.chars() {
        match ch {
            '<' => {
                if depth == 0 {
                    output.push(' ');
                }
                depth += 1;
            }
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => output.push(ch),
            _ => {}
        }
    }
    output
}

pub(super) fn cpp_non_type_token(value: &str) -> bool {
    matches!(
        value,
        "return"
            | "if"
            | "else"
            | "for"
            | "while"
            | "do"
            | "switch"
            | "case"
            | "default"
            | "break"
            | "continue"
            | "goto"
            | "throw"
            | "new"
            | "delete"
            | "co_await"
            | "co_yield"
            | "co_return"
            | "static_cast"
            | "const_cast"
            | "dynamic_cast"
            | "reinterpret_cast"
            | "sizeof"
            | "alignof"
            | "typeid"
            | "and"
            | "or"
            | "not"
            | "xor"
    )
}

pub(super) fn identifier_occurrences(value: &str, needle: &str) -> Vec<usize> {
    value
        .match_indices(needle)
        .filter_map(|(index, _)| identifier_boundary(value, index, needle.len()).then_some(index))
        .collect()
}

pub(super) fn identifier_boundary(value: &str, start: usize, len: usize) -> bool {
    let before = value[..start].chars().next_back();
    let after = value[start + len..].chars().next();
    !before.is_some_and(is_code_ident_char) && !after.is_some_and(is_code_ident_char)
}

pub(super) fn is_code_ident_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

pub(super) fn signature_parameter_text(signature: &str) -> Option<&str> {
    let open = signature.find('(')?;
    let mut depth = 0usize;
    for (offset, ch) in signature[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&signature[open + 1..open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

pub(super) fn split_top_level_commas(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut angle_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    for (index, ch) in value.char_indices() {
        match ch {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            ',' if angle_depth == 0 && paren_depth == 0 && bracket_depth == 0 => {
                let part = value[start..index].trim();
                if !part.is_empty() {
                    parts.push(part);
                }
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    let part = value[start..].trim();
    if !part.is_empty() {
        parts.push(part);
    }
    parts
}

pub(super) fn rust_parameter_name(pattern: &str) -> Option<&str> {
    let mut pattern = pattern.trim();
    if let Some(stripped) = pattern.strip_prefix("mut ") {
        pattern = stripped.trim_start();
    }
    pattern
        .rsplit(|ch: char| !is_rust_ident_char(ch))
        .find(|part| !part.is_empty())
}

pub(super) fn is_rust_ident_char(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}
