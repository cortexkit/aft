//! What kind of content a file holds, decided once for every tool that asks.
//!
//! grep prints the matching lines of source, docs, config and tests, and only
//! counts the lines of lockfiles, generated and minified files, logs and
//! captured command output, and JSON data files. inspect needs to know which
//! files are generated, and aft_search ranks some kinds below source. This
//! module is the one place that names those kinds.
//!
//! The name decides first, with no I/O: lockfile names, minified and
//! source-map suffixes, generated path shapes, log and capture names. Only when
//! the name does not decide, and only for extensions where content can change
//! the answer, does the classifier look at the start of the file: at most
//! [`HEAD_BYTES`], or [`JSON_HEAD_BYTES`] for JSON documents, whose long-line
//! test needs more. A file is never read past that bound, however long its
//! first line is.
//!
//! Whether a file is a test or test support (fixtures, mocks, snapshots) is a
//! separate flag, [`TestRole`], decided by the same path predicates inspect and
//! aft_search already use. A captured `stderr` under `tests/fixtures/` is
//! command output and test support at once.

use std::collections::HashMap;
use std::io::Read;
#[cfg(windows)]
use std::path::PathBuf;
use std::path::{Component, Path};
use std::time::SystemTime;

/// Bytes read from the start of a file for the generated-header and minified
/// checks.
pub(crate) const HEAD_BYTES: usize = 8 * 1024;

/// Bytes read from the start of a JSON document for the long-line test.
pub(crate) const JSON_HEAD_BYTES: usize = 64 * 1024;

/// Lines at the top of a file searched for a generated-code header.
pub(crate) const GENERATED_HEADER_LINES: usize = 5;

/// Shortest JSON line, in bytes, taken as a sign that the file holds data
/// (a captured file body, a serialized list) rather than hand-edited
/// settings.
pub(crate) const DATA_JSON_LONG_LINE_BYTES: usize = 256;

/// Smallest head that can be judged minified by line density.
const MINIFIED_MIN_HEAD_BYTES: usize = 2 * 1024;

/// What a file's content is, for deciding how search output treats it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ContentKind {
    /// Source code, docs and configuration: what a reader searches for.
    Text,
    /// A package manager's lockfile (`bun.lock`, `Cargo.lock`, `*.lock`).
    Lockfile,
    /// Generated code: a generated path shape, a source map, or a
    /// generated-code header.
    Generated,
    /// A minified bundle, by name (`*.min.js`) or by line density.
    Minified,
    /// A log or captured command output (`*.log`, `stdout`, `*.stderr.txt`,
    /// line-delimited JSON records).
    Capture,
    /// A JSON document holding data rather than configuration.
    DataJson,
}

impl ContentKind {
    /// True for every kind grep counts instead of printing by default.
    pub fn is_counted(self) -> bool {
        self != Self::Text
    }

    /// The kind as grep names it in one entry (`3 lines (lockfile)`).
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Lockfile => "lockfile",
            Self::Generated => "generated",
            Self::Minified => "minified",
            Self::Capture => "log or command output",
            Self::DataJson => "JSON data",
        }
    }

    /// The kind as grep names several files of it (`+3 more lockfiles`).
    pub fn plural(self) -> &'static str {
        match self {
            Self::Text => "text files",
            Self::Lockfile => "lockfiles",
            Self::Generated => "generated files",
            Self::Minified => "minified files",
            Self::Capture => "log or command-output files",
            Self::DataJson => "JSON data files",
        }
    }
}

/// Whether a file is an automated test, test support, or neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TestRole {
    None,
    /// An automated-test file (`*.test.ts`, `tests/`, `*_test.go`, ...).
    Test,
    /// Test support: fixtures, mocks, snapshots, corpora.
    Support,
}

/// A file's content kind, its test role, and what decided the kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FileKind {
    pub content: ContentKind,
    pub test: TestRole,
    /// The file name or name part that decided `content`: a lockfile name
    /// (`bun.lock`), a suffix (`.log`, `.json`, `.min.js`, `.lock`), or a name
    /// segment (`stdout`, `stderr`). `None` when the path shape or the
    /// content decided it. An `include` glob that names the marker asks for
    /// files of this kind.
    pub marker: Option<&'static str>,
}

impl FileKind {
    /// Whether the marker is a suffix of the file name (`.log`) rather than
    /// a whole name (`bun.lock`) or a name segment (`stdout`).
    pub fn marker_is_suffix(&self) -> bool {
        self.marker.is_some_and(|marker| marker.starts_with('.'))
    }
}

/// Lockfiles by exact file name, compared ASCII case-insensitively.
const LOCKFILE_NAMES: &[&str] = &[
    "bun.lock",
    "bun.lockb",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "deno.lock",
    "Cargo.lock",
    "go.sum",
    "go.work.sum",
    "poetry.lock",
    "uv.lock",
    "Pipfile.lock",
    "pdm.lock",
    "pixi.lock",
    "conda-lock.yml",
    "Gemfile.lock",
    "gems.locked",
    "composer.lock",
    "mix.lock",
    "pubspec.lock",
    "Podfile.lock",
    "Package.resolved",
    "Cartfile.resolved",
    "packages.lock.json",
    "paket.lock",
    "flake.lock",
    "gradle.lockfile",
    ".terraform.lock.hcl",
    "renv.lock",
    "cabal.project.freeze",
    "stack.yaml.lock",
    "shard.lock",
    "rebar.lock",
    "MODULE.bazel.lock",
    "conan.lock",
    "Chart.lock",
    "jsonnetfile.lock.json",
];

