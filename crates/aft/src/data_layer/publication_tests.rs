//! An incremental engine started the way a running checkout starts it, from
//! a publication, must give exactly what a cold resolution gives after every
//! edit.
//!
//! `LiveCheckout::cold` keeps every reverse input, binding and method-call
//! input in memory. A running checkout starts its engine with
//! `LiveCheckout::from_resolved_timed` over its published segments instead:
//! reverse inputs come from the stored terms, files are bound when first
//! needed, method-call inputs are computed when first read, and parses and
//! resolutions are decoded when first read. These tests apply edit
//! sequences to engines started both ways and, after every step, compare
//! every file's answers, recorded reads and fragment bytes with a cold
//! resolution of the same files, and ask `stale_recordings()` for any
//! recorded read that no longer holds.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use aft_store::cache::BlockCache;
use aft_store::checkout::{CheckoutState, PathKind, PathRecord, StatEvidence};
use aft_store::content::{ContentRecord, RecordBody, RecordKey};
use aft_store::graph::FragmentKey;
use aft_store::ids::{ContentKey, Digest128, FamilyId, SegmentId, WriterIncarnation};
use aft_store::segment::{Segment, SegmentMeta};

use super::callgraph::{resolve_checkout, CheckoutInputs, FileResolution, Parses, Resolutions};
use super::content::{blob_from_record, extract, language_for, EXTRACTOR_VERSION};
use super::graph_store::{known_sections, write_segment, StoredSlices};
use super::incremental::LiveCheckout;
use crate::callgraph_store::join::CallgraphBlob;

/// Every parsed file's content record, by the parse key its parse carries.
type Records = HashMap<String, (RecordKey, Arc<ContentRecord>)>;
type Fragments = BTreeMap<String, FragmentKey>;

/// A checkout's files as the call graph reads them, with the content
/// records a segment of it stores.
struct Fixture {
    inputs: CheckoutInputs,
    records: Records,
}

/// A checkout as text files and symlinks (path to target), each step of a
/// sequence derived from the one before.
#[derive(Clone, Default)]
struct Tree {
    files: BTreeMap<String, String>,
    links: BTreeMap<String, String>,
}

impl Tree {
    fn file(mut self, path: &str, text: &str) -> Self {
        self.links.remove(path);
        self.files.insert(path.to_string(), text.to_string());
        self
    }

    fn link(mut self, path: &str, target: &str) -> Self {
        self.files.remove(path);
        self.links.insert(path.to_string(), target.to_string());
        self
    }

    /// Without `path`, and without everything under it when it is a folder.
    fn without(mut self, path: &str) -> Self {
        let folder = format!("{path}/");
        self.files
            .retain(|name, _| name != path && !name.starts_with(&folder));
        self.links
            .retain(|name, _| name != path && !name.starts_with(&folder));
        self
    }

    /// `from` moved to `to`, with the same text.
    fn rename(self, from: &str, to: &str) -> Self {
        let text = self.files[from].clone();
        self.without(from).file(to, &text)
    }

    fn fixture(&self) -> Fixture {
        let files = self
            .files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect::<Vec<_>>();
        let links = self
            .links
            .iter()
            .map(|(path, target)| (path.as_str(), target.as_str()))
            .collect::<Vec<_>>();
        checkout(&files, &links)
    }
}

