use crate::compress::generic::GenericCompressor;
use crate::compress::{Compressor, Specificity};

pub struct NpmCompressor;

impl Compressor for NpmCompressor {
    fn specificity(&self) -> Specificity {
        Specificity::PackageManager
    }

    fn matches(&self, command: &str) -> bool {
        command
            .split_whitespace()
            .next()
            .is_some_and(|head| head == "npm")
    }

    fn compress(&self, command: &str, output: &str) -> String {
        match npm_subcommand(command).as_deref() {
            Some("install" | "i" | "ci") => compress_install(output),
            Some("run" | "test") => GenericCompressor::compress_output(output),
            Some("audit") => compress_audit(output),
            Some("publish") => compress_install(output),
            _ => GenericCompressor::compress_output(output),
        }
    }
}

fn npm_subcommand(command: &str) -> Option<String> {
    command
        .split_whitespace()
        .skip_while(|token| *token != "npm")
        .skip(1)
        .find(|token| !token.starts_with('-'))
        .map(ToString::to_string)
}

fn compress_install(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let has_audit = lines
        .iter()
        .any(|line| line.trim_start().starts_with("audited "));
    let has_final_summary = lines.iter().any(|line| is_final_summary(line));
    let tail_start = lines.len().saturating_sub(5);
    let mut result = Vec::new();
    let mut deprecated_seen = 0usize;
    let mut deprecated_omitted = 0usize;

    for (index, line) in lines.iter().enumerate() {
        if is_npm_progress(line) {
            continue;
        }
        if line.trim_start().starts_with("npm WARN deprecated ") {
            deprecated_seen += 1;
            if deprecated_seen <= 5 {
                result.push((*line).to_string());
            } else {
                deprecated_omitted += 1;
            }
            continue;
        }
        if has_audit && has_final_summary && line.trim_start().starts_with("added ") {
            continue;
        }
        if index >= tail_start
            || line.trim_start().starts_with("npm ERR!")
            || is_final_summary(line)
        {
            result.push((*line).to_string());
        }
    }

    if deprecated_omitted > 0 {
        insert_after_deprecations(
            &mut result,
            format!("... and {deprecated_omitted} more deprecation warnings"),
        );
    }

    trim_trailing_lines(&result.join("\n"))
}

fn is_npm_progress(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("npm http fetch")
        || trimmed.starts_with("npm timing")
        || trimmed.starts_with("npm verb")
}

fn is_final_summary(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("audited ")
        || trimmed.starts_with("found ")
        || trimmed.contains(" vulnerabilities")
        || trimmed.contains(" packages are looking for funding")
        || trimmed.starts_with("published ")
        || trimmed.starts_with("+ ")
}

fn insert_after_deprecations(result: &mut Vec<String>, summary: String) {
    let position = result
        .iter()
        .rposition(|line| line.trim_start().starts_with("npm WARN deprecated "))
        .map_or(0, |index| index + 1);
    result.insert(position, summary);
}

fn compress_audit(output: &str) -> String {
    let mut result = Vec::new();
    let mut vulnerabilities = 0usize;
    let mut omitted = 0usize;

    for line in output.lines() {
        if is_audit_vulnerability_line(line) {
            vulnerabilities += 1;
            if vulnerabilities <= 10 {
                result.push(line.to_string());
            } else {
                omitted += 1;
            }
            continue;
        }
        if is_audit_summary(line) || vulnerabilities <= 10 {
            result.push(line.to_string());
        }
    }

    if omitted > 0 {
        result.push(format!("... and {omitted} more vulnerabilities"));
    }

    trim_trailing_lines(&result.join("\n"))
}

fn is_audit_vulnerability_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("VULN ") || trimmed.starts_with("# ") && trimmed.contains(" - ")
}

fn is_audit_summary(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("found ")
        || trimmed.starts_with("npm audit fix")
        || trimmed.contains(" vulnerabilities")
}

fn trim_trailing_lines(input: &str) -> String {
    input
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}