const MINIFIED_SUFFIXES: &[&str] = &[".min.js", ".min.mjs", ".min.cjs", ".min.css"];

/// Source maps of compiled output. A bare `*.map` is not enough: the
/// extension is used for unrelated files too.
const SOURCE_MAP_SUFFIXES: &[&str] = &[".js.map", ".mjs.map", ".cjs.map", ".css.map", ".ts.map"];

/// Extensions of logs and line-delimited records. `.jsonl` and `.ndjson`
/// belong here rather than with JSON data: one record per line is the shape
/// of a log. `.out` and `.err` alone are not captures, because test suites
/// keep expected outputs under those names.
const CAPTURE_EXTENSIONS: &[(&str, &str)] = &[
    ("log", ".log"),
    ("jsonl", ".jsonl"),
    ("ndjson", ".ndjson"),
    ("stdout", ".stdout"),
    ("stderr", ".stderr"),
];

/// Extensions a `stdout`/`stderr` name segment may end in and still be a
/// capture (`stderr.txt`, `bash-1.stdout.txt`, `run.stdout.out`).
const CAPTURE_SEGMENT_EXTENSIONS: &[&str] = &["txt", "log", "out"];

const JSON_DATA_EXTENSIONS: &[(&str, &str)] = &[("json", ".json"), ("geojson", ".geojson")];

const MINIFIED_CANDIDATE_EXTENSIONS: &[&str] = &["js", "mjs", "cjs", "css"];

/// Name the kind of `path` with no cache. See [`FileKindCache::classify`].
pub fn classify(project_root: &Path, path: &Path, held_bytes: Option<&[u8]>) -> FileKind {
    FileKindCache::new().classify(project_root, path, held_bytes)
}

/// Kinds of files already examined in one request, so a file seen several
/// times is read at most once. Entries are keyed by path, length and
/// modification time, so a file rewritten mid-request is examined again.
#[derive(Debug, Default)]
pub struct FileKindCache {
    entries: HashMap<(std::path::PathBuf, u64, Option<SystemTime>), FileKind>,
}

impl FileKindCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Name the kind of `path`. Only the part of `path` below
    /// `project_root` decides path shapes and the test role; a relative
    /// `path` is taken as already relative to the root.
    ///
    /// `held_bytes`, when the caller already holds the file's bytes, replaces
    /// the bounded read; only their first [`HEAD_BYTES`] (or
    /// [`JSON_HEAD_BYTES`]) are looked at.
    pub fn classify(
        &mut self,
        project_root: &Path,
        path: &Path,
        held_bytes: Option<&[u8]>,
    ) -> FileKind {
        let relative = relative_within(project_root, path);
        let test = test_role(relative.as_deref().unwrap_or(path));
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some((content, marker)) = classify_by_name(relative.as_deref(), &file_name) {
            return FileKind {
                content,
                test,
                marker,
            };
        }
        let checks = ContentChecks::for_name(&file_name);
        if !checks.any() {
            return FileKind {
                content: ContentKind::Text,
                test,
                marker: None,
            };
        }

        let (content, marker) = if let Some(bytes) = held_bytes {
            checks.decide(&file_name, bytes)
        } else {
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                project_root.join(path)
            };
            let metadata = std::fs::metadata(&absolute).ok();
            let key = (
                absolute.clone(),
                metadata.as_ref().map(|meta| meta.len()).unwrap_or(0),
                metadata.as_ref().and_then(|meta| meta.modified().ok()),
            );
            if let Some(kind) = self.entries.get(&key) {
                return FileKind { test, ..*kind };
            }
            let head = read_head(&absolute, checks.head_limit()).unwrap_or_default();
            let decided = checks.decide(&file_name, &head);
            self.entries.insert(
                key,
                FileKind {
                    content: decided.0,
                    test,
                    marker: decided.1,
                },
            );
            decided
        };
        FileKind {
            content,
            test,
            marker,
        }
    }
}

/// The kind a file name alone decides (lockfile names, minified and
/// source-map suffixes, generated path shapes, log and capture names), or
/// `None` when only content can tell. `relative` is the project-relative path, or
/// `None` for a file outside the project (or one whose spelling could not be
/// matched to the root), whose folders never decide a kind: its generated
/// shape is then judged on the file name alone (`api_pb.ts`,
/// `types.generated.ts`).
fn classify_by_name(
    relative: Option<&Path>,
    file_name: &str,
) -> Option<(ContentKind, Option<&'static str>)> {
    let lower = file_name.to_ascii_lowercase();
    let extension = extension_of(&lower);

    // 1. Lockfiles.
    if let Some(name) = LOCKFILE_NAMES
        .iter()
        .find(|name| name.eq_ignore_ascii_case(file_name))
    {
        return Some((ContentKind::Lockfile, Some(*name)));
    }
    if extension == "lock" {
        return Some((ContentKind::Lockfile, Some(".lock")));
    }

    // 2. Minified bundles by name.
    if let Some(suffix) = MINIFIED_SUFFIXES
        .iter()
        .find(|suffix| lower.ends_with(*suffix))
    {
        return Some((ContentKind::Minified, Some(*suffix)));
    }

    // 3. Source maps, then the generated path shapes.
    if let Some(suffix) = SOURCE_MAP_SUFFIXES
        .iter()
        .find(|suffix| lower.ends_with(*suffix))
    {
        return Some((ContentKind::Generated, Some(*suffix)));
    }
    if path_has_generated_shape(relative.unwrap_or(Path::new(file_name))) {
        return Some((ContentKind::Generated, None));
    }

    // 4. Logs and command output, by name only. A folder name never decides
    // (`logs/README.md`, `log/contract.mjs` stay text), and neither does a
    // source extension (`log.rs`, `stdout.rs`, `foo.log.ts`).
    if let Some((_, marker)) = CAPTURE_EXTENSIONS
        .iter()
        .find(|(candidate, _)| *candidate == extension)
    {
        return Some((ContentKind::Capture, Some(*marker)));
    }
    if is_rotated_log(&lower) {
        return Some((ContentKind::Capture, Some(".log")));
    }
    if let Some(segment) = capture_segment(&lower, extension) {
        return Some((ContentKind::Capture, Some(segment)));
    }
    None
}

