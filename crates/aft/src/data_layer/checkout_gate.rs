//! Replays real edits through data-layer checkouts on real files, the way
//! a running AFT drives them, and checks every answer and every recorded
//! input against a cold build of the same files.
//!
//! The edit pack (see [`super::edit_gate`]) is written to two folders, each
//! a Git repository with the same root commit, so both are checkouts of one
//! repository and share one family store:
//!
//! - Checkout A replays the pack commit by commit through a [`Checkout`]:
//!   each edit is written to disk, recorded with [`Checkout::record_paths`]
//!   and resolved with [`Checkout::refresh_callgraph`]. Every few edits,
//!   and at every third commit end, its changes are published on their own
//!   with [`Checkout::publish_changes`], as the runtime does when a checkout
//!   goes quiet; once A's text and call graph read from more than eight
//!   segments, A is built again whole, as the runtime does to fold them. A
//!   third of the way through, A is built again while a file it was never
//!   told about has changed and a watcher keeps reporting paths during the
//!   build. Two thirds through, A is dropped and bound again from the
//!   store, as a restarted process would be, with that commit's edits made
//!   while nothing watched. After that commit, A restarts once more over a
//!   store whose newest copies of the fragments A and B share are B's (see
//!   `restart_over_shared_fragments`).
//! - Checkout B runs through the runtime ([`DataCheckout`]), some commits
//!   behind A. It starts from a publication of its text index alone, as a
//!   process leaves it when it stops before its first call graph, so the
//!   runtime must build the call graph when it opens. Its edits arrive as
//!   watcher paths; the runtime refreshes and publishes them as it decides.
//!   It is built again for A's second restart, and later restarts too.
//!
//! Both checkouts also hold a small probe folder the pack lacks: a
//! TypeScript import through a chain of symlinks whose second link differs
//! between A and B, and a `.gitignore` whose rule moves early in the run.
//! A's second link is the only change made while A is down for its second
//! restart.
//!
//! At every commit end each checkout is compared with a cold build of its
//! folder in a store of its own: every call-graph primitive the commands
//! use, on a sample of symbols; text candidates for a sample of
//! identifiers; and the incremental engine itself, file by file: its inputs
//! (paths, parses, configuration), every file's answers, recorded reads and
//! fragment bytes, and [`LiveCheckout::stale_recordings`]. At a commit end
//! that publishes, A is compared before and after publishing, and a second
//! handle bound fresh over the same store is compared too. Runs from
//! `examples/checkout_gate.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aft_store::content::RecordKey;
use aft_store::ids::{Digest128, FamilyId};
use aft_store::text::{plan_pattern, Scope};

use super::checkout::{Checkout, CheckoutView, DataLayer, PlaneStatus, Planes};
use super::content::EXTRACTOR_VERSION;
use super::edit_gate::{describe_question, Files};
use super::incremental::LiveCheckout;
use super::runtime::{DataCheckout, Fresh};

/// One comparison of a checkout with a cold build of its folder.
#[derive(Debug, Default, serde::Serialize)]
pub struct CommitRecord {
    pub commit: usize,
    /// Which checkout: `a`, `a bound again` (a second handle bound fresh
    /// over the store), or `b`.
    pub checkout: String,
    /// What the checkout served: `changes laid over` its last publication,
    /// or `published` right after it published its changes.
    pub serving: String,
    pub edits: usize,
    /// What happened to the checkout since its last comparison.
    pub events: Vec<String>,
    pub record_ms_p50: f64,
    pub record_ms_max: f64,
    pub refresh_ms_p50: f64,
    pub refresh_ms_max: f64,
    pub cold_build_ms: f64,
    /// Call-graph primitives whose answers differ from the cold build's.
    pub differing_primitives: usize,
    /// Sampled identifiers for which one side's candidates miss a file
    /// that really contains the identifier: a wrong answer.
    pub missed_text: usize,
    /// Sampled identifiers whose candidate sets differ only in files that
    /// do not contain the identifier. Candidates are a superset the reader
    /// verifies against file contents, so this is not a wrong answer; it
    /// shows how far the two filters differ.
    pub differing_false_positives: usize,
    pub text_samples: usize,
    /// The incremental engine compared file by file with the cold build's;
    /// `None` when the checkout has no call graph to compare.
    pub engine: Option<EngineComparison>,
    /// The view's call graph was not ready.
    pub not_ready: bool,
    /// Why the checkout's last build or refresh failed, if it did.
    pub failure: Option<String>,
    /// After a restart bind: readers the stored input terms of the
    /// publication it bound miss, which an update would then never reach.
    pub stored_inputs_missing: Option<usize>,
    /// What the cold build shows of the probe folder that it should not:
    /// the probe's edits did not move what they must move, so the checks
    /// that rely on them prove nothing.
    pub probe_unexpected: Vec<String>,
    /// Segment files on disk in the family store.
    pub segment_files: usize,
    pub examples: Vec<String>,
}

impl CommitRecord {
    fn failed(&self) -> bool {
        self.differing_primitives > 0
            || self.missed_text > 0
            || self.not_ready
            || self.failure.is_some()
            || self
                .stored_inputs_missing
                .is_some_and(|missing| missing > 0)
            || !self.probe_unexpected.is_empty()
            || self.engine.as_ref().is_none_or(EngineComparison::failed)
    }
}

/// An incremental engine compared file by file with a cold build's engine.
#[derive(Debug, Default, serde::Serialize)]
pub struct EngineComparison {
    /// Files whose resolutions were compared. None compared is a failure:
    /// the comparison checked nothing.
    pub files: usize,
    /// Paths whose record (kind, content, link target, language), parse or
    /// configuration bytes differ from the cold build's, or that only one
    /// side has.
    pub differing_inputs: usize,
    /// Files whose rows, method-call sites or dispatch targets differ, or
    /// that only one side resolves.
    pub differing_answers: usize,
    /// Files whose recorded reads, method-call dependencies or dispatch
    /// traces differ.
    pub differing_recordings: usize,
    /// Files whose fragment, as a publication would store it, differs.
    pub differing_fragments: usize,
    /// Files whose recorded reads or surface answers no longer hold in the
    /// engine's own checkout: an update that should have resolved them
    /// again did not.
    pub stale_recordings: usize,
}

impl EngineComparison {
    fn failed(&self) -> bool {
        self.files == 0
            || self.differing_inputs > 0
            || self.differing_answers > 0
            || self.differing_recordings > 0
            || self.differing_fragments > 0
            || self.stale_recordings > 0
    }
}

