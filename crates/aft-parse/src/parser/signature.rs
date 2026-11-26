use super::MAX_SIGNATURE_BYTES;

pub(super) fn first_line(text: &str) -> &str {
    let line = text.lines().next().unwrap_or(text).trim_end();
    line.strip_suffix('{').unwrap_or(line).trim_end()
}

pub(super) fn bounded_parts<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut signature = String::new();
    for part in parts {
        let remaining = MAX_SIGNATURE_BYTES - signature.len();
        if part.len() <= remaining {
            if signature.capacity() < signature.len() + part.len() {
                signature.reserve_exact(part.len());
            }
            signature.push_str(part);
            continue;
        }
        let mut end = remaining;
        while !part.is_char_boundary(end) {
            end -= 1;
        }
        signature.reserve_exact(end + '…'.len_utf8());
        signature.push_str(&part[..end]);
        signature.push('…');
        break;
    }
    signature
}

pub(super) fn join_lines(lines: &[&str]) -> String {
    bounded_parts(
        lines
            .iter()
            .enumerate()
            .flat_map(|(index, line)| [if index == 0 { "" } else { "\n" }, *line]),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_opus8_joined_signature_truncates_the_completed_text() {
        let long = "a".repeat(MAX_SIGNATURE_BYTES - 3) + "🦀 trailing";
        for lines in [[long.as_str(), "next line"], ["@decorator", long.as_str()]] {
            let mut expected = lines.join("\n");
            super::super::truncate_signature(&mut expected);
            assert_eq!(join_lines(&lines), expected);
        }
    }
}