fn checkout(files: &[(&str, &str)], links: &[(&str, &str)]) -> Fixture {
    let mut records = Vec::new();
    let mut parses = BTreeMap::new();
    let mut configs = HashMap::new();
    let mut stored = Records::new();
    for (path, text) in files {
        let bytes = text.as_bytes();
        let content = ContentKey::of(bytes);
        let language = language_for(path, bytes).unwrap_or_default();
        records.push(PathRecord {
            path: path.as_bytes().to_vec(),
            mode: 0o100_644,
            kind: PathKind::Text,
            content,
            link: Vec::new(),
            stat: StatEvidence::default(),
            language: language.clone(),
        });
        if language.is_empty() {
            continue;
        }
        let record = extract(bytes, &language).unwrap();
        let record_key = RecordKey::new(content, &language, EXTRACTOR_VERSION);
        let key = record_key.digest().to_string();
        stored.insert(key.clone(), (record_key, Arc::new(record.clone())));
        match &record.data {
            RecordBody::Config { source } => {
                configs.insert(path.as_bytes().to_vec(), Arc::from(source.as_slice()));
            }
            RecordBody::Parsed { .. } => {
                let CallgraphBlob::Parse(parse) = blob_from_record(&record).unwrap() else {
                    unreachable!("a parsed record converts to a parse");
                };
                parses.insert((*path).to_string(), (key, Arc::new(parse)));
            }
        }
    }
    for (path, target) in links {
        records.push(PathRecord {
            path: path.as_bytes().to_vec(),
            mode: 0o120_000,
            kind: PathKind::Symlink,
            content: ContentKey::of(target.as_bytes()),
            link: target.as_bytes().to_vec(),
            stat: StatEvidence::default(),
            language: String::new(),
        });
    }
    records.sort_by(|a, b| a.path.cmp(&b.path));
    Fixture {
        inputs: CheckoutInputs {
            state: CheckoutState::new(Vec::new(), Digest128::of(b""), records).unwrap(),
            parses: parses.into(),
            configs,
        },
        records: stored,
    }
}

fn family() -> FamilyId {
    FamilyId::from_digest(Digest128::of(b"publication tests"))
}

/// Writes `fixture`'s cold resolution as segment `id` under `dir`, as a
/// build publishes it.
fn write(fixture: &Fixture, dir: &Path, id: u64) -> (Arc<Segment>, Fragments) {
    let resolved = resolve_checkout(&fixture.inputs, 2).unwrap();
    let path = dir.join(format!("{id}.seg"));
    let fragments = write_segment(
        &path,
        SegmentMeta {
            family: family(),
            segment: SegmentId::new(id),
            incarnation: WriterIncarnation::new(1),
            created_unix_ms: 0,
        },
        &fixture.inputs,
        &resolved,
        &fixture.records,
    )
    .unwrap();
    let segment = Arc::new(
        Segment::open(
            &path,
            family(),
            &known_sections(),
            Arc::new(BlockCache::new(8 << 20)),
        )
        .unwrap(),
    );
    (segment, fragments)
}

/// One way of running the incremental engine over a sequence.
struct Engine {
    label: &'static str,
    live: LiveCheckout,
    /// The publication it started from; `None` for a cold start.
    stored: Option<Arc<StoredSlices>>,
}

impl Engine {
    /// Started the way a running checkout starts it: from stored slices
    /// over `segments` (oldest first), with the fragments `fragments` names.
    fn published(
        label: &'static str,
        fixture: &Fixture,
        segments: Vec<Arc<Segment>>,
        fragments: &Fragments,
    ) -> Self {
        let stored = Arc::new(StoredSlices::open(segments, &fixture.inputs, fragments).unwrap());
        let inputs = CheckoutInputs {
            state: fixture.inputs.state.clone(),
            parses: Parses::stored(&stored),
            configs: fixture.inputs.configs.clone(),
        };
        let live = LiveCheckout::from_resolved_timed(
            inputs,
            Resolutions::stored(&stored),
            Arc::clone(&stored),
            &mut Vec::new(),
        )
        .unwrap();
        Self {
            label,
            live,
            stored: Some(stored),
        }
    }

    fn cold(fixture: &Fixture) -> Self {
        Self {
            label: "cold start",
            live: LiveCheckout::cold(fixture.inputs.clone()).unwrap(),
            stored: None,
        }
    }
}

/// Paths whose record differs between two checkouts.
fn changed(before: &CheckoutInputs, after: &CheckoutInputs) -> BTreeSet<String> {
    let same = |a: &PathRecord, b: &PathRecord| {
        a.kind == b.kind && a.content == b.content && a.link == b.link
    };
    let mut out = BTreeSet::new();
    for record in before.state.paths() {
        match after.state.get(&record.path) {
            Some(now) if same(record, now) => {}
            _ => {
                out.insert(String::from_utf8(record.path.clone()).unwrap());
            }
        }
    }
    for record in after.state.paths() {
        if before.state.get(&record.path).is_none() {
            out.insert(String::from_utf8(record.path.clone()).unwrap());
        }
    }
    out
}