/// A folder under the system's temporary folder, removed when dropped.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Result<Self, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "aft-checkout-gate-{}-{nanos}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).map_err(|error| error.to_string())?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let index = ((values.len() - 1) as f64 * p).round() as usize;
    values[index]
}

fn write_file(root: &Path, path: &str, text: &str) -> Result<(), String> {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    // A symlink at the path is replaced, not written through.
    if full.symlink_metadata().is_ok_and(|meta| meta.is_symlink()) {
        std::fs::remove_file(&full).map_err(|error| error.to_string())?;
    }
    std::fs::write(&full, text).map_err(|error| error.to_string())
}

/// Makes `path` a symlink to the folder `target` (relative to the link's
/// folder), replacing what was there.
fn write_link(root: &Path, path: &str, target: &str) -> Result<(), String> {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    if full.symlink_metadata().is_ok() {
        std::fs::remove_file(&full).map_err(|error| error.to_string())?;
    }
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, &full);
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(target, &full);
    made.map_err(|error| format!("{path} -> {target}: {error}"))
}

/// Whether a pack path is safe to write under the gate's folder.
fn safe(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.split('/').any(|part| part == ".." || part == ".git")
}

/// Identifiers to search for: every 50th symbol name of the parsed files,
/// at least four characters long, so each plans to at least one trigram.
fn text_samples(files: &Files) -> Vec<String> {
    let inputs = files.inputs();
    let mut names = BTreeSet::new();
    for (_, parse) in inputs.parses.values() {
        for symbol in &parse.symbols {
            if symbol.name.len() >= 4 && symbol.name.is_ascii() {
                names.insert(symbol.name.clone());
            }
        }
    }
    names.into_iter().step_by(50).collect()
}

/// Compares `view` with `cold` and fills the record's differences.
fn compare(
    root: &Path,
    view: &CheckoutView,
    cold: &CheckoutView,
    files: &Files,
    record: &mut CommitRecord,
) {
    let inputs = files.inputs();
    let mut queries = BTreeMap::new();
    let mut found = Vec::new();
    super::gate::compare_readers(
        &view.callgraph(),
        &cold.callgraph(),
        &inputs,
        &mut queries,
        &mut found,
    );
    record.differing_primitives = queries
        .values()
        .filter(|(_, differing, _, _)| *differing > 0)
        .count();
    record.examples.extend(found.into_iter().take(10));
    for failures in [view.callgraph_failures(), cold.callgraph_failures()] {
        record.differing_primitives += failures.len();
        record.examples.extend(failures.into_iter().take(5));
    }
    let samples = text_samples(files);
    record.text_samples = samples.len();
    for name in samples {
        let Ok(query) = plan_pattern(&regex::escape(&name), false) else {
            continue;
        };
        let (ours, theirs) = match (
            view.text_candidates(&query, Scope::All),
            cold.text_candidates(&query, Scope::All),
        ) {
            (Ok(ours), Ok(theirs)) => (
                ours.into_iter().collect::<BTreeSet<_>>(),
                theirs.into_iter().collect::<BTreeSet<_>>(),
            ),
            (ours, theirs) => {
                record.missed_text += 1;
                record.examples.push(format!(
                    "text {name}: {:?} vs {:?}",
                    ours.err(),
                    theirs.err()
                ));
                continue;
            }
        };
        if ours == theirs {
            continue;
        }
        // A file only one side lists is a miss on the other side when it
        // really contains the name.
        let contains = |path: &&Vec<u8>| {
            std::str::from_utf8(path)
                .ok()
                .and_then(|path| std::fs::read(root.join(path)).ok())
                .is_some_and(|bytes| {
                    bytes
                        .windows(name.len())
                        .any(|window| window == name.as_bytes())
                })
        };
        let missed = ours
            .symmetric_difference(&theirs)
            .filter(contains)
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect::<Vec<_>>();
        if missed.is_empty() {
            record.differing_false_positives += 1;
        } else {
            record.missed_text += 1;
            if record.examples.len() < 20 {
                record
                    .examples
                    .push(format!("text {name}: one side misses {missed:?}"));
            }
        }
    }
    record.not_ready = view.status().callgraph != PlaneStatus::Ready;
}

/// What an incremental engine resolves from: each path's record, each
/// parsed file's parse key, and each configuration file's bytes.
struct EngineInputs {
    records: BTreeMap<Vec<u8>, String>,
    parses: BTreeMap<String, String>,
    configs: BTreeMap<Vec<u8>, Digest128>,
}

impl EngineInputs {
    /// `live`'s inputs. With `decode`, each parse key is the one the
    /// engine's parse carries, which decodes every stored parse; without,
    /// it is the key the path's record names, as a publication derives it.
    fn of(live: &LiveCheckout, decode: bool) -> Self {
        let inputs = &live.inputs;
        let records = inputs
            .state
            .paths()
            .iter()
            .map(|record| {
                // A record whose content is a stand-in (a symlink, a file
                // too large to read) is described by what it stands for,
                // since the stand-in follows its stat.
                let what = if record.kind.has_content() {
                    format!("{:?} {} {:?}", record.kind, record.content, record.language)
                } else {
                    format!(
                        "{:?} -> {}",
                        record.kind,
                        String::from_utf8_lossy(&record.link)
                    )
                };
                (record.path.clone(), what)
            })
            .collect();
        let parses = if decode {
            inputs
                .parses
                .iter()
                .map(|(path, (key, _))| (path.clone(), key.clone()))
                .collect()
        } else {
            inputs
                .parses
                .keys()
                .map(|path| {
                    let key =
                        inputs
                            .state
                            .get(path.as_bytes())
                            .map_or_else(String::new, |record| {
                                RecordKey::new(record.content, &record.language, EXTRACTOR_VERSION)
                                    .digest()
                                    .to_string()
                            });
                    (path.clone(), key)
                })
                .collect()
        };
        let configs = inputs
            .configs
            .iter()
            .map(|(path, bytes)| (path.clone(), Digest128::of(bytes)))
            .collect();
        Self {
            records,
            parses,
            configs,
        }
    }
}

/// Keys of two maps whose values differ, or that only one map has.
fn differing_keys<'m, K: Ord, V: PartialEq>(
    ours: &'m BTreeMap<K, V>,
    theirs: &'m BTreeMap<K, V>,
) -> Vec<&'m K> {
    ours.keys()
        .chain(theirs.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|key| ours.get(*key) != theirs.get(*key))
        .collect()
}

