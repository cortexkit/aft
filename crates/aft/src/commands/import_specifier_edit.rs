//! Lossless edits for named-import lists in languages whose import commands merge
//! or remove individual specifiers. `organize_imports` remains the operation that
//! deliberately rewrites an import statement's formatting.

use crate::imports;
use crate::parser::LangId;
use std::ops::Range;

#[derive(Clone, Copy)]
enum ListKind {
    Braces,
    Parentheses,
    PythonFrom,
}

struct ImportList {
    body: Range<usize>,
    chunks: Vec<Range<usize>>,
    commas: Vec<usize>,
    specifier_chunks: Vec<usize>,
}

pub(super) fn remove_named_specifiers(
    raw_text: &str,
    names: &[String],
    target_name: &str,
    lang: LangId,
) -> Option<String> {
    let list = parse_list(raw_text, lang)?;
    if list.specifier_chunks.len() != names.len() {
        return None;
    }

    let remove: Vec<usize> = names
        .iter()
        .zip(&list.specifier_chunks)
        .filter(|(name, _)| imports::specifier_matches(name, target_name))
        .map(|(_, chunk)| *chunk)
        .collect();
    if remove.is_empty() || remove.len() == names.len() {
        return None;
    }

    let remove: std::collections::HashSet<usize> = remove.into_iter().collect();
    let mut chunks = Vec::new();
    let mut pending_comments = String::new();
    for (index, range) in list.chunks.iter().enumerate() {
        if remove.contains(&index) {
            pending_comments.push_str(&comments_only(
                raw_text,
                range.clone(),
                lang == LangId::Python,
            ));
            if list.commas.get(index).is_none() {
                pending_comments.push_str(trailing_whitespace(&raw_text[range.clone()]));
            }
        } else {
            chunks.push(format!("{pending_comments}{}", &raw_text[range.clone()]));
            pending_comments.clear();
        }
    }

    if !pending_comments.is_empty() {
        if pending_comments.trim().is_empty() {
            if let Some(last) = chunks.last_mut() {
                last.push_str(&pending_comments);
            }
        } else {
            if (pending_comments.contains("//") || pending_comments.contains('#'))
                && !pending_comments.ends_with('\n')
            {
                pending_comments.push('\n');
            }
            chunks.push(pending_comments);
        }
    }
    if chunks.is_empty() {
        return None;
    }

    // The first retained specifier takes the first specifier's original leading
    // whitespace, so deleting the first entry does not add a blank line or extra
    // indentation before the next one.
    if let Some(first_retained_index) = list
        .chunks
        .iter()
        .enumerate()
        .find(|(index, _)| !remove.contains(index) && list.specifier_chunks.contains(index))
        .map(|(index, _)| index)
    {
        if first_retained_index != list.specifier_chunks[0] {
            if let Some(first_chunk) = chunks.first_mut() {
                let original_prefix =
                    leading_whitespace(&raw_text[list.chunks[list.specifier_chunks[0]].clone()]);
                let current = first_chunk.trim_start().to_string();
                *first_chunk = format!("{original_prefix}{current}");
            }
        }
    }

    let body = chunks.join(",");
    Some(format!(
        "{}{}{}",
        &raw_text[..list.body.start],
        body,
        &raw_text[list.body.end..]
    ))
}

pub(super) fn insert_named_specifiers(
    raw_text: &str,
    existing_names: &[String],
    additions: &[String],
    lang: LangId,
) -> Option<String> {
    let mut rewritten = raw_text.to_string();
    let mut names = existing_names.to_vec();
    for addition in additions {
        if names.iter().any(|existing| {
            imports::specifier_imported_name(existing) == imports::specifier_imported_name(addition)
                && imports::specifier_local_name(existing)
                    == imports::specifier_local_name(addition)
        }) {
            continue;
        }
        let list = parse_list(&rewritten, lang)?;
        if list.specifier_chunks.len() != names.len() {
            return None;
        }
        let ordered = names.windows(2).all(|pair| {
            imports::specifier_local_name(&pair[0]) <= imports::specifier_local_name(&pair[1])
        });
        let insert_before = if ordered {
            names.iter().position(|name| {
                imports::specifier_local_name(name) > imports::specifier_local_name(addition)
            })
        } else {
            None
        };
        let (offset, text) = if let Some(index) = insert_before {
            let chunk = list.specifier_chunks[index];
            let token_start = first_code_byte(&rewritten, list.chunks[chunk].clone())?;
            if let Some((newline, indent)) = multiline_prefix(&rewritten, token_start) {
                (token_start, format!("{addition},{newline}{indent}"))
            } else {
                (token_start, format!("{addition}, "))
            }
        } else {
            let (offset, template) = append_position(&rewritten, &list)?;
            (offset, template.replace("__NAME__", addition))
        };
        rewritten.insert_str(offset, &text);
        if let Some(index) = insert_before {
            names.insert(index, addition.clone());
        } else {
            names.push(addition.clone());
        }
    }
    Some(rewritten)
}