/// The extension as `Path::extension` reads it, lowercased: the part after
/// the last dot, empty for no dot or a leading dot only (`.bashrc`).
fn extension_of(lower_name: &str) -> &str {
    match lower_name.rfind('.') {
        Some(0) | None => "",
        Some(dot) => &lower_name[dot + 1..],
    }
}

/// `app.log.1`, `server.log.20`: a log rotated with a numeric suffix.
fn is_rotated_log(lower_name: &str) -> bool {
    let Some((head, digits)) = lower_name.rsplit_once('.') else {
        return false;
    };
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) && head.ends_with(".log")
}

/// `stdout`/`stderr` when the file is named exactly that, or carries it as a
/// dot-separated name segment and ends in a plain-text extension
/// (`stderr.txt`, `bash-1.stdout.txt`).
fn capture_segment(lower_name: &str, extension: &str) -> Option<&'static str> {
    let as_marker = |segment: &str| match segment {
        "stdout" => Some("stdout"),
        "stderr" => Some("stderr"),
        _ => None,
    };
    if extension.is_empty() {
        return as_marker(lower_name);
    }
    if !CAPTURE_SEGMENT_EXTENSIONS.contains(&extension) {
        return None;
    }
    let stem = &lower_name[..lower_name.len() - extension.len() - 1];
    stem.split('.').find_map(as_marker)
}

/// Which content checks apply to a file whose name did not decide its kind.
#[derive(Clone, Copy, Debug)]
struct ContentChecks {
    generated_header: bool,
    minified_density: bool,
    json_data: Option<&'static str>,
}

impl ContentChecks {
    fn for_name(file_name: &str) -> Self {
        let lower = file_name.to_ascii_lowercase();
        let extension = extension_of(&lower);
        let json_data = JSON_DATA_EXTENSIONS
            .iter()
            .find(|(candidate, _)| *candidate == extension)
            .map(|(_, marker)| *marker);
        let minified_density = MINIFIED_CANDIDATE_EXTENSIONS.contains(&extension);
        let generated_header =
            json_data.is_some() || minified_density || is_source_file_name(file_name);
        Self {
            generated_header,
            minified_density,
            json_data,
        }
    }

    fn any(self) -> bool {
        self.generated_header || self.minified_density || self.json_data.is_some()
    }

    fn head_limit(self) -> usize {
        if self.json_data.is_some() {
            JSON_HEAD_BYTES
        } else {
            HEAD_BYTES
        }
    }

    /// The content checks, run on the first bytes of a file whose name did
    /// not decide its kind.
    fn decide(self, file_name: &str, bytes: &[u8]) -> (ContentKind, Option<&'static str>) {
        let head = &bytes[..bytes.len().min(self.head_limit())];
        let short_head = &head[..head.len().min(HEAD_BYTES)];
        if self.generated_header && head_has_generated_marker(short_head) {
            return (ContentKind::Generated, None);
        }
        if self.minified_density && head_is_minified(short_head) {
            return (ContentKind::Minified, None);
        }
        if let Some(marker) = self.json_data {
            if is_json_data(file_name, short_head, head) {
                return (ContentKind::DataJson, Some(marker));
            }
        }
        (ContentKind::Text, None)
    }
}

/// A source-language file whose header may mark it generated. Docs and
/// config formats are excluded: their text is what a reader searches.
fn is_source_file_name(file_name: &str) -> bool {
    use crate::parser::LangId;
    matches!(
        crate::parser::detect_language(Path::new(file_name)),
        Some(lang) if !matches!(lang, LangId::Markdown | LangId::Html | LangId::Yaml | LangId::Toml)
    )
}

/// At least 2 KiB of head with at most one newline per KiB of it: lines
/// that long are what minifiers produce and hand-written code rarely has.
/// Whether the file at `path` with these bytes is a minified bundle: by
/// name (`*.min.js`, `*.min.css`), or a JavaScript or CSS file whose first
/// 8 KiB has almost no line breaks. The index store neither parses such a
/// file for the call graph nor embeds it for meaning search, since its names
/// are mangled and its chunks say nothing a search could use; grep still
/// searches it.
pub(crate) fn is_minified(path: &str, bytes: &[u8]) -> bool {
    let lower = path.to_ascii_lowercase();
    if MINIFIED_SUFFIXES
        .iter()
        .any(|suffix| lower.ends_with(suffix))
    {
        return true;
    }
    let file_name = lower.rsplit('/').next().unwrap_or(&lower);
    MINIFIED_CANDIDATE_EXTENSIONS.contains(&extension_of(file_name))
        && head_is_minified(&bytes[..bytes.len().min(HEAD_BYTES)])
}

fn head_is_minified(head: &[u8]) -> bool {
    head.len() >= MINIFIED_MIN_HEAD_BYTES
        && head.iter().filter(|byte| **byte == b'\n').count() <= head.len() / 1024
}