/// Compares the engine `live` file by file with `cold`, an engine started
/// from a cold build's publication, noting examples. The cold side's
/// resolutions are decoded from the fragments that build stored.
fn compare_engine(
    live: &LiveCheckout,
    cold: &LiveCheckout,
    examples: &mut Vec<String>,
) -> EngineComparison {
    let mut comparison = EngineComparison::default();
    let note = |examples: &mut Vec<String>, line: String| {
        if examples.len() < 40 {
            examples.push(line);
        }
    };
    let ours = EngineInputs::of(live, true);
    let theirs = EngineInputs::of(cold, false);
    for path in differing_keys(&ours.records, &theirs.records) {
        comparison.differing_inputs += 1;
        note(
            examples,
            format!(
                "path {}: engine {:?}, cold {:?}",
                String::from_utf8_lossy(path),
                ours.records.get(path),
                theirs.records.get(path)
            ),
        );
    }
    for path in differing_keys(&ours.parses, &theirs.parses) {
        comparison.differing_inputs += 1;
        note(
            examples,
            format!(
                "parse {path}: engine {:?}, cold {:?}",
                ours.parses.get(path),
                theirs.parses.get(path)
            ),
        );
    }
    for path in differing_keys(&ours.configs, &theirs.configs) {
        comparison.differing_inputs += 1;
        note(
            examples,
            format!(
                "configuration {}: differs or only one side has it",
                String::from_utf8_lossy(path)
            ),
        );
    }
    // Every resolution either engine can read; one whose stored read failed
    // is left out, and so shows as resolved on one side only.
    let resolved = live.resolved.iter().collect::<BTreeMap<_, _>>();
    let cold_resolved = cold.resolved.iter().collect::<BTreeMap<_, _>>();
    let paths = resolved
        .keys()
        .chain(cold_resolved.keys())
        .map(|path| (*path).clone())
        .collect::<BTreeSet<_>>();
    for path in paths {
        let (Some(ours), Some(theirs)) = (
            resolved.get(&path).copied(),
            cold_resolved.get(&path).copied(),
        ) else {
            comparison.differing_answers += 1;
            note(examples, format!("{path}: resolved on one side only"));
            continue;
        };
        comparison.files += 1;
        let mut answers = Vec::new();
        if ours.rows != theirs.rows {
            answers.push("rows");
        }
        if ours.sites != theirs.sites {
            answers.push("method-call sites");
        }
        if ours.dispatch.as_ref().map(|dispatch| &dispatch.targets)
            != theirs.dispatch.as_ref().map(|dispatch| &dispatch.targets)
        {
            answers.push("dispatch targets");
        }
        if !answers.is_empty() {
            comparison.differing_answers += 1;
            note(examples, format!("{path}: {}", answers.join(", ")));
        }
        // The bound surface is computed when a file is bound, never stored,
        // so a resolution decoded from a fragment has none.
        let (our_binding, their_binding) = (
            ours.binding.without_surface(),
            theirs.binding.without_surface(),
        );
        let mut recordings = Vec::new();
        if our_binding != their_binding {
            recordings.push("recorded reads");
        }
        if ours.site_dependencies != theirs.site_dependencies {
            recordings.push("method-call dependencies");
        }
        if ours.dispatch != theirs.dispatch {
            recordings.push("dispatch trace");
        }
        if !recordings.is_empty() {
            comparison.differing_recordings += 1;
            note(examples, format!("{path}: {}", recordings.join(", ")));
            let reads = |binding: &crate::callgraph_store::join::ViewBindingDependencies| {
                binding
                    .answers()
                    .map(|(question, answer)| (describe_question(question), format!("{answer:?}")))
                    .collect::<BTreeMap<_, _>>()
            };
            let (our_reads, their_reads) = (reads(&our_binding), reads(&their_binding));
            for question in differing_keys(&our_reads, &their_reads).into_iter().take(4) {
                note(
                    examples,
                    format!(
                        "  read {question}: engine {:?}, cold {:?}",
                        our_reads.get(question),
                        their_reads.get(question)
                    ),
                );
            }
        }
        if super::fragment::encode(ours) != super::fragment::encode(theirs) {
            comparison.differing_fragments += 1;
        }
    }
    let stale = live.stale_recordings();
    comparison.stale_recordings = stale.len();
    for path in stale.iter().take(5) {
        note(
            examples,
            format!("{path}: a recorded read or surface answer no longer holds"),
        );
    }
    comparison
}

/// Compares `checkout` with the cold build `cold` of the same folder, as a
/// view (call-graph primitives and text candidates) and as an engine: an
/// engine started from the cold build's publication, dropped afterwards.
fn compare_checkout(
    checkout: &Checkout,
    cold: &Checkout,
    files: &Files,
    record: &mut CommitRecord,
) -> Result<(), String> {
    let view = checkout.view().ok_or("the checkout has no view")?;
    let cold_view = cold.view().ok_or("the cold checkout has no view")?;
    compare(checkout.root(), &view, &cold_view, files, record);
    drop((view, cold_view));
    let examples = &mut record.examples;
    let compared = cold
        .inspect_engine(|cold| checkout.inspect_engine(|live| compare_engine(live, cold, examples)))
        .map_err(|error| error.to_string())?
        .ok_or("the cold build published no call graph")?;
    record.engine = compared.map_err(|error| error.to_string())?;
    if record.engine.is_none() {
        record
            .examples
            .push("the checkout has no call graph published".to_string());
    }
    record.failure = checkout.failure();
    Ok(())
}

/// A cold build of the folder at `root` in a store of its own.
struct Cold {
    _store: Scratch,
    checkout: Checkout,
    build_ms: f64,
    /// What the build shows of the probe folder that it should not.
    probe_unexpected: Vec<String>,
}

impl Cold {
    /// Builds the folder at `root` cold, whose probe's ignore rule has
    /// moved when `ignore_moved`, and whose probe calls `other/lib.ts` when
    /// `calls_other`.
    fn build(
        layer: &Arc<DataLayer>,
        family: FamilyId,
        root: &Path,
        name: &str,
        (ignore_moved, calls_other): (bool, bool),
    ) -> Result<Self, String> {
        let store = Scratch::new(name)?;
        let started = Instant::now();
        let checkout = Checkout::bind(Arc::clone(layer), store.path(), family, root)
            .map_err(|error| error.to_string())?;
        checkout.build().map_err(|error| error.to_string())?;
        let build_ms = started.elapsed().as_secs_f64() * 1000.0;
        let probe_unexpected = checkout
            .inspect_engine(|cold| probe_unexpected(cold, ignore_moved, calls_other))
            .map_err(|error| error.to_string())?
            .ok_or("the cold build published no call graph")?;
        Ok(Self {
            _store: store,
            checkout,
            build_ms,
            probe_unexpected,
        })
    }