/// The next inputs as `Checkout::refresh_callgraph` builds them: the
/// engine's own parses and configuration, with the changed paths replaced.
fn next(previous: &CheckoutInputs, after: &Fixture, changed: &BTreeSet<String>) -> CheckoutInputs {
    let mut parses = previous.parses.clone();
    let mut configs = previous.configs.clone();
    for path in changed {
        parses.remove(path);
        configs.remove(path.as_bytes());
        if let Some(value) = after.inputs.parses.get(path) {
            parses.insert(path.clone(), value.clone());
        }
        if let Some(bytes) = after.inputs.configs.get(path.as_bytes()) {
            configs.insert(path.as_bytes().to_vec(), Arc::clone(bytes));
        }
    }
    CheckoutInputs {
        state: after.inputs.state.clone(),
        parses,
        configs,
    }
}

/// Every way `live` differs from `cold`, a cold resolution of the same
/// files.
fn differences(live: &LiveCheckout, cold: &BTreeMap<String, FileResolution>) -> Vec<String> {
    let mut out = Vec::new();
    let ours = live.resolved.keys().cloned().collect::<BTreeSet<_>>();
    let theirs = cold.keys().cloned().collect::<BTreeSet<_>>();
    if ours != theirs {
        out.push(format!("files: engine {ours:?} cold {theirs:?}"));
    }
    for (path, theirs) in cold {
        let Some(ours) = live.resolved.get(path) else {
            continue;
        };
        if ours.rows != theirs.rows {
            out.push(format!("{path}: rows"));
        }
        if ours.sites != theirs.sites {
            out.push(format!(
                "{path}: method-call sites engine={:?} cold={:?}",
                ours.sites
                    .iter()
                    .map(|site| format!("{:?}", site.resolution))
                    .collect::<Vec<_>>(),
                theirs
                    .sites
                    .iter()
                    .map(|site| format!("{:?}", site.resolution))
                    .collect::<Vec<_>>()
            ));
        }
        if ours.site_dependencies != theirs.site_dependencies {
            out.push(format!(
                "{path}: method-call dependency keys engine={:?} cold={:?}",
                ours.site_dependencies, theirs.site_dependencies
            ));
        }
        if ours.dispatch != theirs.dispatch {
            out.push(format!("{path}: dispatch targets or trace"));
        }
        // The bound surface is computed when a file is bound, never stored:
        // compare everything else binding recorded.
        if ours.binding.without_surface() != theirs.binding.without_surface() {
            out.push(format!(
                "{path}: recorded reads engine={:?} cold={:?}",
                ours.binding.answers().collect::<Vec<_>>(),
                theirs.binding.answers().collect::<Vec<_>>()
            ));
        }
        if super::fragment::encode(ours) != super::fragment::encode(theirs) {
            out.push(format!("{path}: stored fragment bytes differ"));
        }
    }
    out.extend(
        live.stale_recordings()
            .into_iter()
            .map(|path| format!("{path}: a recorded read or surface answer no longer holds")),
    );
    out
}

/// Files outside `changed` whose stored fragment differs between `before`
/// and `after`, two cold resolutions: the step changed an answer or a
/// recorded read of a file it did not edit, which an update must find.
fn moved(
    before: &BTreeMap<String, FileResolution>,
    after: &BTreeMap<String, FileResolution>,
    changed: &BTreeSet<String>,
) -> usize {
    before
        .iter()
        .filter(|(path, file)| {
            !changed.contains(*path)
                && after.get(*path).is_some_and(|now| {
                    super::fragment::encode(now) != super::fragment::encode(file)
                })
        })
        .count()
}

