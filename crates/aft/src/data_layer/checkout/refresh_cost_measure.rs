//! Measurement only: the cost of a refresh that re-resolves the method-call
//! sites of a declaration change's readers, against a cold build and an
//! engine start, on a copy of a real checkout. The figures in
//! `refresh_budget`'s module documentation came from it:
//!
//! ```text
//! cargo test --profile stage -p agent-file-tools --lib -- --ignored --nocapture \
//!     data_layer::checkout::refresh_cost_measure
//! ```
//!
//! `AFT_TEST_REFRESH_COST_PROJECT` names the checkout to copy (AFT's own by
//! default), `AFT_TEST_REFRESH_COST_LANG` (`ts` or `java`) writes the probe
//! files in that language rather than Rust, `AFT_TEST_REFRESH_COST_TYPE_FILE`
//! names a file of the checkout whose last class gains a `get` method, and
//! `AFT_TEST_REFRESH_COST_THREADS` sets the data layer's threads (8 by
//! default). Each step prints its wall and CPU time and its peak resident
//! size, and each refresh what its budget admitted, stage by stage.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use aft_store::ids::{Digest128, FamilyId};

use super::{Checkout, DataLayer};
use crate::data_layer::refresh_budget::RECORD;

fn copy_tree(from: &Path, to: &Path) {
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if matches!(
            name.as_ref(),
            "target" | "node_modules" | ".git" | "dist" | ".cortexkit" | "bench-data"
        ) {
            continue;
        }
        let kind = entry.file_type().unwrap();
        let dest = to.join(entry.file_name());
        if kind.is_dir() {
            std::fs::create_dir_all(&dest).unwrap();
            copy_tree(&entry.path(), &dest);
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &dest).unwrap();
        }
    }
}

fn cpu_ms() -> f64 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage fills the struct it is given.
    let usage = unsafe {
        libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
        usage.assume_init()
    };
    let ms = |time: libc::timeval| time.tv_sec as f64 * 1000.0 + time.tv_usec as f64 / 1000.0;
    ms(usage.ru_utime) + ms(usage.ru_stime)
}

/// A line of `/proc/self/status`, in MiB; `None` off Linux.
fn status_mib(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with(field))?;
    let kib = line.split_whitespace().nth(1)?.parse::<u64>().ok()?;
    Some(kib >> 10)
}

fn timed<T>(run: impl FnOnce() -> T) -> (T, String) {
    // Resets the process's peak resident size to its current one (Linux),
    // so the peak read afterwards is this step's.
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let rss = status_mib("VmRSS:");
    let (wall, cpu) = (Instant::now(), cpu_ms());
    let value = run();
    (
        value,
        format!(
            "wall_ms={} cpu_ms={:.0} rss_before_mib={rss:?} peak_rss_mib={:?}",
            wall.elapsed().as_millis(),
            cpu_ms() - cpu,
            status_mib("VmHWM:")
        ),
    )
}

fn record(checkout: &Checkout, path: &str) {
    checkout.record_paths(&[path.as_bytes().to_vec()]).unwrap();
}

fn write(checkout: &Checkout, path: &str, text: &str) {
    let full = checkout.root().join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(&full, text).unwrap();
    record(checkout, path);
}

fn refresh(checkout: &Checkout, label: &str) {
    *RECORD.lock().unwrap() = Some(Vec::new());
    let starts = checkout.engine_starts();
    let (stats, time) = timed(|| checkout.refresh_callgraph().unwrap());
    let log = RECORD.lock().unwrap().take().unwrap();
    eprintln!(
        "MEASURE {label}: {time} engine_started={} stats={:?} {}",
        checkout.engine_starts() - starts,
        stats.map(|stats| (
            stats.cold_build,
            stats.changed_paths,
            stats.rebound,
            stats.resolved,
            stats.dispatch_recomputed,
            stats.sites_resolved,
            stats.touched.len()
        )),
        log.join(" | ")
    );
}

fn publish(checkout: &Checkout, label: &str) {
    let (report, time) = timed(|| checkout.publish_changes().unwrap());
    eprintln!(
        "MEASURE {label} publish: {time} published={:?}",
        report.map(|report| (report.paths, report.fragments))
    );
}

