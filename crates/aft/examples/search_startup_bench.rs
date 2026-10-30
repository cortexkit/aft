//! Reproducible timings for search startup / grep-cap / ignore-fingerprint fixes.
//!
//!   cargo run --release -p agent-file-tools --example search_startup_bench -- --generate /tmp/aft-bench-fixture
//!   cargo run --release -p agent-file-tools --example search_startup_bench -- --measure /tmp/aft-bench-fixture

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Instant;

use aft::grep_executor::fallback_grep_bench;
use aft::pattern_compile::{CompileOpts, CompileResult};
use aft::search_index::{ignore_rules_fingerprint, SearchIndex};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: search_startup_bench --generate <dir> | --measure <dir> [--source-files N] [--ignored-files N]");
        std::process::exit(1);
    }
    match args[1].as_str() {
        "--generate" => generate_fixture(Path::new(&args[2]), &args[3..]),
        "--measure" => measure_fixture(Path::new(&args[2])),
        other => {
            eprintln!("unknown mode: {other}");
            std::process::exit(1);
        }
    }
}

fn parse_usize_flag(args: &[String], flag: &str, default: usize) -> usize {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn generate_fixture(root: &Path, args: &[String]) {
    let source_files = parse_usize_flag(args, "--source-files", 20_000);
    let ignored_files = parse_usize_flag(args, "--ignored-files", 5_000);
    if root.exists() {
        fs::remove_dir_all(root).expect("remove old fixture");
    }
    fs::create_dir_all(root.join("src")).expect("mkdir src");
    fs::write(root.join(".gitignore"), "ignored_bulk/\n").expect("write gitignore");

    eprintln!("generating {source_files} source files...");
    for index in 0..source_files {
        let sub = root.join(format!("src/pkg_{index:05}"));
        fs::create_dir_all(&sub).expect("mkdir pkg");
        fs::write(
            sub.join("lib.rs"),
            format!(
                "pub fn bench_fn_{index}() {{ let _ = \"aft_bench_unique_token_{index}\"; }}\n"
            ),
        )
        .expect("write lib");
    }

    let ignored_root = root.join("ignored_bulk");
    eprintln!("generating {ignored_files} ignored files...");
    for index in 0..ignored_files {
        let sub = ignored_root.join(format!("nm_{index:05}"));
        fs::create_dir_all(&sub).expect("mkdir ignored sub");
        fs::write(sub.join("junk.rs"), "fn junk() {}\n").expect("write junk");
    }

    if Command::new("git").arg("--version").output().is_ok() {
        let _ = Command::new("git").arg("init").arg(root).status();
        let _ = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "user.email", "bench@aft.invalid"])
            .status();
        let _ = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["config", "user.name", "AFT Bench"])
            .status();
    }
    eprintln!("fixture ready at {}", root.display());
}

fn measure_fixture(root: &Path) {
    let canonical = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    eprintln!("=== AFT search startup bench @ {} ===", canonical.display());

    let t0 = Instant::now();
    let _fp = ignore_rules_fingerprint(&canonical);
    eprintln!(
        "#4 ignore_rules_fingerprint: {} ms",
        t0.elapsed().as_millis()
    );

    let t0 = Instant::now();
    let index = SearchIndex::build_with_limit(&canonical, 1_048_576);
    eprintln!(
        "#3 cold index build: {} ms, files={}, trigrams={}",
        t0.elapsed().as_millis(),
        index.file_count(),
        index.trigram_count()
    );

    let compile = aft::pattern_compile::compile(
        "pub fn bench_fn",
        CompileOpts {
            case_insensitive: false,
            ..CompileOpts::default()
        },
    );
    let compiled = match compile {
        CompileResult::Ok(c) => c,
        _ => {
            eprintln!("pattern compile failed");
            std::process::exit(1);
        }
    };

    let t0 = Instant::now();
    let indexed = match index.search_grep(&compiled, &[], &[], &canonical, 10) {
        Ok(result) => result,
        Err(error) => {
            eprintln!("indexed grep failed: {error}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "#2 indexed grep cap=10: {} ms, files_searched={}, matches={}",
        t0.elapsed().as_millis(),
        indexed.files_searched,
        indexed.matches.len()
    );

    let t0 = Instant::now();
    let fallback = fallback_grep_bench(&canonical, &canonical, &canonical, &compiled, &[], &[], 10);
    eprintln!(
        "#2 fallback grep cap=10: {} ms, files_searched={}, matches={}",
        t0.elapsed().as_millis(),
        fallback.files_searched,
        fallback.matches.len()
    );

    eprintln!("=== done ===");
}