/// Runs `steps` through engines started four ways, comparing each with a
/// cold resolution after every step:
///
/// - `publication`: started from a publication of the first step, then
///   updated through every step, as a long-running checkout is;
/// - `shared publication`: the same, over its own segment and a newer one
///   that a second checkout of the same files wrote, so every fragment is
///   read from the other checkout's copy;
/// - `republished`: started from a publication of the step before, then
///   updated by one step, as the first refresh after a build is;
/// - `cold start`: started with `LiveCheckout::cold`, with no publication,
///   as `edit_gate.rs` starts its engine.
///
/// With `every_step_but`, each step not listed in it must change an answer
/// or recorded read of a file it did not edit, or the step proves nothing;
/// without it only the sequence as a whole must change one.
fn run(name: &str, steps: &[(&str, Fixture)], every_step_but: Option<&[&str]>) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let first = &steps[0].1;
    let (own, fragments) = write(first, dir.path(), 1);
    let (sibling, sibling_fragments) = write(first, dir.path(), 2);
    let mut problems = Vec::new();
    if sibling_fragments != fragments {
        problems.push(format!(
            "{name}: two checkouts of the same files stored different fragments"
        ));
    }
    let mut engines = vec![
        Engine::published("publication", first, vec![Arc::clone(&own)], &fragments),
        Engine::published("shared publication", first, vec![own, sibling], &fragments),
        Engine::cold(first),
    ];
    let mut segment_id = 3;
    let mut previous = resolve_checkout(&first.inputs, 1).unwrap();
    let mut total_moved = 0;
    for pair in steps.windows(2) {
        let ((_, before), (label, after)) = (&pair[0], &pair[1]);
        let changed = changed(&before.inputs, &after.inputs);
        assert!(
            !changed.is_empty(),
            "{name} {label}: the step changes nothing"
        );
        let (segment, fragments) = write(before, dir.path(), segment_id);
        segment_id += 1;
        let mut republished = Engine::published("republished", before, vec![segment], &fragments);
        let cold = resolve_checkout(&after.inputs, 1).unwrap();
        if cold.is_empty() {
            problems.push(format!("{name} {label}: the checkout resolves no file"));
        }
        for engine in engines.iter_mut().chain(std::iter::once(&mut republished)) {
            let inputs = next(&engine.live.inputs, after, &changed);
            let at = format!("{name} {label} [{}]", engine.label);
            match engine.live.update(inputs, &changed) {
                Ok(_) => problems.extend(
                    differences(&engine.live, &cold)
                        .into_iter()
                        .map(|difference| format!("{at}: {difference}")),
                ),
                Err(error) => problems.push(format!("{at}: update failed: {error}")),
            }
            if let Some(stored) = &engine.stored {
                problems.extend(
                    stored
                        .failures()
                        .into_iter()
                        .map(|failure| format!("{at}: stored read failed: {failure}")),
                );
            }
        }
        let moved = moved(&previous, &cold, &changed);
        eprintln!("{name} {label}: {moved} files it did not edit answer differently");
        if every_step_but.is_some_and(|labels| !labels.contains(label)) && moved == 0 {
            problems.push(format!(
                "{name} {label}: the step moved no answer of a file it did not edit"
            ));
        }
        total_moved += moved;
        previous = cold;
    }
    if total_moved == 0 {
        problems.push(format!("{name}: the fixture moved no answer"));
    }
    problems
}

/// Labels each step of a sequence by its position.
fn numbered(steps: Vec<Fixture>) -> Vec<(&'static str, Fixture)> {
    const LABELS: [&str; 6] = ["base", "step 1", "step 2", "step 3", "step 4", "step 5"];
    assert!(steps.len() <= LABELS.len());
    LABELS.into_iter().zip(steps).collect()
}