/// A JSON document that holds data: not a hand-edited manifest, not a
/// schema, and with a line of at least [`DATA_JSON_LONG_LINE_BYTES`] bytes in
/// its first [`JSON_HEAD_BYTES`]. Size alone does not make data here: a long
/// file of short lines (a provenance record, a config schema) stays text.
fn is_json_data(file_name: &str, short_head: &[u8], head: &[u8]) -> bool {
    let lower = file_name.to_ascii_lowercase();
    if is_hand_edited_manifest(&lower) || is_schema(&lower, short_head) {
        return false;
    }
    head.split(|byte| *byte == b'\n')
        .any(|line| line.len() >= DATA_JSON_LONG_LINE_BYTES)
}

/// Hand-edited manifests that share the JSON extension but describe a
/// project rather than hold data about it.
pub(crate) fn is_hand_edited_manifest(lower_name: &str) -> bool {
    lower_name.starts_with('.')
        || matches!(
            lower_name,
            "package.json" | "composer.json" | "deno.json" | "app.json" | "manifest.json"
        )
        || ((lower_name.starts_with("tsconfig") || lower_name.starts_with("jsconfig"))
            && lower_name.ends_with(".json"))
}

/// A JSON Schema document or a config that names its schema: by name
/// (`*.schema.json`) or by a `"$schema"` key near the top.
fn is_schema(lower_name: &str, short_head: &[u8]) -> bool {
    lower_name.ends_with(".schema.json")
        || memchr::memmem::find(short_head, br#""$schema""#).is_some()
}

/// Whether the first [`GENERATED_HEADER_LINES`] lines of `head` carry a
/// generated-code marker. Only the first [`HEAD_BYTES`] are looked at.
pub(crate) fn head_has_generated_marker(head: &[u8]) -> bool {
    let head = &head[..head.len().min(HEAD_BYTES)];
    text_has_generated_marker(&String::from_utf8_lossy(head))
}

/// The generated-code markers inspect has always used, over the first
/// [`GENERATED_HEADER_LINES`] lines of `text`.
fn text_has_generated_marker(text: &str) -> bool {
    let prefix = text
        .lines()
        .take(GENERATED_HEADER_LINES)
        .collect::<Vec<_>>()
        .join("\n")
        .to_ascii_lowercase();

    if prefix.contains("@generated")
        || prefix.contains("code generated by")
        || prefix.contains("do not edit")
    {
        return true;
    }

    prefix.contains("eslint-disable")
        && (prefix.contains("codegen")
            || prefix.contains("generated")
            || prefix.contains("auto-generated")
            || prefix.contains("automatically generated"))
}

/// Whether the file at `path` starts with a generated-code header, reading
/// at most [`HEAD_BYTES`].
pub(crate) fn file_head_has_generated_marker(path: &Path) -> bool {
    read_head(path, HEAD_BYTES).is_some_and(|head| head_has_generated_marker(&head))
}

/// The first `limit` bytes of the file at `path`, or `None` when it cannot
/// be opened. Never reads past `limit`.
pub(crate) fn read_head(path: &Path, limit: usize) -> Option<Vec<u8>> {
    let file = crate::project_content::open(path).ok()?;
    let mut head = Vec::with_capacity(limit.min(HEAD_BYTES));
    let mut reader = read_probe::Counting::new(file, path).take(limit as u64);
    reader.read_to_end(&mut head).ok()?;
    Some(head)
}

/// Generated path shapes: a `gen`, `generated` or `__generated__` folder,
/// protobuf outputs (`*_pb.ts`), or a `.generated.` name.
pub(crate) fn path_has_generated_shape(path: &Path) -> bool {
    if path.components().any(is_generated_segment) {
        return true;
    }

    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let file_name = file_name.to_ascii_lowercase();
    file_name.ends_with("_pb.ts")
        || file_name.ends_with("_pb.js")
        || file_name.ends_with("_pb.d.ts")
        || file_name.contains(".generated.")
}

fn is_generated_segment(component: Component<'_>) -> bool {
    let Component::Normal(segment) = component else {
        return false;
    };
    matches!(
        segment.to_str().map(str::to_ascii_lowercase).as_deref(),
        Some("gen" | "generated" | "__generated__")
    )
}

/// Only the part of `path` below `project_root` decides a generated shape;
/// folders above the project (a checkout under `~/gen/`) must not.
pub(crate) fn relative_shape_is_generated(project_root: &Path, path: &Path) -> bool {
    relative_within(project_root, path).is_some_and(|relative| path_has_generated_shape(&relative))
}

/// The part of `path` below `project_root`, or `None` when `path` is not
/// under it. A plainly relative `path` (no root, no drive) is taken as
/// already relative to the root and returned as is.
///
/// The prefix match is tried first whatever the spelling, because on Windows
/// a path such as `/home/gen/project/src/main.rs` has a root but no drive, so
/// `Path::is_absolute` calls it relative even though it names folders above
/// the project. On Windows the root and the path can also arrive in different
/// spellings (`\\?\C:\...` from canonicalization versus `C:\...`), so a
/// failed prefix match is retried on normalized copies. A rooted path that
/// still does not match (a Windows short-name alias such as `PROJEC~1`, or a
/// path on another drive) is outside the
/// root: callers then decide by the file name alone and never by folder
/// names that may lie above the project.
fn relative_within(project_root: &Path, path: &Path) -> Option<std::path::PathBuf> {
    if let Ok(relative) = path.strip_prefix(project_root) {
        return Some(relative.to_path_buf());
    }
    let rooted = path
        .components()
        .any(|component| matches!(component, Component::Prefix(_) | Component::RootDir));
    if !rooted {
        return Some(path.to_path_buf());
    }
    #[cfg(windows)]
    {
        let root = crate::windows_path::normalize_windows_path(project_root);
        let path = crate::windows_path::normalize_windows_path(path);
        let lowercase = |value: &Path| PathBuf::from(value.to_string_lossy().to_lowercase());
        if let Ok(relative) = lowercase(&path).strip_prefix(lowercase(&root)) {
            return Some(relative.to_path_buf());
        }
    }
    None
}

fn test_role(relative: &Path) -> TestRole {
    let relative = relative.to_string_lossy();
    if crate::file_kind::is_test_file(&relative) {
        TestRole::Test
    } else if crate::file_kind::is_test_support_file(&relative) {
        TestRole::Support
    } else {
        TestRole::None
    }
}

/// Byte accounting for content reads, so tests can prove a read stayed
/// within its bound. Outside tests the wrapper only forwards.
pub(crate) mod read_probe {
    use std::io::Read;
    use std::path::Path;
    #[cfg(test)]
    use std::path::PathBuf;

    #[cfg(test)]
    static BYTES_READ: std::sync::OnceLock<
        std::sync::Mutex<std::collections::BTreeMap<PathBuf, usize>>,
    > = std::sync::OnceLock::new();

    /// A reader that, in tests, records how many bytes it returned for its
    /// path.
    pub(crate) struct Counting<R> {
        inner: R,
        #[cfg(test)]
        path: PathBuf,
    }

    impl<R> Counting<R> {
        pub(crate) fn new(inner: R, path: &Path) -> Self {
            #[cfg(not(test))]
            let _ = path;
            Self {
                inner,
                #[cfg(test)]
                path: path.to_path_buf(),
            }
        }
    }

    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let read = self.inner.read(buf)?;
            #[cfg(test)]
            if let Ok(mut bytes) = BYTES_READ.get_or_init(Default::default).lock() {
                *bytes.entry(self.path.clone()).or_default() += read;
            }
            Ok(read)
        }
    }

    /// Bytes read so far from `path` through a [`Counting`] reader.
    #[cfg(test)]
    pub(crate) fn bytes_read(path: &Path) -> usize {
        BYTES_READ
            .get_or_init(Default::default)
            .lock()
            .map(|bytes| bytes.get(path).copied().unwrap_or(0))
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(root: &Path, relative: &str) -> FileKind {
        classify(root, &root.join(relative), None)
    }

    fn content_of(relative: &str) -> (ContentKind, Option<&'static str>) {
        let root = Path::new("/project");
        let kind = classify(root, &root.join(relative), Some(b"".as_slice()));
        (kind.content, kind.marker)
    }

    #[test]
    fn file_kind_every_lockfile_name_is_a_lockfile_marked_by_its_name() {
        for name in LOCKFILE_NAMES {
            assert_eq!(
                content_of(&format!("sub/{name}")),
                (ContentKind::Lockfile, Some(*name)),
                "{name}"
            );
            let lower = name.to_ascii_lowercase();
            assert_eq!(
                content_of(&lower).0,
                ContentKind::Lockfile,
                "lowercased {lower}"
            );
        }
        assert_eq!(
            content_of("vendor/custom.lock"),
            (ContentKind::Lockfile, Some(".lock"))
        );
    }

    #[test]
    fn file_kind_capture_names_are_logs_or_command_output() {
        let cases: &[(&str, &str)] = &[
            ("runs/io/stdout", "stdout"),
            ("runs/io/stderr", "stderr"),
            ("runs/stderr.txt", "stderr"),
            ("receipts/bash-1.stdout.txt", "stdout"),
            ("receipts/run.stdout.out", "stdout"),
            ("receipts/run.stderr.log", ".log"),
            ("var/app.log", ".log"),
            ("var/app.log.1", ".log"),
            ("var/app.log.20", ".log"),
            ("records/x.jsonl", ".jsonl"),
            ("records/x.ndjson", ".ndjson"),
            ("receipts/build.stdout", ".stdout"),
            ("receipts/build.stderr", ".stderr"),
        ];
        for (path, marker) in cases {
            assert_eq!(
                content_of(path),
                (ContentKind::Capture, Some(*marker)),
                "{path}"
            );
        }
    }

    #[test]
    fn file_kind_capture_rule_never_catches_source_docs_or_folder_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let never = [
            "src/log.rs",
            "src/logger.ts",
            "src/stdout.rs",
            "src/stderr.ts",
            "tools/capture.py",
            "tools/capture-sink.js",
            "src/foo.log.ts",
            "CHANGELOG.md",
            "changelog",
            "notes/release.log.md",
            "test/common-auth-094-adoption/log/contract.mjs",
            "logs/README.md",
            "receipts/README.md",
            "research/v1/analysis.py",
            "tests/golden/expected.out",
            "tests/golden/expected.err",
            "src/stdout_writer.go",
        ];
        for path in never {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "const needle = 1;\n").unwrap();
            assert_eq!(kind_of(root, path).content, ContentKind::Text, "{path}");
        }
    }

    #[test]
    fn file_kind_minified_and_generated_names() {
        assert_eq!(
            content_of("vendor/highlight.min.js"),
            (ContentKind::Minified, Some(".min.js"))
        );
        assert_eq!(
            content_of("dist/site.MIN.CSS"),
            (ContentKind::Minified, Some(".min.css"))
        );
        assert_eq!(
            content_of("dist/app.js.map"),
            (ContentKind::Generated, Some(".js.map"))
        );
        assert_eq!(
            content_of("src/generated/types.ts"),
            (ContentKind::Generated, None)
        );
        assert_eq!(content_of("src/api_pb.ts"), (ContentKind::Generated, None));
        // A bare `.map` is too generic to mean a source map.
        assert_eq!(content_of("data/world.map").0, ContentKind::Text);
        // Folders above the project root never decide a generated shape.
        let kind = classify(
            Path::new("/home/gen/project"),
            Path::new("/home/gen/project/src/main.rs"),
            Some(b"fn main() {}\n".as_slice()),
        );
        assert_eq!(kind.content, ContentKind::Text);
    }

    #[test]
    fn file_kind_path_outside_the_root_is_decided_by_its_file_name() {
        // A file the root does not contain: its folders (`gen/`) never
        // decide, but a generated file name still does.
        let root = Path::new("/project");
        let text = b"fn main() {}\n".as_slice();
        let kind = classify(root, Path::new("/elsewhere/gen/main.rs"), Some(text));
        assert_eq!(kind.content, ContentKind::Text);
        let kind = classify(root, Path::new("/elsewhere/gen/api_pb.ts"), Some(text));
        assert_eq!(kind.content, ContentKind::Generated);
    }

    #[cfg(windows)]
    #[test]
    fn file_kind_windows_folders_above_the_root_never_decide() {
        let text = b"fn main() {}\n".as_slice();
        let content =
            |root: &str, path: &str| classify(Path::new(root), Path::new(path), Some(text)).content;
        // Drive-letter root and file.
        let root = r"C:\Users\gen\project";
        assert_eq!(
            content(root, r"C:\Users\gen\project\src\main.rs"),
            ContentKind::Text
        );
        assert_eq!(
            content(root, r"C:\Users\gen\project\src\generated\types.ts"),
            ContentKind::Generated
        );
        // A `\\?\`-prefixed root (the form Windows canonicalization returns)
        // with a plainly spelled file.
        let verbatim = r"\\?\C:\Users\gen\project";
        assert_eq!(
            content(verbatim, r"C:\Users\gen\project\src\main.rs"),
            ContentKind::Text
        );
        assert_eq!(
            content(verbatim, r"C:\Users\gen\project\gen\schema.ts"),
            ContentKind::Generated
        );
        // A root with no drive letter, which `Path::is_absolute` calls
        // relative on Windows.
        assert_eq!(
            content(r"\home\gen\project", r"\home\gen\project\src\main.rs"),
            ContentKind::Text
        );
        // A file whose spelling cannot be matched to the root (no drive
        // letter, or a Windows short-name folder alias such as `PROJEC~1`): only
        // its file name decides.
        assert_eq!(
            content(root, r"\Users\gen\project\src\main.rs"),
            ContentKind::Text
        );
        assert_eq!(
            content(root, r"C:\Users\gen\PROJEC~1\src\main.rs"),
            ContentKind::Text
        );
        assert_eq!(
            content(root, r"C:\Users\gen\PROJEC~1\src\api_pb.ts"),
            ContentKind::Generated
        );
    }

    #[test]
    fn file_kind_content_signals_decide_only_when_the_name_does_not() {
        let root = Path::new("/project");
        let header = b"// Code generated by protoc-gen-es. DO NOT EDIT.\nexport const x = 1;\n";
        let kind = classify(root, &root.join("src/api.ts"), Some(header.as_slice()));
        assert_eq!((kind.content, kind.marker), (ContentKind::Generated, None));

        let bundle = "x".repeat(4 * 1024);
        let kind = classify(root, &root.join("dist/bundle.js"), Some(bundle.as_bytes()));
        assert_eq!((kind.content, kind.marker), (ContentKind::Minified, None));

        // Docs are never read for headers: their text is what a reader wants.
        let kind = classify(root, &root.join("docs/api.md"), Some(header.as_slice()));
        assert_eq!(kind.content, ContentKind::Text);

        let source: String = (0..200).map(|i| format!("const v{i} = {i};\n")).collect();
        let kind = classify(root, &root.join("src/app.js"), Some(source.as_bytes()));
        assert_eq!(kind.content, ContentKind::Text);
    }

    #[test]
    fn file_kind_json_data_needs_a_long_line_and_exempts_schemas_and_manifests() {
        let root = Path::new("/project");
        let long_line = format!("{{\"blob\": \"{}\"}}\n", "a".repeat(300));
        let classify_json = |name: &str, body: &str| {
            let kind = classify(root, &root.join(name), Some(body.as_bytes()));
            (kind.content, kind.marker)
        };
        assert_eq!(
            classify_json("research/mutations.json", &long_line),
            (ContentKind::DataJson, Some(".json"))
        );
        assert_eq!(
            classify_json("maps/world.geojson", &long_line),
            (ContentKind::DataJson, Some(".geojson"))
        );
        assert_eq!(
            classify_json("assets/aft.schema.json", &long_line).0,
            ContentKind::Text
        );
        let with_schema_key = format!("{{\"$schema\": \"x\",\n{long_line}");
        assert_eq!(
            classify_json("config/tool.json", &with_schema_key).0,
            ContentKind::Text
        );
        assert_eq!(
            classify_json("package.json", &long_line).0,
            ContentKind::Text
        );
        assert_eq!(
            classify_json("tsconfig.build.json", &long_line).0,
            ContentKind::Text
        );
        assert_eq!(
            classify_json(".eslintrc.json", &long_line).0,
            ContentKind::Text
        );
        // Size alone does not make data: 47 KB of short lines stays text.
        let short_lines: String = (0..700)
            .map(|i| format!("  \"entry{i}\": \"value of about one hundred bytes ............................\",\n"))
            .collect();
        assert!(short_lines.len() > 40 * 1024);
        assert_eq!(
            classify_json("src/source-output.json", &short_lines).0,
            ContentKind::Text
        );
    }

    #[test]
    fn file_kind_json_long_line_test_reads_at_most_64_kib() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let short_lines = "  \"k\": \"v\",\n".repeat(JSON_HEAD_BYTES / 12 + 100);
        assert!(short_lines.len() > JSON_HEAD_BYTES);
        let late = root.join("late.json");
        std::fs::write(
            &late,
            format!("{short_lines}\"blob\": \"{}\"\n", "a".repeat(400)),
        )
        .unwrap();
        assert_eq!(kind_of(root, "late.json").content, ContentKind::Text);
        assert!(read_probe::bytes_read(&late) <= JSON_HEAD_BYTES);

        let early = root.join("early.json");
        std::fs::write(
            &early,
            format!("\"blob\": \"{}\"\n{short_lines}", "a".repeat(400)),
        )
        .unwrap();
        assert_eq!(kind_of(root, "early.json").content, ContentKind::DataJson);
        assert!(read_probe::bytes_read(&early) <= JSON_HEAD_BYTES);
    }

    /// The index store's minified check agrees with the file kinds: by
    /// name, or a JavaScript or CSS head with almost no line breaks, and
    /// never a source file of another language however dense.
    #[test]
    fn is_minified_matches_the_minified_file_kind() {
        let dense = format!("var a=1;{}", "b();".repeat(2048));
        let lines = "export function a() {}\n".repeat(400);
        assert!(is_minified("web/app.min.js", lines.as_bytes()));
        assert!(is_minified("web/STYLE.MIN.CSS", b""));
        assert!(is_minified("web/bundle.js", dense.as_bytes()));
        assert!(!is_minified("web/app.js", lines.as_bytes()));
        assert!(!is_minified("src/lib.rs", dense.as_bytes()));
        assert!(!is_minified("web.min.js/app.ts", lines.as_bytes()));
        for (path, bytes) in [
            ("bundle.js", dense.as_bytes()),
            ("app.js", lines.as_bytes()),
        ] {
            let kind = classify(Path::new("/p"), &Path::new("/p").join(path), Some(bytes));
            assert_eq!(
                kind.content == ContentKind::Minified,
                is_minified(path, bytes),
                "{path}"
            );
        }
    }

    #[test]
    fn file_kind_single_line_bundle_is_read_at_most_8_kib() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let bundle = root.join("bundle.js");
        std::fs::write(&bundle, "x".repeat(10 * 1024 * 1024)).unwrap();

        // inspect's generated-header check used to read the whole first line.
        assert!(!crate::inspect::is_generated_file(root, &bundle));
        let after_inspect = read_probe::bytes_read(&bundle);
        assert!(
            after_inspect <= HEAD_BYTES,
            "inspect read {after_inspect} bytes of a single-line bundle"
        );

        assert_eq!(kind_of(root, "bundle.js").content, ContentKind::Minified);
        let after_classify = read_probe::bytes_read(&bundle) - after_inspect;
        assert!(
            after_classify <= HEAD_BYTES,
            "classify read {after_classify} bytes of a single-line bundle"
        );
    }

    #[test]
    fn file_kind_held_bytes_and_cache_avoid_reads() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let file = root.join("app.ts");
        std::fs::write(&file, "export const x = 1;\n").unwrap();
        classify(root, &file, Some(b"export const x = 1;\n".as_slice()));
        assert_eq!(read_probe::bytes_read(&file), 0);

        let mut cache = FileKindCache::new();
        cache.classify(root, &file, None);
        let first = read_probe::bytes_read(&file);
        assert!(first > 0);
        cache.classify(root, &file, None);
        assert_eq!(read_probe::bytes_read(&file), first);
    }

    #[test]
    fn file_kind_test_role_is_independent_of_content() {
        let kind = classify(
            Path::new("/project"),
            Path::new("/project/tests/fixtures/run/stderr"),
            None,
        );
        assert_eq!(kind.content, ContentKind::Capture);
        assert_eq!(kind.test, TestRole::Test);
        let kind = classify(
            Path::new("/project"),
            Path::new("/project/src/__fixtures__/run/stderr"),
            None,
        );
        assert_eq!(
            (kind.content, kind.test),
            (ContentKind::Capture, TestRole::Support)
        );
        let kind = classify(
            Path::new("/project"),
            Path::new("/project/src/app.test.ts"),
            Some(b"".as_slice()),
        );
        assert_eq!(
            (kind.content, kind.test),
            (ContentKind::Text, TestRole::Test)
        );
    }
}