fn ident(name: &str) -> bool {
    const RUST_KEYWORDS: &[&str] = &[
        "as",
        "break",
        "const",
        "continue",
        "crate",
        "else",
        "enum",
        "extern",
        "false",
        "fn",
        "for",
        "if",
        "impl",
        "in",
        "let",
        "loop",
        "match",
        "mod",
        "move",
        "mut",
        "pub",
        "ref",
        "return",
        "self",
        "Self",
        "static",
        "struct",
        "super",
        "trait",
        "true",
        "type",
        "unsafe",
        "use",
        "where",
        "while",
        "async",
        "await",
        "dyn",
        "abstract",
        "become",
        "box",
        "do",
        "final",
        "macro",
        "override",
        "priv",
        "typeof",
        "unsized",
        "virtual",
        "yield",
        "try",
        "constructor",
    ];
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !RUST_KEYWORDS.contains(&name)
}

/// The language the probe files are written in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Rust,
    TypeScript,
    Java,
}

impl Lang {
    fn extension(self) -> &'static str {
        match self {
            Self::Rust => "rs",
            Self::TypeScript => "ts",
            Self::Java => "java",
        }
    }

    /// A method `name` declared in this language, for inserting into a
    /// class body.
    fn method(self, name: &str) -> String {
        match self {
            Self::Rust => format!("    pub fn {name}(&self) -> usize {{ 0 }}\n"),
            Self::TypeScript => format!("  {name}(): number {{ return 0; }}\n"),
            Self::Java => format!("    public int {name}() {{ return 0; }}\n"),
        }
    }
}

const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "try",
    "void",
    "volatile",
    "while",
];

fn declaring_file(lang: Lang, n: usize, names: &[String]) -> (String, String) {
    let methods = names
        .iter()
        .filter(|name| lang != Lang::Java || !JAVA_KEYWORDS.contains(&name.as_str()))
        .map(|name| lang.method(name))
        .collect::<String>();
    let path = format!("probe_fan/ProbeFan{n}.{}", lang.extension());
    let text = match lang {
        Lang::Rust => format!("pub struct ProbeFan{n};\nimpl ProbeFan{n} {{\n{methods}}}\n"),
        Lang::TypeScript => format!("export class ProbeFan{n} {{\n{methods}}}\n"),
        Lang::Java => format!("package probe_fan;\n\npublic class ProbeFan{n} {{\n{methods}}}\n"),
    };
    (path, text)
}