fn parse_list(raw_text: &str, lang: LangId) -> Option<ImportList> {
    let kind = match lang {
        LangId::TypeScript | LangId::Tsx | LangId::JavaScript | LangId::Vue | LangId::Rust => {
            ListKind::Braces
        }
        LangId::Python => {
            if raw_text.trim_start().starts_with("from ") {
                if raw_text.contains('(') {
                    ListKind::Parentheses
                } else {
                    ListKind::PythonFrom
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    let body = match kind {
        ListKind::Braces => delimited_body(raw_text, '{', '}', false)?,
        ListKind::Parentheses => delimited_body(raw_text, '(', ')', true)?,
        ListKind::PythonFrom => {
            let import_keyword = raw_text.rfind("import")?;
            let start = import_keyword + "import".len();
            start..raw_text.len()
        }
    };
    let commas = top_level_commas(raw_text, body.clone(), matches!(lang, LangId::Python));
    let mut chunks = Vec::with_capacity(commas.len() + 1);
    let mut start = body.start;
    for comma in &commas {
        chunks.push(start..*comma);
        start = comma + 1;
    }
    chunks.push(start..body.end);

    let specifier_chunks = chunks
        .iter()
        .enumerate()
        .filter(|(_, range)| has_code(raw_text, (*range).clone(), matches!(lang, LangId::Python)))
        .map(|(index, _)| index)
        .collect();
    Some(ImportList {
        body,
        chunks,
        commas,
        specifier_chunks,
    })
}

fn delimited_body(raw_text: &str, open: char, close: char, python: bool) -> Option<Range<usize>> {
    let open_byte = raw_text.find(open)?;
    let close_byte = matching_close(raw_text, open_byte, open, close, python)?;
    (close_byte > open_byte).then_some(open_byte + 1..close_byte)
}

fn matching_close(
    raw_text: &str,
    open_byte: usize,
    open: char,
    close: char,
    python: bool,
) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    let bytes = raw_text.as_bytes();
    let mut index = open_byte;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == q {
                quote = None;
            }
            index += 1;
            continue;
        }
        if byte == b'\'' || byte == b'"' || byte == b'`' {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if byte == b'#' && python {
            index = line_comment_end(bytes, index);
            continue;
        }
        if comment_end(bytes, index, python).is_some() {
            index = comment_end(bytes, index, python)?;
            continue;
        }
        if byte == open as u8 {
            depth += 1;
        } else if byte == close as u8 {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
        index += 1;
    }
    None
}

fn top_level_commas(raw_text: &str, body: Range<usize>, python: bool) -> Vec<usize> {
    let bytes = raw_text.as_bytes();
    let mut commas = Vec::new();
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    let mut index = body.start;
    while index < body.end {
        let byte = bytes[index];
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == q {
                quote = None;
            }
            index += 1;
            continue;
        }
        if byte == b'\'' || byte == b'"' || byte == b'`' {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if let Some(end) = comment_end(bytes, index, python) {
            index = end;
            continue;
        }
        if byte == b'#' && python {
            index = line_comment_end(bytes, index);
            continue;
        }
        match byte {
            b'{' | b'[' | b'(' => depth += 1,
            b'}' | b']' | b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => commas.push(index),
            _ => {}
        }
        index += 1;
    }
    commas
}

fn has_code(raw_text: &str, range: Range<usize>, python: bool) -> bool {
    let bytes = raw_text.as_bytes();
    let mut index = range.start;
    while index < range.end {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
        } else if let Some(end) = comment_end(bytes, index, python) {
            index = end.min(range.end);
        } else if bytes[index] == b'#' && python {
            index = line_comment_end(bytes, index).min(range.end);
        } else {
            return true;
        }
    }
    false
}

fn first_code_byte(raw_text: &str, range: Range<usize>) -> Option<usize> {
    let bytes = raw_text.as_bytes();
    let mut index = range.start;
    while index < range.end {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
        } else if let Some(end) = comment_end(bytes, index, false) {
            index = end.min(range.end);
        } else {
            return Some(index);
        }
    }
    None
}

fn comment_end(bytes: &[u8], index: usize, _python: bool) -> Option<usize> {
    if bytes.get(index..index + 2) == Some(b"//") {
        Some(line_comment_end(bytes, index))
    } else if bytes.get(index..index + 2) == Some(b"/*") {
        bytes[index + 2..]
            .windows(2)
            .position(|pair| pair == b"*/")
            .map(|offset| index + 2 + offset + 2)
            .or(Some(bytes.len()))
    } else {
        None
    }
}

fn line_comment_end(bytes: &[u8], index: usize) -> usize {
    bytes[index..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |offset| index + offset + 1)
}

fn multiline_prefix(raw_text: &str, token_start: usize) -> Option<(&'static str, String)> {
    let line_start = raw_text[..token_start].rfind('\n')? + 1;
    let indent = &raw_text[line_start..token_start];
    if !indent.chars().all(char::is_whitespace) {
        return None;
    }
    let newline = if raw_text[..token_start].contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    Some((newline, indent.to_string()))
}

fn append_position(raw_text: &str, list: &ImportList) -> Option<(usize, String)> {
    let last_chunk_index = *list.specifier_chunks.last()?;
    let last_chunk = list.chunks[last_chunk_index].clone();
    let token_start = first_code_byte(raw_text, last_chunk.clone())?;
    let body_text = &raw_text[list.body.clone()];
    let multiline = body_text.contains('\n');
    let token_end = last_code_end(raw_text, last_chunk.clone())?;
    let has_trailing_comma = list.commas.get(last_chunk_index).is_some_and(|comma| {
        !list
            .specifier_chunks
            .iter()
            .any(|index| *index > last_chunk_index)
            && list
                .chunks
                .get(last_chunk_index + 1)
                .is_some_and(|tail| !has_code(raw_text, tail.clone(), false))
            && *comma >= token_end
    });

    if multiline {
        let (newline, indent) =
            multiline_prefix(raw_text, token_start).or_else(|| Some(("\n", "    ".to_string())))?;
        if has_trailing_comma {
            let comma = *list.commas.get(last_chunk_index)?;
            Some((comma + 1, format!("{newline}{indent}__NAME__,")))
        } else {
            Some((token_end, format!(",{newline}{indent}__NAME__")))
        }
    } else if has_trailing_comma {
        let comma = *list.commas.get(last_chunk_index)?;
        Some((comma, ", __NAME__".to_string()))
    } else {
        Some((token_end, ", __NAME__".to_string()))
    }
}

fn last_code_end(raw_text: &str, range: Range<usize>) -> Option<usize> {
    let bytes = raw_text.as_bytes();
    let mut index = range.start;
    let mut last = None;
    while index < range.end {
        if bytes[index].is_ascii_whitespace() {
            index += 1;
        } else if let Some(end) = comment_end(bytes, index, false) {
            index = end.min(range.end);
        } else {
            last = Some(index + 1);
            index += 1;
        }
    }
    last
}

fn comments_only(raw_text: &str, range: Range<usize>, python: bool) -> String {
    let bytes = raw_text.as_bytes();
    let mut comments = String::new();
    let mut index = range.start;
    while index < range.end {
        let marker = if bytes.get(index..index + 2) == Some(b"//")
            || bytes.get(index..index + 2) == Some(b"/*")
        {
            Some(index)
        } else if python && bytes[index] == b'#' {
            Some(index)
        } else {
            None
        };
        if let Some(start) = marker {
            let line_start = raw_text[range.start..start]
                .rfind('\n')
                .map_or(range.start, |offset| range.start + offset + 1);
            let prefix = &raw_text[line_start..start];
            if prefix.chars().all(char::is_whitespace) {
                comments.push_str(prefix);
            }
            let end = if bytes.get(start..start + 2) == Some(b"/*") {
                bytes[start + 2..range.end]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .map_or(range.end, |offset| start + 2 + offset + 2)
            } else {
                bytes[start..range.end]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(range.end, |offset| start + offset)
            };
            comments.push_str(&raw_text[start..end]);
            index = end;
        } else {
            index += 1;
        }
    }
    comments
}

fn trailing_whitespace(value: &str) -> &str {
    let start = value
        .char_indices()
        .rev()
        .find(|(_, ch)| !ch.is_whitespace())
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    &value[start..]
}

fn leading_whitespace(value: &str) -> &str {
    let end = value
        .char_indices()
        .find(|(_, ch)| !ch.is_whitespace())
        .map_or(value.len(), |(index, _)| index);
    &value[..end]
}