/// Symlink chains, folders that become symlinks, name-matched methods, Go
/// methods and interfaces, Rust modules, a Python module that becomes a
/// package, and TypeScript path aliases.
#[test]
fn engine_from_a_publication_matches_cold() {
    let mut problems = Vec::new();

    // A TypeScript import through a symlink chain whose middle hop moves,
    // then whose head moves.
    let ts = [
        (
            "main.ts",
            "import { run } from './alias/lib';\nexport function main() { run(); }\n",
        ),
        ("old/lib.ts", "export function run() { return 1; }\n"),
        (
            "new/lib.ts",
            "export function other() { return 2; }\nexport function run() { return 2; }\n",
        ),
    ];
    problems.extend(run(
        "symlink chain",
        &numbered(vec![
            checkout(&ts, &[("alias", "mid"), ("mid", "old")]),
            checkout(&ts, &[("alias", "mid"), ("mid", "new")]),
            checkout(&ts, &[("alias", "old"), ("mid", "new")]),
        ]),
        None,
    ));

    // A real folder becomes a symlink to another folder.
    let main = (
        "main.ts",
        "import { run } from './lib/util';\nexport function main() { run(); }\n",
    );
    problems.extend(run(
        "folder becomes a symlink",
        &numbered(vec![
            checkout(
                &[
                    main,
                    ("lib/util.ts", "export function run() { return 1; }\n"),
                ],
                &[],
            ),
            checkout(
                &[
                    main,
                    ("alt/util.ts", "export function run() { return 2; }\n"),
                ],
                &[("lib", "alt")],
            ),
        ]),
        None,
    ));

    // Name-matched method calls: a method appears in a new file, then
    // leaves a published one, then the new file goes. A receiver of type
    // `any` matches no method by name today, so a typed receiver makes the
    // fixture move an answer.
    let caller = (
        "main.ts",
        "import { A } from './a';\nexport function main(x: any, a: A) { x.run(); x.stop(); a.run(); }\n",
    );
    let a_runs = ("a.ts", "export class A { run() { return 1; } }\n");
    let a_halts = ("a.ts", "export class A { halt() { return 1; } }\n");
    let b = ("b.ts", "export class B { run() { return 2; } stop() {} }\n");
    problems.extend(run(
        "typescript name-matched methods",
        &numbered(vec![
            checkout(&[caller, a_runs], &[]),
            checkout(&[caller, a_runs, b], &[]),
            checkout(&[caller, a_halts, b], &[]),
            checkout(&[caller, a_halts], &[]),
        ]),
        None,
    ));

    // Go: a typed receiver's method and an interface requirement appear in
    // new files of the package.
    let module = ("go.mod", "module example.com/m\n");
    let t = (
        "p/t.go",
        "package p\n\ntype T struct{}\n\nfunc (T) Run() {}\n",
    );
    let use_t = (
        "p/use.go",
        "package p\n\nfunc Use(t T) { t.Run(); t.Stop() }\n",
    );
    let stop = ("p/stop.go", "package p\n\nfunc (T) Stop() {}\n");
    let iface = (
        "p/iface.go",
        "package p\n\ntype Runner interface { Run() }\n\nfunc Call(r Runner) { r.Run() }\n",
    );
    problems.extend(run(
        "go methods and interfaces",
        &numbered(vec![
            checkout(&[module, t, use_t], &[]),
            checkout(&[module, t, use_t, stop], &[]),
            checkout(&[module, t, use_t, stop, iface], &[]),
        ]),
        None,
    ));

    // Rust: a module added, then its callee renamed.
    let cargo = ("Cargo.toml", "[package]\nname = \"demo\"\n");
    let lib_two = (
        "src/lib.rs",
        "mod util;\nmod extra;\npub fn caller() { util::callee(); }\n",
    );
    let extra = (
        "src/extra.rs",
        "use crate::util;\npub fn other() { util::callee(); }\n",
    );
    problems.extend(run(
        "rust modules",
        &numbered(vec![
            checkout(
                &[
                    cargo,
                    (
                        "src/lib.rs",
                        "mod util;\npub fn caller() { util::callee(); }\n",
                    ),
                    ("src/util.rs", "pub fn callee() {}\n"),
                ],
                &[],
            ),
            checkout(
                &[
                    cargo,
                    lib_two,
                    ("src/util.rs", "pub fn callee() {}\n"),
                    extra,
                ],
                &[],
            ),
            checkout(
                &[
                    cargo,
                    lib_two,
                    ("src/util.rs", "pub fn renamed() {}\n"),
                    extra,
                ],
                &[],
            ),
        ]),
        None,
    ));

    // Python: a module becomes a package.
    let py_main = ("main.py", "from pkg.mod import f\n\ndef main():\n    f()\n");
    problems.extend(run(
        "python module becomes a package",
        &numbered(vec![
            checkout(
                &[
                    py_main,
                    ("pkg/__init__.py", ""),
                    ("pkg/mod.py", "def f():\n    return 1\n"),
                ],
                &[],
            ),
            checkout(
                &[
                    py_main,
                    ("pkg/__init__.py", ""),
                    ("pkg/mod/__init__.py", "def f():\n    return 2\n"),
                ],
                &[],
            ),
        ]),
        None,
    ));

    // TypeScript path aliases: a tsconfig field changes, then the tsconfig
    // becomes a symlink to another config.
    let alias_main = (
        "main.ts",
        "import { run } from '@lib/lib';\nexport function main() { run(); }\n",
    );
    let old_lib = ("old/lib.ts", "export function run() { return 1; }\n");
    let new_lib = ("new/lib.ts", "export function run() { return 2; }\n");
    let (old_config, new_config) = (tsconfig_paths("old"), tsconfig_paths("new"));
    problems.extend(run(
        "tsconfig paths",
        &numbered(vec![
            checkout(
                &[
                    alias_main,
                    old_lib,
                    new_lib,
                    ("tsconfig.json", old_config.as_str()),
                ],
                &[],
            ),
            checkout(
                &[
                    alias_main,
                    old_lib,
                    new_lib,
                    ("tsconfig.json", new_config.as_str()),
                ],
                &[],
            ),
            checkout(
                &[
                    alias_main,
                    old_lib,
                    new_lib,
                    ("configs/tsconfig.json", old_config.as_str()),
                ],
                &[("tsconfig.json", "configs/tsconfig.json")],
            ),
        ]),
        None,
    ));

    assert!(problems.is_empty(), "{problems:#?}");
}

