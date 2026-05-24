use crate::compress::generic::GenericCompressor;
use crate::compress::Compressor;

pub struct CargoCompressor;

impl Compressor for CargoCompressor {
    fn matches(&self, command: &str) -> bool {
        command
            .split_whitespace()
            .next()
            .is_some_and(|head| head == "cargo")
    }

    fn compress(&self, command: &str, output: &str) -> String {
        match cargo_subcommand(command).as_deref() {
            Some("build" | "check" | "clippy") => compress_build_like(output),
            Some("test") => compress_test(output),
            _ => GenericCompressor::compress_output(output),
        }
    }
}

fn cargo_subcommand(command: &str) -> Option<String> {
    let mut seen_cargo = false;
    for token in command.split_whitespace() {
        if !seen_cargo {
            if token == "cargo" {
                seen_cargo = true;
            }
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        return Some(token.to_string());
    }
    None
}

fn compress_build_like(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let has_diagnostic = lines
        .iter()
        .any(|line| is_warning_or_error(line) || line.trim_start().starts_with("error["));

    if !has_diagnostic {
        return output.trim_end().to_string();
    }

    let mut result = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];
        if is_ignored_progress(line) {
            index += 1;
            continue;
        }

        if is_warning_or_error(line) || line.trim_start().starts_with("error[") {
            let start = index;
            index += 1;
            while index < lines.len() && !starts_next_build_message(lines[index]) {
                index += 1;
            }
            result.extend(lines[start..index].iter().map(|line| (*line).to_string()));
            continue;
        }

        if is_final_cargo_summary(line) {
            result.push(line.to_string());
        }
        index += 1;
    }

    trim_trailing_lines(&result.join("\n"))
}

fn starts_next_build_message(line: &str) -> bool {
    is_ignored_progress(line)
        || is_warning_or_error(line)
        || line.trim_start().starts_with("error[")
        || is_final_cargo_summary(line)
}

fn is_warning_or_error(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("warning:") || trimmed.starts_with("error:")
}

fn is_ignored_progress(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed == "Updating crates.io index" || is_compiling_line(trimmed)
}

fn is_compiling_line(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("Compiling ") else {
        return false;
    };
    let mut parts = rest.split_whitespace();
    let _crate_name = parts.next();
    parts.next().is_some_and(|part| {
        part.strip_prefix('v').is_some_and(|version| {
            version
                .chars()
                .all(|char| char.is_ascii_digit() || char == '.')
        })
    })
}

fn is_final_cargo_summary(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("Finished ")
        || trimmed.starts_with("error: could not compile")
        || trimmed.starts_with("test result:")
}

fn compress_test(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let has_failures = lines.iter().any(|line| line.trim() == "failures:");
    if !has_failures {
        let result: Vec<String> = lines
            .iter()
            .filter(|line| {
                let trimmed = line.trim_start();
                trimmed.starts_with("running ")
                    || trimmed.starts_with("test result:")
                    || is_final_cargo_summary(trimmed)
            })
            .map(|line| (*line).to_string())
            .collect();
        return trim_trailing_lines(&result.join("\n"));
    }

    let mut result = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        if trimmed.starts_with("running ") || trimmed.starts_with("test result:") {
            result.push(line.to_string());
            index += 1;
            continue;
        }

        if trimmed == "failures:" {
            result.extend(lines[index..].iter().map(|line| (*line).to_string()));
            break;
        }

        if line.starts_with("---- ") {
            while index < lines.len() {
                result.push(lines[index].to_string());
                index += 1;
                if index < lines.len()
                    && (lines[index].trim_start().starts_with("test result:")
                        || lines[index].trim() == "failures:")
                {
                    break;
                }
            }
            continue;
        }

        index += 1;
    }

    trim_trailing_lines(&result.join("\n"))
}

fn trim_trailing_lines(input: &str) -> String {
    input
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}
