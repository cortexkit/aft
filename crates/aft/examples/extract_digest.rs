//! Prints a digest of every file's call-graph parse output, for comparing
//! two builds of the extractor.
//!
//! ```text
//! extract_digest <root> <file list>
//! ```
//!
//! The list names one path per line, relative to the root. Each output line
//! is `path<TAB>language<TAB>digest`, the digest being BLAKE3 of the stored
//! payload bytes. It uses only interfaces older checkouts also have, so the
//! same file can be copied into an older checkout's `crates/aft/examples/`
//! and the two outputs diffed.

#![allow(clippy::unwrap_used, clippy::print_stdout)]

use std::path::Path;

use aft::callgraph_store::join::CallgraphBlob;
use aft::data_layer::content::language_for;

fn main() {
    let mut args = std::env::args().skip(1);
    let root = args.next().unwrap();
    let list = args.next().unwrap();
    let root = Path::new(&root);
    for path in std::fs::read_to_string(list).unwrap().lines() {
        let full = root.join(path);
        let Ok(metadata) = std::fs::symlink_metadata(&full) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > 1024 * 1024 {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        if Path::new(path).extension().is_some_and(|ext| ext == "h") {
            let _ = aft::parser::detect_language(&full);
        }
        let Some(language) = language_for(path, text.as_bytes()) else {
            continue;
        };
        if language == "config" {
            continue;
        }
        let digest = match CallgraphBlob::extract(&text, &language, "digest") {
            Ok(blob) => blake3::hash(&blob.to_bytes().unwrap()).to_hex().to_string(),
            Err(error) => format!("error: {error}"),
        };
        println!("{path}\t{language}\t{digest}");
    }
}