/// A tsconfig mapping `@lib/*` to `<target>/*`.
fn tsconfig_paths(target: &str) -> String {
    format!(
        "{{\"compilerOptions\":{{\"baseUrl\":\".\",\"paths\":{{\"@lib/*\":[\"{target}/*\"]}}}}}}\n"
    )
}

/// Every kind of edit a checkout sees, in turn, on one TypeScript project:
/// each step must move an answer of a file it did not edit.
#[test]
fn typescript_edits_from_a_publication_match_cold() {
    let main = "import { run } from '@lib/lib';\n\
        import { helper } from './link/helper';\n\
        import { util } from './pkg';\n\
        import { moved } from './moved';\n\
        import { extra } from './extra';\n\
        export function main() { run(); helper(); util(); moved(); extra(); }\n";
    let base = Tree::default()
        .file("tsconfig.json", &tsconfig_paths("old"))
        .file("src/main.ts", main)
        .file("old/lib.ts", "export function run() { return 1; }\n")
        .file("new/lib.ts", "export function run() { return 2; }\n")
        .file("src/extra.ts", "export function extra() {}\n")
        .file("a/helper.ts", "export function helper() { return 1; }\n")
        .file(
            "b/helper.ts",
            "export function other() {}\nexport function helper() { return 2; }\n",
        )
        .link("src/link", "../a");
    let content = base
        .clone()
        .file("old/lib.ts", "export function other() { return 3; }\n");
    let added = content
        .clone()
        .file("src/moved.ts", "export function moved() {}\n");
    let renamed = added.clone().rename("src/moved.ts", "src/renamed.ts");
    let removed = renamed.clone().without("src/extra.ts");
    let retargeted = removed.clone().link("src/link", "../b");
    let config = retargeted
        .clone()
        .file("tsconfig.json", &tsconfig_paths("new"));
    let folder = config
        .clone()
        .file("src/pkg/index.ts", "export function util() {}\n")
        .file("src/pkg/more.ts", "export function more() {}\n");
    let no_folder = folder.clone().without("src/pkg");
    let steps = [
        ("base", base),
        ("content change", content),
        ("file added", added),
        ("file renamed", renamed),
        ("file removed", removed),
        ("symlink retargeted", retargeted),
        ("config field changed", config),
        ("folder added", folder),
        ("folder removed", no_folder),
    ];
    let steps = steps
        .iter()
        .map(|(label, tree)| (*label, tree.fixture()))
        .collect::<Vec<_>>();
    let problems = run("typescript edits", &steps, Some(&[]));
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Every kind of edit a checkout sees, in turn, on one Rust workspace: each
/// step must move an answer of a file it did not edit.
#[test]
fn rust_edits_from_a_publication_match_cold() {
    let start = "mod util;\nmod extra;\nmod later;\nmod net;\nmod tools;\n\
        pub fn start() {\n    util::callee();\n    extra::other();\n    later::run();\n    \
        net::client::connect();\n    tools::fmt::pretty();\n    dep::run();\n}\n";
    let base = Tree::default()
        .file("Cargo.toml", "[workspace]\nmembers = [\"app\", \"dep\"]\n")
        .file(
            "app/Cargo.toml",
            "[package]\nname = \"app\"\n\n[dependencies]\ndep = { path = \"../dep\" }\n",
        )
        .file("app/src/lib.rs", start)
        .file("app/src/util.rs", "pub fn callee() {}\n")
        .file("app/src/extra.rs", "pub fn other() {}\n")
        .file("app/src/net/mod.rs", "pub mod client;\n")
        .file("app/src/net/client.rs", "pub fn connect() {}\n")
        .file("dep/Cargo.toml", "[package]\nname = \"dep\"\n")
        .file("old/src/lib.rs", "pub fn run() {}\n")
        .file("new/src/lib.rs", "pub fn run() {}\npub fn more() {}\n")
        .link("dep/src", "../old/src");
    let content = base
        .clone()
        .file("app/src/util.rs", "pub fn renamed() {}\n");
    let added = content
        .clone()
        .file("app/src/later.rs", "pub fn run() {}\n");
    let renamed = added
        .clone()
        .rename("app/src/extra.rs", "app/src/extra/mod.rs");
    let removed = renamed.clone().without("app/src/net/client.rs");
    let retargeted = removed.clone().link("dep/src", "../new/src");
    let config = retargeted
        .clone()
        .file("dep/Cargo.toml", "[package]\nname = \"core\"\n");
    let folder = config
        .clone()
        .file("app/src/tools/mod.rs", "pub mod fmt;\n")
        .file("app/src/tools/fmt.rs", "pub fn pretty() {}\n");
    let no_folder = folder.clone().without("app/src/tools");
    let steps = [
        ("base", base),
        ("content change", content),
        ("file added", added),
        ("file renamed", renamed),
        ("file removed", removed),
        ("symlink retargeted", retargeted),
        ("config field changed", config),
        ("folder added", folder),
        ("folder removed", no_folder),
    ];
    let steps = steps
        .iter()
        .map(|(label, tree)| (*label, tree.fixture()))
        .collect::<Vec<_>>();
    let problems = run("rust edits", &steps, Some(&[]));
    assert!(problems.is_empty(), "{problems:#?}");
}

/// The first Rust edit after a restart: an engine started from a
/// publication has bound no file and computed no method-call input yet.
/// Resolution reads the module tree, built by binding every file that
/// declares modules, and resolving a method call reads every file that
/// declares a method of that name; the resolver does each one file at a
/// time on its own thread. On a large checkout that serial work made the
/// first refresh take seconds, so the update does both in parallel first,
/// and the resolver must find them done. The answers must not change.
#[test]
fn first_rust_update_from_a_publication_reads_nothing_one_file_at_a_time() {
    let helper = |calls: usize| {
        format!(
            "use crate::shapes::Square;\npub fn helper() {{\n    let square = Square;\n{}}}\n",
            "    square.area();\n".repeat(calls)
        )
    };
    let base = Tree::default()
        .file("Cargo.toml", "[package]\nname = \"demo\"\n")
        .file(
            "src/lib.rs",
            "mod shapes;\nmod util;\npub fn start() {\n    util::helper();\n}\n",
        )
        .file(
            "src/shapes.rs",
            "pub mod circle;\npub struct Square;\nimpl Square {\n    pub fn area(&self) -> u32 {\n        4\n    }\n}\n",
        )
        .file(
            "src/shapes/circle.rs",
            "pub struct Circle;\nimpl Circle {\n    pub fn area(&self) -> u32 {\n        3\n    }\n}\n",
        )
        .file("src/util.rs", &helper(1));
    let edited = base.clone().file("src/util.rs", &helper(2));
    let (before, after) = (base.fixture(), edited.fixture());
    let dir = tempfile::tempdir().unwrap();
    let (segment, fragments) = write(&before, dir.path(), 1);
    let mut engine = Engine::published("publication", &before, vec![segment], &fragments);
    let changed = changed(&before.inputs, &after.inputs);
    let inputs = next(&engine.live.inputs, &after, &changed);
    engine.live.update(inputs, &changed).unwrap();

    let (bound, computed) = engine.live.take_first_reads();
    for declaring in ["src/lib.rs", "src/shapes.rs"] {
        assert!(
            !bound.iter().any(|file| file == declaring),
            "resolution bound the module-declaring {declaring} itself: {bound:?}"
        );
    }
    for declarer in ["src/shapes.rs", "src/shapes/circle.rs"] {
        assert!(
            !computed.iter().any(|file| file == declarer),
            "the method-call resolver computed {declarer}'s inputs itself: {computed:?}"
        );
    }
    let cold = resolve_checkout(&after.inputs, 1).unwrap();
    assert!(
        !cold["src/util.rs"].sites.is_empty(),
        "the calls of `area` are method-call sites"
    );
    let problems = differences(&engine.live, &cold);
    assert!(problems.is_empty(), "{problems:#?}");
}

/// Every kind of edit a checkout sees, in turn, on one Go module: each step
/// must move an answer of a file it did not edit, except the symlink
/// retarget. The Go resolver follows neither a symlinked package folder nor
/// a symlinked `go.mod`, so that import stays unresolved either way; the
/// step still checks that the engine changes nothing a cold resolution
/// would not.
#[test]
fn go_edits_from_a_publication_match_cold() {
    let main = "package main\n\nimport (\n\t\"example.com/m/link\"\n\t\"example.com/m/p\"\n\
        \t\"example.com/m/q\"\n\t\"example.com/m/s\"\n)\n\n\
        func main() { p.Run(); q.Do(); link.Hop(); s.Go() }\n";
    let base = Tree::default()
        .file("go.mod", "module example.com/m\n")
        .file("app/main.go", main)
        .file("p/p.go", "package p\n\nfunc Run() {}\n")
        .file(
            "p/t.go",
            "package p\n\ntype T struct{}\n\nfunc (T) Run() {}\n",
        )
        .file(
            "p/use.go",
            "package p\n\nfunc Use(t T) { t.Run(); t.Stop() }\n",
        )
        .file("q/q.go", "package q\n\nfunc Do() {}\n")
        .file("impl/a/hop.go", "package link\n\nfunc Hop() {}\n")
        .file("impl/b/other.go", "package link\n\nfunc Other() {}\n")
        .link("link", "impl/a");
    let content = base.clone().file(
        "p/t.go",
        "package p\n\ntype T struct{}\n\nfunc (T) Running() {}\n",
    );
    let added = content
        .clone()
        .file("p/stop.go", "package p\n\nfunc (T) Stop() {}\n");
    let renamed = added.clone().rename("q/q.go", "q/do.go");
    let removed = renamed.clone().without("p/stop.go");
    let retargeted = removed.clone().link("link", "impl/b");
    let folder = retargeted
        .clone()
        .file("s/s.go", "package s\n\nfunc Go() {}\n")
        .file("s/more.go", "package s\n\nfunc More() {}\n");
    let no_folder = folder.clone().without("s");
    let config = no_folder
        .clone()
        .file("go.mod", "module example.com/renamed\n");
    let steps = [
        ("base", base),
        ("content change", content),
        ("file added", added),
        ("file renamed", renamed),
        ("file removed", removed),
        ("symlink retargeted", retargeted),
        ("folder added", folder),
        ("folder removed", no_folder),
        ("config field changed", config),
    ];
    let steps = steps
        .iter()
        .map(|(label, tree)| (*label, tree.fixture()))
        .collect::<Vec<_>>();
    let problems = run("go edits", &steps, Some(&["symlink retargeted"]));
    assert!(problems.is_empty(), "{problems:#?}");
}