    fn compare(
        &self,
        checkout: &Checkout,
        files: &Files,
        record: &mut CommitRecord,
    ) -> Result<(), String> {
        record.cold_build_ms = self.build_ms;
        record.probe_unexpected.clone_from(&self.probe_unexpected);
        compare_checkout(checkout, &self.checkout, files, record)
    }
}

/// One change to a checkout's files.
#[derive(Clone, Debug)]
enum Edit {
    Write {
        path: String,
        text: String,
    },
    Delete {
        path: String,
    },
    /// The path becomes a symlink to the folder `target`.
    Link {
        path: String,
        target: String,
    },
}

impl Edit {
    fn write(path: &str, text: &str) -> Self {
        Self::Write {
            path: path.to_string(),
            text: text.to_string(),
        }
    }

    fn link(path: &str, target: &str) -> Self {
        Self::Link {
            path: path.to_string(),
            target: target.to_string(),
        }
    }

    fn path(&self) -> &str {
        match self {
            Self::Write { path, .. } | Self::Delete { path } | Self::Link { path, .. } => path,
        }
    }

    /// Makes the change on disk under `root`, and in `files`, the gate's
    /// own account of the files for sampling symbols and names.
    fn apply(&self, root: &Path, files: &mut Files) -> Result<(), String> {
        match self {
            Self::Write { path, text } => {
                write_file(root, path, text)?;
                files.set(path, text)?;
            }
            Self::Delete { path } => {
                let _ = std::fs::remove_file(root.join(path));
                files.remove(path);
            }
            Self::Link { path, target } => {
                write_link(root, path, target)?;
                files.remove(path);
            }
        }
        Ok(())
    }
}

/// The probe folder each checkout holds beside the pack's files.
const PROBE: &str = "gate_probe";

/// The probe folder of a checkout whose `alias` reaches `real` through the
/// symlink `<hops>/hop` (`hops_a` in A, `hops_b` in B). Both checkouts
/// resolve `main.ts` alike, through different paths, so a fragment keyed
/// only by its answers would be shared between them. Each symlink sits in a
/// folder of its own, so retargeting A's changes nothing B's chain reads,
/// not even a folder listing. Its `.gitignore` admits `docs/` and ignores
/// `gen/`, until the rule moves.
fn probe(hops: &str) -> Vec<Edit> {
    let at = |path: &str| format!("{PROBE}/{path}");
    vec![
        Edit::write(
            &at("main.ts"),
            "import { run } from './alias/lib';\nexport function main() { run(); }\n",
        ),
        Edit::write(&at("real/lib.ts"), "export function run() { return 1; }\n"),
        Edit::write(
            &at("other/lib.ts"),
            "export function other() { return 2; }\nexport function run() { return 2; }\n",
        ),
        Edit::link(&at("hops_a/hop"), "../real"),
        Edit::link(&at("hops_b/hop"), "../real"),
        Edit::link(&at("alias"), &format!("{hops}/hop")),
        Edit::write(&at(".gitignore"), "gen/\n"),
        Edit::write(
            &at("gen/out.ts"),
            "export function generated() { return 1; }\n",
        ),
        Edit::write(
            &at("docs/notes.ts"),
            "export function notes() { return 1; }\n",
        ),
        Edit::write(
            &at("uses.ts"),
            "import { generated } from './gen/out';\nimport { notes } from './docs/notes';\n\
             export function uses() { generated(); notes(); }\n",
        ),
    ]
}

/// The probe's ignore rule moves from `gen/` to `docs/`: `gen/out.ts`
/// becomes a member and `docs/notes.ts` stops being one, though neither
/// file changed.
fn probe_ignore_moves() -> Edit {
    Edit::write(&format!("{PROBE}/.gitignore"), "docs/\n")
}

/// A's symlink `hops_a/hop` moves to `other`: through A's chain, `main.ts`
/// now calls `other/lib.ts`. B's chain does not pass through it.
fn probe_retarget() -> Edit {
    Edit::link(&format!("{PROBE}/hops_a/hop"), "../other")
}

/// A probe file written while nothing records it, before a build.
fn probe_unwatched() -> Edit {
    Edit::write(
        &format!("{PROBE}/unwatched.ts"),
        "export function unwatched() { return 1; }\n",
    )
}

/// What a cold build of the probe folder must show, once its ignore rule
/// moved (`ignore_moved`) and once its `main.ts` calls `other/lib.ts`
/// (`calls_other`): which of its two ignorable files are members, and which
/// `lib.ts` `main.ts` calls. Lists every way it does not.
fn probe_unexpected(cold: &LiveCheckout, ignore_moved: bool, calls_other: bool) -> Vec<String> {
    let mut unexpected = Vec::new();
    let (member, ignored) = if ignore_moved {
        ("gen/out.ts", "docs/notes.ts")
    } else {
        ("docs/notes.ts", "gen/out.ts")
    };
    let has = |path: &str| {
        cold.inputs
            .state
            .get(format!("{PROBE}/{path}").as_bytes())
            .is_some()
    };
    if !has(member) {
        unexpected.push(format!("{PROBE}/{member} is not a member"));
    }
    if has(ignored) {
        unexpected.push(format!("{PROBE}/{ignored} is a member"));
    }
    let target = format!(
        "{PROBE}/{}/lib.ts",
        if calls_other { "other" } else { "real" }
    );
    let calls = cold
        .resolved
        .get(&format!("{PROBE}/main.ts"))
        .is_some_and(|main| {
            main.rows
                .iter()
                .any(|row| row.target_path.as_deref() == Some(target.as_bytes()))
        });
    if !calls {
        unexpected.push(format!("{PROBE}/main.ts does not call {target}"));
    }
    unexpected
}

