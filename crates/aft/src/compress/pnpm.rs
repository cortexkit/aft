use crate::compress::generic::GenericCompressor;
use crate::compress::{Compressor, Specificity};

pub struct PnpmCompressor;

impl Compressor for PnpmCompressor {
    fn specificity(&self) -> Specificity {
        Specificity::PackageManager
    }

    fn matches(&self, command: &str) -> bool {
        command
            .split_whitespace()
            .next()
            .is_some_and(|head| head == "pnpm")
    }

    fn compress(&self, command: &str, output: &str) -> String {
        match pnpm_subcommand(command).as_deref() {
            Some("install" | "i" | "add" | "remove") => compress_package(output),
            Some("run" | "test" | "build") => GenericCompressor::compress_output(output),
            _ => GenericCompressor::compress_output(output),
        }
    }
}

fn pnpm_subcommand(command: &str) -> Option<String> {
    command
        .split_whitespace()
        .skip_while(|token| *token != "pnpm")
        .skip(1)
        .find(|token| !token.starts_with('-'))
        .map(ToString::to_string)
}

fn compress_package(output: &str) -> String {
    let mut result = Vec::new();
    let mut progress_seen = 0usize;
    let mut up_to_date_seen = false;

    for line in output.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("Progress: resolved ") {
            progress_seen += 1;
            if progress_seen > 2 {
                continue;
            }
        }
        if trimmed == "Already up-to-date" {
            if up_to_date_seen {
                continue;
            }
            up_to_date_seen = true;
        }
        if trimmed.contains("WARN GET_NO_AUTH")
            || trimmed.starts_with("ERR_PNPM_")
            || trimmed.starts_with("Progress: resolved ")
            || trimmed == "Already up-to-date"
            || trimmed.starts_with("dependencies:")
            || trimmed.starts_with("devDependencies:")
            || trimmed.starts_with("Done in ")
        {
            result.push(line.to_string());
        }
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