/// Whether a project-relative path is a test-support / standalone file that
/// should be EXCLUDED from dead_code and unused_exports reporting. These files
/// (test fixtures, corpora, mock data, snapshots) are consumed by file path or
/// loaded dynamically — never imported as modules — so static reachability
/// always reports their symbols as dead/unused. They are noise, not signal.
///
/// Edges FROM these files are still honored elsewhere (their imports keep
/// product code live); only their own exported symbols are suppressed from the
/// dead/unused lists. The match is on conventional directory names anywhere in
/// the path, kept conservative to avoid hiding real product directories.
pub(crate) fn is_test_support_file(relative_path: &str) -> bool {
    let normalized = relative_path.replace('\\', "/");
    normalized.split('/').any(is_test_support_dir_name)
}

/// Whether one path segment is a conventional test-support directory name
/// (fixtures, corpora, mock data, snapshots). Shared by the reporting filter
/// above and the language-server walk, which starts no server for these
/// directories.
pub(crate) fn is_test_support_dir_name(segment: &str) -> bool {
    matches!(
        segment,
        "fixtures"
            | "__fixtures__"
            | "testdata"
            | "test-data"
            | "__mocks__"
            | "__snapshots__"
            | "corpora"
    )
}

/// Whether a project-relative path is an actual automated-test file (unit /
/// integration / spec), as opposed to product code. Search uses this to hide test
/// files by default; inspect uses it to distinguish test-origin references from
/// production usage without excluding the test files from analysis.
///
/// This is intentionally SEPARATE from `is_test_support_file`: test-support files
/// are reporting noise excluded by path, while real tests remain scanned and can
/// move otherwise-dead production symbols into the test-only bucket. Matching is
/// high-precision to avoid hiding real product code.
pub(crate) fn is_test_file(relative_path: &str) -> bool {
    let normalized = relative_path.replace('\\', "/");

    // Directory-segment conventions: `__tests__` (JS/TS) and a `tests` test root
    // (Rust integration tests, Python). Singular `test`/`spec` are omitted —
    // they collide with product modules and their files are caught by name below.
    if normalized
        .split('/')
        .any(|segment| matches!(segment, "__tests__" | "__test__" | "tests"))
    {
        return true;
    }

    let file = normalized.rsplit('/').next().unwrap_or(&normalized);
    let lower = file.to_ascii_lowercase();

    // `*.test.<ext>` / `*.spec.<ext>` — JS/TS/JSX/TSX/Vue/mjs/cjs/…
    if lower.contains(".test.") || lower.contains(".spec.") {
        return true;
    }

    // Per-language filename suffixes (lowercase conventions).
    if lower.ends_with("_test.rs")
        || lower.ends_with("_test.go")
        || lower.ends_with("_test.py")
        || lower.ends_with("_test.rb")
        || lower.ends_with("_test.exs")
        || lower.ends_with("_spec.rb")
        || (lower.starts_with("test_") && lower.ends_with(".py"))
    {
        return true;
    }

    // CamelCase test classes — matched case-SENSITIVELY so product files like
    // `latest.java` (which ends with "test.java" lowercased) don't false-match.
    const CAMEL_SUFFIXES: &[&str] = &[
        "Test.java",
        "Tests.java",
        "Test.kt",
        "Tests.kt",
        "Test.cs",
        "Tests.cs",
        "Test.swift",
        "Tests.swift",
        "Test.scala",
        "Spec.scala",
    ];
    CAMEL_SUFFIXES.iter().any(|suffix| file.ends_with(suffix))
}