#[test]
#[ignore = "measurement; AFT_TEST_REFRESH_COST_PROJECT names a checkout to copy"]
fn measure_method_reader_refreshes() {
    let source = std::env::var_os("AFT_TEST_REFRESH_COST_PROJECT").map_or_else(
        || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        PathBuf::from,
    );
    let lang = match std::env::var("AFT_TEST_REFRESH_COST_LANG").as_deref() {
        Ok("ts") => Lang::TypeScript,
        Ok("java") => Lang::Java,
        _ => Lang::Rust,
    };
    let threads = std::env::var("AFT_TEST_REFRESH_COST_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8);
    let root = tempfile::tempdir().unwrap();
    copy_tree(&source, root.path());
    let store = tempfile::tempdir().unwrap();
    let layer = Arc::new(DataLayer::new(64 << 20, threads));
    let family = FamilyId::from_digest(Digest128::of(b"probe"));
    let checkout = Checkout::bind(layer, store.path(), family, root.path()).unwrap();
    let (report, time) = timed(|| checkout.build().unwrap());
    eprintln!(
        "MEASURE first build: {time} files={} parsed={}",
        report.files, report.parsed_files
    );
    let parsed = report.parsed_files;
    let warm = &format!("probe_fan/Warm.{}", lang.extension());
    write(&checkout, warm, "// warm 0\n");
    refresh(&checkout, "engine start + one new file");
    publish(&checkout, "warm");

    let (report, time) = timed(|| checkout.build().unwrap());
    eprintln!(
        "MEASURE cold rebuild (stored records): {time} parsed={} stored_records={}",
        report.parsed_files, report.stored_records
    );
    let (_, time) = timed(|| checkout.measure_engine_start().unwrap());
    eprintln!("MEASURE engine start alone: {time}");
    write(&checkout, warm, "// warm 1\n");
    refresh(&checkout, "engine start + one comment edit");
    publish(&checkout, "warm 1");
    write(&checkout, warm, "// warm 2\n");
    refresh(&checkout, "warm engine, one comment edit");
    publish(&checkout, "warm 2");

    // Every declared method name and the files whose sites read it.
    let (readers, time) = timed(|| {
        checkout
            .inspect_engine(|live| {
                let mut names = BTreeSet::new();
                for path in live.inputs.parses.keys() {
                    if let Some((_, parse)) = live.inputs.parses.read(path) {
                        for method in &parse.dispatch.methods {
                            if ident(&method.name) {
                                names.insert(method.name.clone());
                            }
                        }
                    }
                }
                names
                    .into_iter()
                    .filter_map(|name| {
                        let files = live.method_site_readers(&name);
                        (files.len() >= 10).then_some((name, files))
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap()
            .unwrap()
    });
    let mut ranked = readers.iter().collect::<Vec<_>>();
    ranked.sort_by_key(|(name, files)| (std::cmp::Reverse(files.len()), (*name).clone()));
    eprintln!(
        "MEASURE method names with 10+ readers: {} ({time}); top: {}",
        ranked.len(),
        ranked
            .iter()
            .take(25)
            .map(|(name, files)| format!("{name}={}", files.len()))
            .collect::<Vec<_>>()
            .join(" ")
    );

    for (n, target) in [0.05, 0.10, 0.25, 0.50, 0.75].into_iter().enumerate() {
        let wanted = (parsed as f64 * target) as usize;
        let mut union = BTreeSet::new();
        let mut chosen = Vec::new();
        for (name, files) in &ranked {
            if union.len() >= wanted {
                break;
            }
            // Skip a name that would overshoot the target by more than half.
            let grown = union.union(files).count();
            if grown > wanted + wanted / 2 && !chosen.is_empty() {
                continue;
            }
            union.extend(files.iter().cloned());
            chosen.push((*name).clone());
        }
        let label = format!(
            "target {:.0}% ({} readers by {} names)",
            target * 100.0,
            union.len(),
            chosen.len()
        );
        let (path, text) = declaring_file(lang, n, &chosen);
        write(&checkout, &path, &text);
        refresh(&checkout, &format!("{label} add file"));
        publish(&checkout, &label);
        std::fs::remove_file(checkout.root().join(&path)).unwrap();
        record(&checkout, &path);
        refresh(&checkout, &format!("{label} remove file"));
        publish(&checkout, &format!("{label} removed"));
    }

    // Realistic fan-out edits.
    let common = [
        "get", "set", "has", "add", "delete", "update", "new", "len", "map", "find", "insert",
        "remove", "push", "clear", "close",
    ]
    .map(str::to_string);
    let (path, text) = declaring_file(lang, 100, &common);
    write(&checkout, &path, &text);
    refresh(&checkout, "new file declaring common method names");
    publish(&checkout, "common names");
    let (path, text) = declaring_file(lang, 101, &["get".to_string(), "new".to_string()]);
    write(&checkout, &path, &text);
    refresh(&checkout, "new file declaring get and new");
    publish(&checkout, "get and new");
    // Methods added to a type in a file that is already published: only the
    // readers of the added method names are reached, not those of every
    // method the file declares (as for a new file).
    let (_, text) = declaring_file(lang, 102, &["probe_only".to_string()]);
    write(&checkout, warm, &text);
    refresh(
        &checkout,
        "existing file gains a type with one unique method",
    );
    publish(&checkout, "existing file");
    let (_, text) = declaring_file(lang, 102, &["probe_only".to_string(), "get".to_string()]);
    write(&checkout, warm, &text);
    refresh(&checkout, "existing type gains method get");
    publish(&checkout, "existing get");
    let (_, text) = declaring_file(
        lang,
        102,
        &[
            "probe_only".to_string(),
            "get".to_string(),
            "new".to_string(),
        ],
    );
    write(&checkout, warm, &text);
    refresh(&checkout, "existing type gains method new");
    publish(&checkout, "existing new");

    // `get` added to a widely used class of the checkout itself, before its
    // closing brace.
    if let Ok(path) = std::env::var("AFT_TEST_REFRESH_COST_TYPE_FILE") {
        let full = checkout.root().join(&path);
        let mut text = std::fs::read_to_string(&full).unwrap();
        let end = text.rfind('}').expect("a closing brace");
        text.insert_str(end, &lang.method("get"));
        write(&checkout, &path, &text);
        refresh(&checkout, &format!("{path} gains method get"));
        publish(&checkout, "type file");
    }
}
