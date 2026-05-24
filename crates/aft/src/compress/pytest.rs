use crate::compress::Compressor;

pub struct PytestCompressor;

impl Compressor for PytestCompressor {
    fn matches(&self, command: &str) -> bool {
        let tokens: Vec<&str> = command.split_whitespace().collect();
        tokens.first().is_some_and(|head| *head == "pytest")
            || tokens
                .windows(3)
                .any(|window| matches!(window, ["python" | "python3", "-m", "pytest"]))
    }

    fn compress(&self, _command: &str, output: &str) -> String {
        compress_pytest(output)
    }
}

fn compress_pytest(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let mut result = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();

        if is_header_line(trimmed) || is_failure_or_error_test_line(trimmed) {
            result.push(line.to_string());
            index += 1;
            continue;
        }

        if is_section_header(trimmed, "FAILURES") || is_section_header(trimmed, "ERRORS") {
            while index < lines.len() {
                let current = lines[index];
                let current_trimmed = current.trim();
                if index != 0
                    && index != lines.len() - 1
                    && current_trimmed.starts_with('=')
                    && !is_section_header(current_trimmed, "FAILURES")
                    && !is_section_header(current_trimmed, "ERRORS")
                {
                    break;
                }
                result.push(current.to_string());
                index += 1;
            }
            continue;
        }

        if is_section_header(trimmed, "warnings summary") {
            let (warnings, next_index) = compress_warnings(&lines, index);
            result.extend(warnings);
            index = next_index;
            continue;
        }

        if is_section_header(trimmed, "short test summary info") || is_final_summary(trimmed) {
            result.push(line.to_string());
            index += 1;
            continue;
        }

        if is_pass_status_line(trimmed) {
            index += 1;
            continue;
        }

        index += 1;
    }

    trim_trailing_lines(&result.join("\n"))
}

fn is_header_line(trimmed: &str) -> bool {
    trimmed.starts_with("platform ")
        || trimmed.starts_with("rootdir:")
        || trimmed.starts_with("collected ")
}

fn is_failure_or_error_test_line(trimmed: &str) -> bool {
    trimmed.contains(" FAILED")
        || trimmed.ends_with(" FAILED")
        || trimmed.contains(" ERROR")
        || trimmed.ends_with(" ERROR")
}

fn is_section_header(trimmed: &str, name: &str) -> bool {
    trimmed.starts_with('=') && trimmed.contains(name) && trimmed.ends_with('=')
}

fn is_pass_status_line(trimmed: &str) -> bool {
    !trimmed.is_empty()
        && (trimmed
            .chars()
            .all(|char| matches!(char, '.' | 's' | 'x' | 'X'))
            || trimmed.ends_with(" PASSED")
            || trimmed.contains(" PASSED "))
}

fn is_final_summary(trimmed: &str) -> bool {
    trimmed.starts_with('=')
        && (trimmed.contains(" passed")
            || trimmed.contains(" failed")
            || trimmed.contains(" error")
            || trimmed.contains(" skipped")
            || trimmed.contains(" xfailed"))
        && trimmed.ends_with('=')
}

fn compress_warnings(lines: &[&str], start: usize) -> (Vec<String>, usize) {
    let mut result = vec![lines[start].to_string()];
    let mut index = start + 1;
    let mut warnings_seen = 0usize;
    let mut omitted = 0usize;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        if trimmed.starts_with('=') && trimmed.ends_with('=') {
            break;
        }
        if is_warning_entry(trimmed) {
            warnings_seen += 1;
            if warnings_seen <= 5 {
                result.push(line.to_string());
            } else {
                omitted += 1;
            }
        } else if warnings_seen <= 5 {
            result.push(line.to_string());
        }
        index += 1;
    }

    if omitted > 0 {
        result.push(format!("... and {omitted} more warnings"));
    }

    (result, index)
}

fn is_warning_entry(trimmed: &str) -> bool {
    trimmed.contains("Warning:") || trimmed.contains("warning:") || trimmed.starts_with("tests/")
}

fn trim_trailing_lines(input: &str) -> String {
    input
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}