#[cfg(test)]
mod test_support_tests {
    use super::{is_test_file, is_test_support_file};

    #[test]
    fn is_test_file_matches_real_test_files() {
        // JS/TS family: *.test.* / *.spec.*
        for p in [
            "src/foo.test.ts",
            "src/foo.test.tsx",
            "src/bar.spec.js",
            "packages/x/component.test.jsx",
            "app/foo.test.mjs",
            "src/comp.spec.vue",
        ] {
            assert!(is_test_file(p), "{p} should be a test file");
        }
        // __tests__ directory and tests roots.
        assert!(is_test_file("packages/x/__tests__/reading.ts"));
        assert!(is_test_file("crates/aft/tests/integration/main.rs"));
        // Per-language filename suffixes.
        assert!(is_test_file("crates/aft/src/foo_test.rs"));
        assert!(is_test_file("pkg/handler_test.go"));
        assert!(is_test_file("app/test_models.py"));
        assert!(is_test_file("app/models_test.py"));
        assert!(is_test_file("spec/user_spec.rb"));
        // CamelCase test classes (case-sensitive).
        assert!(is_test_file("src/main/UserServiceTest.java"));
        assert!(is_test_file("src/FooTests.cs"));
        assert!(is_test_file("Sources/AppTests.swift"));
        // Windows separators normalize.
        assert!(is_test_file("packages\\x\\__tests__\\a.ts"));
    }