/// Makes `root` a Git repository whose one commit is empty and the same in
/// every folder this is run on, so the runtime puts every such folder in
/// one family, as it does the checkouts of one repository.
fn git_repository(root: &Path) -> Result<(), String> {
    let git = |args: &[&str]| -> Result<(), String> {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "user.name=checkout gate",
                "-c",
                "user.email=gate@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .env("GIT_AUTHOR_DATE", "2000-01-01T00:00:00+0000")
            .env("GIT_COMMITTER_DATE", "2000-01-01T00:00:00+0000")
            .output()
            .map_err(|error| format!("git {args:?}: {error}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    };
    git(&["init", "-q"])?;
    git(&[
        "commit",
        "-q",
        "--allow-empty",
        "--no-verify",
        "-m",
        "checkout gate",
    ])
}

/// The family the runtime puts the checkout at `root` in: its
/// repository's root commits.
fn family_of(root: &Path) -> Result<FamilyId, String> {
    let identity = crate::search_index::canonical_git_root_commit_identity(root)
        .map_err(|error| format!("{}: {error}", root.display()))?;
    Ok(FamilyId::from_digest(Digest128::of(identity.as_bytes())))
}

/// How many segments a checkout's text and call graph may read from
/// ([`super::checkout::DeltaReport::segments`]) before the runtime builds it
/// again whole: the runtime's own threshold.
const COMPACT_AT: usize = 8;

/// How long the gate waits for the runtime's checkout to have a call graph
/// that covers every recorded change.
const RUNTIME_WAIT: Duration = Duration::from_secs(600);

/// Waits up to `wait` until the runtime's checkout serves a call graph
/// that covers every recorded change and runs no build. Returns why not.
fn settle(handle: &DataCheckout, wait: Duration) -> Result<(), String> {
    let deadline = Instant::now() + wait;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match handle.fresh_view(left) {
            Fresh::Ready(_) if !handle.checkout().status().building => return Ok(()),
            Fresh::Ready(_) => {}
            Fresh::Building => {
                return Err(format!(
                    "the call graph was not ready after {} s: {:?}",
                    wait.as_secs(),
                    handle.checkout().status()
                ))
            }
            Fresh::Failed(reason) => return Err(format!("the call graph failed: {reason}")),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "a build was still running after {} s",
                wait.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Builds `checkout` while another thread keeps recording `noise`, as a
/// watcher reporting paths during a build does. A recording that straddles
/// the build's publication must not put back the state it started from.
fn build_under_watcher(checkout: &Checkout, noise: &[Vec<u8>]) -> Result<usize, String> {
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let recorder = scope.spawn(|| {
            let mut recorded = 0;
            while !stop.load(Ordering::Acquire) {
                if checkout.record_paths(noise).is_ok() {
                    recorded += 1;
                }
            }
            recorded
        });
        let built = checkout.build().map_err(|error| error.to_string());
        stop.store(true, Ordering::Release);
        let recorded = recorder
            .join()
            .map_err(|_| "the recorder panicked".to_string())?;
        built.map(|_| recorded)
    })
}

/// The pack's commits, each as its edits in order, with the pack's base
/// files. `only`, when given, keeps the paths under those prefixes.
fn read_pack(
    pack: &Path,
    only: Option<&[String]>,
) -> Result<(Vec<(String, String)>, Vec<(u64, Vec<Edit>)>), String> {
    let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(
        std::fs::File::open(pack).map_err(|error| error.to_string())?,
    ));
    let kept = |path: &str| {
        safe(path)
            && only.is_none_or(|prefixes| prefixes.iter().any(|prefix| path.starts_with(prefix)))
    };
    let mut lines = reader.lines();
    let first: serde_json::Value = serde_json::from_str(
        &lines
            .next()
            .ok_or("empty pack")?
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let base = first["files"]
        .as_object()
        .ok_or("no base files")?
        .iter()
        .filter(|(path, _)| kept(path))
        .map(|(path, text)| (path.clone(), text.as_str().unwrap_or_default().to_string()))
        .collect();
    let mut commits: Vec<(u64, Vec<Edit>)> = Vec::new();
    for line in lines {
        let line = line.map_err(|error| error.to_string())?;
        let value: serde_json::Value =
            serde_json::from_str(&line).map_err(|error| error.to_string())?;
        if value["type"] != "edit" {
            continue;
        }
        let path = value["path"].as_str().ok_or("edit without path")?;
        if !kept(path) {
            continue;
        }
        let edit = if value["kind"] == "delete" {
            Edit::Delete {
                path: path.to_string(),
            }
        } else {
            Edit::write(path, value["text"].as_str().unwrap_or_default())
        };
        let commit = value["commit"].as_u64().unwrap_or_default();
        match commits.last_mut() {
            Some((last, edits)) if *last == commit => edits.push(edit),
            _ => commits.push((commit, vec![edit])),
        }
    }
    Ok((base, commits))
}

/// The segment files in the family store at `family_dir`.
fn segment_files(family_dir: &Path) -> usize {
    std::fs::read_dir(family_dir.join("segments"))
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or_default()
}

/// Memory samples for finding what the gate's process holds, written to
/// stderr as `memory {json}` lines when `CHECKOUT_GATE_MEMORY_EVERY` is set
/// to N: after every N of A's edits, and after each build, publication,
/// cold build and comparison. Each sample has the process's resident and
/// peak memory and thread count, what the allocator holds, the block
/// cache, the snapshots the family's readers lease, and what each named
/// checkout holds ([`Checkout::memory`]). Reading them takes no lock a
/// checkout waits on.
struct MemoryLog {
    every: usize,
    edits: std::cell::Cell<usize>,
}

impl MemoryLog {
    fn from_env() -> Self {
        Self {
            every: std::env::var("CHECKOUT_GATE_MEMORY_EVERY")
                .ok()
                .and_then(|every| every.parse().ok())
                .unwrap_or(0),
            edits: std::cell::Cell::new(0),
        }
    }

    /// Whether this edit of A's is one to sample after: every `every`th.
    fn due(&self) -> bool {
        if self.every == 0 {
            return false;
        }
        self.edits.set(self.edits.get() + 1);
        self.edits.get() % self.every == 0
    }

    fn sample(
        &self,
        label: &str,
        commit: usize,
        checkouts: &[(&str, &Checkout)],
        layer: &DataLayer,
        family_dir: &Path,
    ) {
        if self.every == 0 {
            return;
        }
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let field = |name: &str| {
            status
                .lines()
                .find_map(|line| line.strip_prefix(name))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|value| value.parse::<u64>().ok())
        };
        let mib = |bytes: u64| bytes / (1 << 20);
        let leases = std::fs::read_dir(family_dir.join("leases"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_name().to_string_lossy().ends_with(".lease"))
                    .count()
            })
            .unwrap_or_default();
        let parts = checkouts
            .iter()
            .map(|(name, checkout)| {
                let memory = checkout.memory(&mut aft_store::heap::Seen::new());
                let live = memory.live.map(|live| {
                    serde_json::json!({
                        "state_paths": live.state_paths,
                        "shared_kib": live.shared_bytes / 1024,
                        "text_paths": live.text_overlay_paths,
                        "text_kib": live.text_overlay_bytes / 1024,
                        "changes": live.changes,
                        "pending": live.pending_callgraph_paths,
                        "refreshed": live.refreshed_files,
                        "dead_code_files": live.dead_code_files,
                    })
                });
                (
                    (*name).to_string(),
                    serde_json::json!({"live": live, "engine": format!("{:?}", memory.engine)}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let (malloc_in_use, malloc_held) = allocator_bytes();
        let cache = layer.cache_stats();
        eprintln!(
            "memory {}",
            serde_json::json!({
                "label": label,
                "commit": commit,
                "rss_mib": field("VmRSS:").map(|kib| kib / 1024),
                "peak_rss_mib": field("VmHWM:").map(|kib| kib / 1024),
                "threads": field("Threads:"),
                "malloc_in_use_mib": malloc_in_use.map(mib),
                "malloc_held_mib": malloc_held.map(mib),
                "cache_mib": cache.resident_bytes / (1 << 20),
                "leases": leases,
                "segments": segment_files(family_dir),
                "checkouts": parts,
            })
        );
    }
}

/// Hands the pages the allocator holds free back to the system, as AFT's
/// own allocator relief (`crate::memory`) does in a running process once it
/// holds enough of them. Between comparisons the gate frees whole cold
/// builds, engines and resolver thread pools; glibc keeps the pages they
/// leave free in its per-thread arenas, so without this the gate's resident
/// memory grows with every comparison while what it uses does not.
fn release_free_memory() {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let _ = crate::memory::relieve_allocator_pressure();
}

/// What glibc's allocator hands out now, and what it holds from the system
/// (`mallinfo2`: in use, and arenas with large mappings); `None` elsewhere
/// or before glibc 2.33.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn allocator_bytes() -> (Option<u64>, Option<u64>) {
    type Mallinfo2 = unsafe extern "C" fn() -> libc::mallinfo2;
    // SAFETY: looking a symbol up by name has no other effect.
    let symbol = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"mallinfo2".as_ptr()) };
    if symbol.is_null() {
        return (None, None);
    }
    // SAFETY: glibc declares mallinfo2 as `struct mallinfo2 (*)(void)`.
    let mallinfo2 = unsafe { std::mem::transmute::<*mut libc::c_void, Mallinfo2>(symbol) };
    // SAFETY: mallinfo2 only reads the allocator's counters.
    let info = unsafe { mallinfo2() };
    let mapped = info.hblkhd as u64;
    (
        Some((info.uordblks as u64).saturating_add(mapped)),
        Some((info.arena as u64).saturating_add(mapped)),
    )
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn allocator_bytes() -> (Option<u64>, Option<u64>) {
    (None, None)
}

/// The checkout in `slot`, which is empty only while it is down.
fn live(slot: &Option<Checkout>) -> Result<&Checkout, String> {
    slot.as_ref()
        .ok_or_else(|| "the checkout is down".to_string())
}

/// Restarts A over a store whose newest copies of the fragments A and B
/// share are B's. A is built, so its publication covers every file on disk;
/// B is built after it; then A goes down while only the probe's link moves,
/// and binds again. The first update after the bind is that link's alone,
/// so only the stored terms of the fragment copies A's reader picked can
/// send the probe's `main.ts` to be resolved again. Returns the record for
/// the comparison that follows, with the stored-input check's misses.
fn restart_over_shared_fragments(
    a: &mut Option<Checkout>,
    b: Option<&DataCheckout>,
    b_wait: Duration,
    bind: &dyn Fn(&Path) -> Result<Checkout, String>,
    (a_root, a_files): (&Path, &mut Files),
    b_events: &mut Vec<String>,
) -> Result<CommitRecord, String> {
    let mut events = Vec::new();
    live(a)?.build().map_err(|error| error.to_string())?;
    events.push("built again, so its publication covers every file".to_string());
    if let Some(handle) = b {
        if let Err(problem) = settle(handle, b_wait) {
            b_events.push(format!("before building again: {problem}"));
        }
        handle
            .checkout()
            .build()
            .map_err(|error| error.to_string())?;
        b_events.push(
            "built again after A, so the newest copy of every fragment they share is B's"
                .to_string(),
        );
    }
    *a = None;
    let retarget = probe_retarget();
    retarget.apply(a_root, a_files)?;
    *a = Some(bind(a_root)?);
    let missing = live(a)?
        .check_stored_inputs()
        .map_err(|error| error.to_string())?;
    let started = Instant::now();
    live(a)?
        .refresh_callgraph()
        .map_err(|error| error.to_string())?;
    events.push(format!(
        "restarted with only {} retargeted while down",
        retarget.path()
    ));
    Ok(CommitRecord {
        checkout: "a".to_string(),
        serving: "restarted over B's newer fragments".to_string(),
        events,
        refresh_ms_max: started.elapsed().as_secs_f64() * 1000.0,
        stored_inputs_missing: Some(missing.len()),
        examples: missing.into_iter().take(5).collect(),
        ..CommitRecord::default()
    })
}

/// What a run of the gate found.
#[derive(Debug, Default, serde::Serialize)]
pub struct Summary {
    /// Comparisons with a cold build, and how many differed.
    pub comparisons: usize,
    pub failed: usize,
    /// Times A's changes were published on their own.
    pub publications: usize,
    /// Builds that folded A's segments once it read from more than eight.
    pub compactions: usize,
    /// The most segments A read from after a publication of its changes.
    pub most_segments: usize,
}

impl Summary {
    fn emit(&mut self, out: &mut dyn std::io::Write, record: &CommitRecord) -> Result<(), String> {
        self.comparisons += 1;
        if record.failed() {
            self.failed += 1;
        }
        writeln!(
            out,
            "{}",
            serde_json::to_string(record).map_err(|error| error.to_string())?
        )
        .map_err(|error| error.to_string())
    }

    /// Publishes `checkout`'s changes on their own, as the runtime does when
    /// a checkout goes quiet, and builds it again whole once it reads from
    /// more than [`COMPACT_AT`] segments, as the runtime then does. A
    /// watcher keeps recording `noise` during that build.
    fn publish(
        &mut self,
        checkout: &Checkout,
        noise: &[Vec<u8>],
        events: &mut Vec<String>,
    ) -> Result<bool, String> {
        let Some(report) = checkout
            .publish_changes()
            .map_err(|error| error.to_string())?
        else {
            return Ok(false);
        };
        self.publications += 1;
        self.most_segments = self.most_segments.max(report.segments);
        events.push(format!(
            "published {} changed paths ({} fragments, {} records, {} text units); {} segments",
            report.paths, report.fragments, report.records, report.text_units, report.segments
        ));
        if report.segments > COMPACT_AT {
            let recorded = build_under_watcher(checkout, noise)?;
            self.compactions += 1;
            events.push(format!(
                "built again whole past {COMPACT_AT} segments, while a watcher recorded {recorded} times"
            ));
        }
        Ok(true)
    }
}

/// Replays the pack at `pack` through checkouts A and B under a temporary
/// folder, with `threads` resolver threads, as the module documentation
/// describes. Writes one JSON line per comparison to `out`.
///
/// `CHECKOUT_GATE_ONLY` (comma-separated path prefixes) keeps only the
/// pack's paths under them, for a shorter run; `CHECKOUT_GATE_PUBLISH_EDITS`
/// sets how many of A's edits pass between publications of its changes
/// within a commit (8 by default; 0 publishes at commit ends only);
/// `CHECKOUT_GATE_MEMORY_EVERY=N` writes memory samples to stderr (see
/// `MemoryLog`).
///
/// # Errors
///
/// When the pack cannot be read, the folders cannot be set up, or a
/// checkout fails.
#[allow(clippy::too_many_lines)]
pub fn run(pack: &Path, threads: usize, out: &mut dyn std::io::Write) -> Result<Summary, String> {
    let only = std::env::var("CHECKOUT_GATE_ONLY").ok().map(|prefixes| {
        prefixes
            .split(',')
            .filter(|prefix| !prefix.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>()
    });
    let publish_every = std::env::var("CHECKOUT_GATE_PUBLISH_EDITS")
        .ok()
        .map_or(Ok(8), |every| every.parse::<usize>())
        .map_err(|error| format!("CHECKOUT_GATE_PUBLISH_EDITS: {error}"))?;
    let (base, mut commits) = read_pack(pack, only.as_deref())?;
    let count = commits.len();
    if count < 6 {
        return Err(format!(
            "the pack has {count} commits with edits; the gate needs at least six"
        ));
    }
    let rebuild_at = count / 3;
    let restart_at = 2 * count / 3;
    let b_restart_at = (5 * count / 6).max(restart_at + 1).min(count - 1);
    let lag = (count / 4).max(1);
    commits[1].1.push(probe_ignore_moves());

    let work = Scratch::new("work")?;
    let mut roots = Vec::new();
    let mut files = Vec::new();
    for (name, hops) in [("a", "hops_a"), ("b", "hops_b")] {
        let root = work.path().join(name);
        std::fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        let root = std::fs::canonicalize(&root).map_err(|error| error.to_string())?;
        git_repository(&root)?;
        let mut checkout_files = Files::default();
        for (path, text) in &base {
            write_file(&root, path, text)?;
            checkout_files.set(path, text)?;
        }
        for edit in probe(hops) {
            edit.apply(&root, &mut checkout_files)?;
        }
        roots.push(root);
        files.push(checkout_files);
    }
    let mut b_files = files.pop().ok_or("B's files were not set up")?;
    let mut a_files = files.pop().ok_or("A's files were not set up")?;
    let (a_root, b_root) = (roots[0].clone(), roots[1].clone());
    let family = family_of(&a_root)?;
    if family_of(&b_root)? != family {
        return Err("the two folders are not checkouts of one repository".to_string());
    }
    let store = Scratch::new("store")?;
    let family_dir = store.path().join("data").join(family.digest().to_string());
    let layer = Arc::new(DataLayer::new(256 << 20, threads));
    let bind = |root: &Path| {
        Checkout::bind(Arc::clone(&layer), &family_dir, family, root)
            .map_err(|error| error.to_string())
    };
    let noise = vec![format!("{PROBE}/main.ts").into_bytes()];
    let mut summary = Summary::default();
    // A: bound and built as a first open builds it.
    let mut a = Some(bind(&a_root)?);
    live(&a)?.build().map_err(|error| error.to_string())?;
    let missing = live(&a)?
        .check_stored_inputs()
        .map_err(|error| error.to_string())?;
    if !missing.is_empty() {
        return Err(format!("the stored input terms miss readers: {missing:#?}"));
    }

    // B: a process published its text index and stopped before the call
    // graph; the runtime opens that.
    if !bind(&b_root)?
        .publish_text_first()
        .map_err(|error| error.to_string())?
    {
        return Err("B's text-only publication was superseded".to_string());
    }
    let mut b = Some(
        DataCheckout::open_planes(&b_root, store.path(), None, Planes::ALL)
            .map_err(|error| error.to_string())?,
    );
    let mut b_events =
        vec!["opened by the runtime over a publication of its text index alone".to_string()];
    let mut b_wait = RUNTIME_WAIT;
    let memory = MemoryLog::from_env();
    // Samples memory with A and B, and any other checkout named, as they
    // are now.
    macro_rules! sample {
        ($label:expr, $commit:expr $(, $name:expr => $checkout:expr)*) => {{
            let mut parts: Vec<(&str, &Checkout)> = Vec::new();
            if let Some(checkout) = a.as_ref() {
                parts.push(("a", checkout));
            }
            if let Some(handle) = b.as_deref() {
                parts.push(("b", handle.checkout()));
            }
            $(parts.push(($name, $checkout));)*
            memory.sample($label, $commit, &parts, &layer, &family_dir);
        }};
    }
    sample!("a built, b opened", 0);

    for (index, (commit, edits)) in commits.iter().enumerate() {
        let commit = usize::try_from(*commit).unwrap_or_default();
        let mut events = Vec::new();
        if index == rebuild_at {
            probe_unwatched().apply(&a_root, &mut a_files)?;
            let recorded = build_under_watcher(live(&a)?, &noise)?;
            events.push(format!(
                "built again while a watcher recorded {recorded} times, with {PROBE}/unwatched.ts changed and never recorded"
            ));
            sample!("a built again", commit);
        }
        let restart = index == restart_at;
        let mut stored_inputs_missing = None;
        if restart {
            // A restarted process: the old handle goes away, the commit's
            // edits land while nothing watches, then the checkout binds
            // again and must find them on its own.
            a = None;
            events.push("restart".to_string());
        }
        let mut record_ms = Vec::new();
        let mut refresh_ms = Vec::new();
        let mut since_publication = 0;
        for edit in edits {
            edit.apply(&a_root, &mut a_files)?;
            if restart {
                continue;
            }
            let checkout = live(&a)?;
            let started = Instant::now();
            checkout
                .record_paths(&[edit.path().as_bytes().to_vec()])
                .map_err(|error| error.to_string())?;
            record_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            let started = Instant::now();
            checkout
                .refresh_callgraph()
                .map_err(|error| error.to_string())?;
            refresh_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            since_publication += 1;
            if publish_every > 0 && since_publication >= publish_every {
                since_publication = 0;
                if summary.publish(checkout, &noise, &mut events)? {
                    release_free_memory();
                    sample!("a published", commit);
                }
            }
            if memory.due() {
                sample!("a edited", commit);
            }
        }
        if restart {
            a = Some(bind(&a_root)?);
            let missing = live(&a)?
                .check_stored_inputs()
                .map_err(|error| error.to_string())?;
            stored_inputs_missing = Some(missing);
            let started = Instant::now();
            live(&a)?
                .refresh_callgraph()
                .map_err(|error| error.to_string())?;
            refresh_ms.push(started.elapsed().as_secs_f64() * 1000.0);
            events.push(format!("{} paths changed while down", edits.len()));
            sample!("a restarted", commit);
        }

        // The probe's ignore rule moves in A's second commit, and the symlink
        // A's chain passes through is retargeted after A's restart commit.
        let cold = Cold::build(
            &layer,
            family,
            &a_root,
            &format!("cold-a-{commit}"),
            (index >= 1, index > restart_at),
        )?;
        sample!("cold a built", commit, "cold" => &cold.checkout);
        let mut record = CommitRecord {
            commit,
            checkout: "a".to_string(),
            serving: "changes laid over".to_string(),
            edits: edits.len(),
            events,
            record_ms_p50: percentile(&mut record_ms, 0.5),
            record_ms_max: record_ms.iter().copied().fold(0.0, f64::max),
            refresh_ms_p50: percentile(&mut refresh_ms, 0.5),
            refresh_ms_max: refresh_ms.iter().copied().fold(0.0, f64::max),
            ..CommitRecord::default()
        };
        if let Some(missing) = stored_inputs_missing {
            record.stored_inputs_missing = Some(missing.len());
            record.examples.extend(missing.into_iter().take(5));
        }
        cold.compare(live(&a)?, &a_files, &mut record)?;
        record.segment_files = segment_files(&family_dir);
        summary.emit(out, &record)?;
        sample!("a compared", commit, "cold" => &cold.checkout);
        if index % 3 == 2 {
            let mut events = Vec::new();
            if !summary.publish(live(&a)?, &noise, &mut events)? {
                events.push("nothing to publish".to_string());
            }
            let mut record = CommitRecord {
                commit,
                checkout: "a".to_string(),
                serving: "published".to_string(),
                events,
                ..CommitRecord::default()
            };
            cold.compare(live(&a)?, &a_files, &mut record)?;
            record.segment_files = segment_files(&family_dir);
            summary.emit(out, &record)?;
            // A second handle bound fresh over the store, as the next
            // process binds it.
            let fresh = bind(&a_root)?;
            let started = Instant::now();
            fresh
                .refresh_callgraph()
                .map_err(|error| error.to_string())?;
            let mut record = CommitRecord {
                commit,
                checkout: "a bound again".to_string(),
                serving: "published".to_string(),
                refresh_ms_max: started.elapsed().as_secs_f64() * 1000.0,
                ..CommitRecord::default()
            };
            cold.compare(&fresh, &a_files, &mut record)?;
            record.segment_files = segment_files(&family_dir);
            summary.emit(out, &record)?;
            sample!("a published and bound again, compared", commit, "cold" => &cold.checkout, "a bound again" => &fresh);
        }
        drop(cold);
        release_free_memory();
        sample!("cold a dropped", commit);
        if restart {
            let record = restart_over_shared_fragments(
                &mut a,
                b.as_deref(),
                b_wait,
                &bind,
                (&a_root, &mut a_files),
                &mut b_events,
            )?;
            let mut record = CommitRecord { commit, ..record };
            let cold = Cold::build(
                &layer,
                family,
                &a_root,
                &format!("cold-a-{commit}-shared"),
                (true, true),
            )?;
            cold.compare(live(&a)?, &a_files, &mut record)?;
            record.segment_files = segment_files(&family_dir);
            summary.emit(out, &record)?;
            sample!("a restarted over b, compared", commit, "cold" => &cold.checkout);
            drop(cold);
            release_free_memory();
        }

        // B takes the commit `lag` before A's, as the watcher reports it.
        if let Some((b_commit, b_edits)) = index.checked_sub(lag).map(|at| &commits[at]) {
            let b_restart = index == b_restart_at;
            if b_restart {
                if let Some(handle) = &b {
                    if let Err(problem) = settle(handle, b_wait) {
                        b_events.push(format!("before the restart: {problem}"));
                    }
                }
                b = None;
            }
            for edit in b_edits {
                edit.apply(&b_root, &mut b_files)?;
                if let Some(handle) = &b {
                    handle.note_paths(&[b_root.join(edit.path())]);
                }
            }
            if b_restart {
                b = Some(
                    DataCheckout::open_planes(&b_root, store.path(), None, Planes::ALL)
                        .map_err(|error| error.to_string())?,
                );
                b_events.push(format!(
                    "restarted: the runtime opened it again after {} paths changed while down",
                    b_edits.len()
                ));
            }
            b_events.push(format!("took pack commit {b_commit}"));
        }
        let handle = b.as_ref().ok_or("B is down")?;
        let problem = settle(handle, b_wait).err();
        if problem.is_some() && !handle.checkout().status().callgraph_published {
            // A checkout whose call graph was never built will not get one
            // by waiting longer; later comparisons fail without the wait.
            b_wait = Duration::from_secs(5);
        }
        sample!("b settled", commit);
        // B's ignore rule moves once B takes the second commit. B's chain
        // does not pass through the symlink retargeted at A's restart, so its
        // `main.ts` keeps calling `real/lib.ts`.
        let b_taken = index.checked_sub(lag);
        let cold = Cold::build(
            &layer,
            family,
            &b_root,
            &format!("cold-b-{commit}"),
            (b_taken.is_some_and(|taken| taken >= 1), false),
        )?;
        let mut record = CommitRecord {
            commit,
            checkout: "b".to_string(),
            serving: "as the runtime left it".to_string(),
            events: std::mem::take(&mut b_events),
            ..CommitRecord::default()
        };
        if let Some(problem) = problem {
            record.not_ready = true;
            record.examples.push(problem);
        }
        cold.compare(handle.checkout(), &b_files, &mut record)?;
        record.segment_files = segment_files(&family_dir);
        summary.emit(out, &record)?;
        sample!("b compared", commit, "cold" => &cold.checkout);
        drop(cold);
        release_free_memory();
    }
    if summary.comparisons == 0 {
        return Err("nothing was compared".to_string());
    }
    Ok(summary)
}