    #[test]
    fn is_test_file_rejects_product_files() {
        for p in [
            "crates/aft/src/inspect/job.rs",
            "packages/x/src/index.ts",
            "src/contestant.ts",     // "test" substring, not a test file
            "src/greatest.ts",       // ends with "test" stem, not ".test."
            "src/latest.java",       // case-sensitive guard: not "Test.java"
            "src/my_attestation.py", // not test_*/*_test
            "src/test/helper.ts",    // singular `test` dir must not blanket-match
        ] {
            assert!(!is_test_file(p), "{p} must NOT be a test file");
        }
    }

    #[test]
    fn matches_conventional_support_dirs() {
        assert!(is_test_support_file("crates/aft/tests/fixtures/sample.ts"));
        assert!(is_test_support_file(
            "packages/x/__tests__/e2e/fixtures/a.ts"
        ));
        assert!(is_test_support_file(
            "benchmarks/codegraph/corpora/repo/lib.go"
        ));
        assert!(is_test_support_file("src/__mocks__/fs.ts"));
        assert!(is_test_support_file("src/__snapshots__/render.snap"));
        assert!(is_test_support_file("internal/testdata/golden.json"));
        // Windows-style separators normalize.
        assert!(is_test_support_file("crates\\aft\\tests\\fixtures\\x.rs"));
    }

    #[test]
    fn does_not_match_product_or_test_files() {
        // A real product file under src must never be excluded.
        assert!(!is_test_support_file("crates/aft/src/inspect/job.rs"));
        // Test FILES are not support files (they hold real assertions/roots).
        assert!(!is_test_support_file(
            "packages/x/__tests__/reading.test.ts"
        ));
        assert!(!is_test_support_file(
            "crates/aft/tests/integration/main.rs"
        ));
        // Substring of a segment must not match (only whole segments).
        assert!(!is_test_support_file("src/fixturesHelper.ts"));
        assert!(!is_test_support_file("src/my_corpora_loader.rs"));
    }
}
