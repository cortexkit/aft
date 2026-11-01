//! Deterministic symbol-level summaries for Git revision ranges.
//!
//! This module reads blobs directly from Git so delivery-review packets describe
//! committed revisions rather than the caller's working tree.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::commands::outline::symbol_to_entry;
use crate::parser::{detect_language, extract_symbols_from_tree, parse_resolved_source, LangId};
use crate::symbols::Symbol;

/// Fixed statement carried by every range packet.
pub const RANGE_SYMBOL_PACKET_DISCLAIMER: &str =
    "Derived from the diff; proves what changed, not that it works.";

/// One changed symbol rendered with the fields used by the outline machinery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolDiffEntry {
    pub name: String,
    pub kind: String,
    pub signature_line: Option<String>,
    /// Dot-separated outline scope, such as `Trait for Type` or `Outer.Inner`.
    pub container_path: String,
}

/// Symbol and line-count changes for one file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileSymbolDiff {
    pub symbols_unavailable: bool,
    pub old_line_count: usize,
    pub new_line_count: usize,
    pub line_count_delta: i64,
    pub added: Vec<SymbolDiffEntry>,
    pub removed: Vec<SymbolDiffEntry>,
    pub modified: Vec<SymbolDiffEntry>,
}

/// The file-level change represented by a range packet entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
    Added,
    Removed,
    Modified,
}

/// A root-relative file entry in a range packet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RangeFileSymbolDiff {
    /// Git's root-relative pathname, normalized to forward slashes for display.
    pub path: String,
    pub change: FileChangeKind,
    /// Set on the destination half of a rename; the packet represents it as an added file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
    /// Set on the source half of a rename; the packet represents it as a removed file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_to: Option<String>,
    #[serde(flatten)]
    pub diff: FileSymbolDiff,
}

/// A deterministic delivery-review packet for one Git revision range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RangeSymbolPacket {
    pub base_sha: String,
    pub tip_sha: String,
    pub files: Vec<RangeFileSymbolDiff>,
    pub disclaimer: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct SymbolIdentity {
    container_path: String,
    name: String,
    kind: String,
}

#[derive(Debug, Clone)]
struct ParsedSymbol {
    entry: SymbolDiffEntry,
    body: Option<Vec<u8>>,
}

#[derive(Debug)]
enum GitFileChange {
    Added(Vec<u8>),
    Removed(Vec<u8>),
    Modified(Vec<u8>),
    Renamed { old: Vec<u8>, new: Vec<u8> },
    Copied { old: Vec<u8>, new: Vec<u8> },
}

/// Compare two file blobs using the shared tree-sitter parser and symbol extractors.
///
/// JSON and YAML are deliberately treated as data files here even though the outline
/// command can offer lightweight summaries for them. A delivery packet must not imply
/// source-symbol coverage where there is no code-symbol contract to review.
pub(crate) fn symbol_diff_file(
    lang: LangId,
    path: &Path,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
) -> FileSymbolDiff {
    let old_line_count = line_count(old.unwrap_or_default());
    let new_line_count = line_count(new.unwrap_or_default());

    if !supports_symbol_diff(lang) {
        return unavailable_file_diff(old_line_count, new_line_count);
    }

    let old_by_identity = match old {
        Some(source) => match parse_symbols(path, source, lang) {
            Some(symbols) => symbols,
            None => return unavailable_file_diff(old_line_count, new_line_count),
        },
        None => BTreeMap::new(),
    };
    let new_by_identity = match new {
        Some(source) => match parse_symbols(path, source, lang) {
            Some(symbols) => symbols,
            None => return unavailable_file_diff(old_line_count, new_line_count),
        },
        None => BTreeMap::new(),
    };

    let mut old_by_identity = old_by_identity;
    let mut added = Vec::new();
    let mut modified = Vec::new();

    for (identity, new_symbols) in new_by_identity {
        let mut old_symbols = old_by_identity.remove(&identity).unwrap_or_default();
        let mut changed = Vec::new();
        // Match unchanged overloads first so adding or reordering an overload
        // does not shift every later declaration into a false modification.
        for new_symbol in new_symbols {
            if let Some(index) = old_symbols
                .iter()
                .position(|old| !symbol_changed(old, &new_symbol))
            {
                old_symbols.remove(index);
            } else {
                changed.push(new_symbol);
            }
        }
        // Pair every changed declaration whose signature still exists before
        // pairing the rest by position. Otherwise an added overload declared
        // earlier could take the old declaration of a body-only change, and
        // the two would be reported the wrong way round.
        let mut is_modified = vec![false; changed.len()];
        for (index, new_symbol) in changed.iter().enumerate() {
            if let Some(old_index) = old_symbols
                .iter()
                .position(|old| old.entry.signature_line == new_symbol.entry.signature_line)
            {
                old_symbols.remove(old_index);
                is_modified[index] = true;
            }
        }
        for flag in is_modified.iter_mut().filter(|flag| !**flag) {
            if old_symbols.is_empty() {
                break;
            }
            old_symbols.remove(0);
            *flag = true;
        }
        for (new_symbol, is_modified) in changed.into_iter().zip(is_modified) {
            if is_modified {
                modified.push(new_symbol.entry);
            } else {
                added.push(new_symbol.entry);
            }
        }
        if !old_symbols.is_empty() {
            old_by_identity.insert(identity, old_symbols);
        }
    }
    let removed = old_by_identity
        .into_values()
        .flatten()
        .map(|symbol| symbol.entry)
        .collect();

    FileSymbolDiff {
        symbols_unavailable: false,
        old_line_count,
        new_line_count,
        line_count_delta: line_count_delta(old_line_count, new_line_count),
        added,
        removed,
        modified,
    }
}

/// Build a deterministic packet for `base_sha..tip_sha` without reading the worktree.
///
/// An unreadable Git range produces an empty packet rather than a partial packet. The
/// caller can distinguish that case from an unchanged valid range by resolving the
/// requested revisions before calling this low-level, infallible engine entry point.
pub fn symbol_diff_range(repo_root: &Path, base_sha: &str, tip_sha: &str) -> RangeSymbolPacket {
    let mut files = BTreeMap::new();

    if let Some(changes) = git_name_status(repo_root, base_sha, tip_sha) {
        let mut blobs = GitBlobReader::new(repo_root);
        for change in changes {
            match change {
                GitFileChange::Added(path) => {
                    insert_file_entry(
                        &mut files,
                        repo_root,
                        &path,
                        None,
                        read_git_blob(repo_root, &mut blobs, tip_sha, &path),
                        FileChangeKind::Added,
                        None,
                        None,
                    );
                }
                GitFileChange::Removed(path) => {
                    insert_file_entry(
                        &mut files,
                        repo_root,
                        &path,
                        read_git_blob(repo_root, &mut blobs, base_sha, &path),
                        None,
                        FileChangeKind::Removed,
                        None,
                        None,
                    );
                }
                GitFileChange::Modified(path) => {
                    insert_file_entry(
                        &mut files,
                        repo_root,
                        &path,
                        read_git_blob(repo_root, &mut blobs, base_sha, &path),
                        read_git_blob(repo_root, &mut blobs, tip_sha, &path),
                        FileChangeKind::Modified,
                        None,
                        None,
                    );
                }
                GitFileChange::Renamed { old, new } => {
                    let old_display = git_path_display(&old);
                    let new_display = git_path_display(&new);
                    insert_file_entry(
                        &mut files,
                        repo_root,
                        &old,
                        read_git_blob(repo_root, &mut blobs, base_sha, &old),
                        None,
                        FileChangeKind::Removed,
                        None,
                        Some(new_display),
                    );
                    insert_file_entry(
                        &mut files,
                        repo_root,
                        &new,
                        None,
                        read_git_blob(repo_root, &mut blobs, tip_sha, &new),
                        FileChangeKind::Added,
                        Some(old_display),
                        None,
                    );
                }
                GitFileChange::Copied { old, new } => {
                    insert_file_entry(
                        &mut files,
                        repo_root,
                        &new,
                        None,
                        read_git_blob(repo_root, &mut blobs, tip_sha, &new),
                        FileChangeKind::Added,
                        Some(git_path_display(&old)),
                        None,
                    );
                }
            }
        }
    }

    RangeSymbolPacket {
        base_sha: base_sha.to_string(),
        tip_sha: tip_sha.to_string(),
        files: files.into_values().collect(),
        disclaimer: RANGE_SYMBOL_PACKET_DISCLAIMER,
    }
}

fn supports_symbol_diff(lang: LangId) -> bool {
    !matches!(lang, LangId::Json | LangId::Yaml)
}

fn unavailable_file_diff(old_line_count: usize, new_line_count: usize) -> FileSymbolDiff {
    FileSymbolDiff {
        symbols_unavailable: true,
        old_line_count,
        new_line_count,
        line_count_delta: line_count_delta(old_line_count, new_line_count),
        added: Vec::new(),
        removed: Vec::new(),
        modified: Vec::new(),
    }
}

fn parse_symbols(
    path: &Path,
    source: &[u8],
    lang: LangId,
) -> Option<BTreeMap<SymbolIdentity, Vec<ParsedSymbol>>> {
    let source = std::str::from_utf8(source).ok()?;
    let (tree, lang) = parse_resolved_source(path, source, lang).ok()?;
    let symbols = extract_symbols_from_tree(source, &tree, lang).ok()?;
    let line_starts = line_start_offsets(source);

    let mut parsed = BTreeMap::new();
    for symbol in symbols {
        let (identity, parsed_symbol) = parsed_symbol(source, &line_starts, symbol);
        parsed
            .entry(identity)
            .or_insert_with(Vec::new)
            .push(parsed_symbol);
    }
    Some(parsed)
}

fn parsed_symbol(
    source: &str,
    line_starts: &[usize],
    symbol: Symbol,
) -> (SymbolIdentity, ParsedSymbol) {
    let outline_entry = symbol_to_entry(&symbol);
    let container_path = symbol.scope_chain.join(".");
    let identity = SymbolIdentity {
        container_path: container_path.clone(),
        name: outline_entry.name.clone(),
        kind: outline_entry.kind.clone(),
    };
    let entry = SymbolDiffEntry {
        name: outline_entry.name,
        kind: outline_entry.kind,
        signature_line: outline_entry.signature,
        container_path,
    };
    let body = source_bytes_for_symbol(source, line_starts, &symbol);
    (identity, ParsedSymbol { entry, body })
}

fn line_start_offsets(source: &str) -> Vec<usize> {
    let mut offsets = vec![0];
    offsets.extend(
        source
            .bytes()
            .enumerate()
            .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
    );
    offsets
}

fn source_bytes_for_symbol(
    source: &str,
    line_starts: &[usize],
    symbol: &Symbol,
) -> Option<Vec<u8>> {
    let start = byte_offset_at(
        source,
        line_starts,
        symbol.range.start_line,
        symbol.range.start_col,
    )?;
    let end = byte_offset_at(
        source,
        line_starts,
        symbol.range.end_line,
        symbol.range.end_col,
    )?;
    (start <= end).then(|| source.as_bytes()[start..end].to_vec())
}

fn byte_offset_at(source: &str, line_starts: &[usize], line: u32, column: u32) -> Option<usize> {
    let line_start = *line_starts.get(line as usize)?;
    let offset = line_start.checked_add(column as usize)?;
    (offset <= source.len()).then_some(offset)
}

fn symbol_changed(old: &ParsedSymbol, new: &ParsedSymbol) -> bool {
    old.body != new.body || old.entry.signature_line != new.entry.signature_line
}

fn line_count(source: &[u8]) -> usize {
    if source.is_empty() {
        return 0;
    }
    source.iter().filter(|byte| **byte == b'\n').count() + usize::from(!source.ends_with(b"\n"))
}

fn line_count_delta(old_line_count: usize, new_line_count: usize) -> i64 {
    let delta = new_line_count as i128 - old_line_count as i128;
    delta.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn insert_file_entry(
    files: &mut BTreeMap<Vec<u8>, RangeFileSymbolDiff>,
    _repo_root: &Path,
    path_bytes: &[u8],
    old: Option<Vec<u8>>,
    new: Option<Vec<u8>>,
    change: FileChangeKind,
    renamed_from: Option<String>,
    renamed_to: Option<String>,
) {
    let path = path_from_git_bytes(path_bytes);
    let diff = match detect_language(&path) {
        Some(lang) => symbol_diff_file(lang, &path, old.as_deref(), new.as_deref()),
        None => unavailable_file_diff(
            line_count(old.as_deref().unwrap_or_default()),
            line_count(new.as_deref().unwrap_or_default()),
        ),
    };
    files.insert(
        path_bytes.to_vec(),
        RangeFileSymbolDiff {
            path: git_path_display(path_bytes),
            change,
            renamed_from,
            renamed_to,
            diff,
        },
    );
}

fn git_name_status(repo_root: &Path, base_sha: &str, tip_sha: &str) -> Option<Vec<GitFileChange>> {
    if !crate::developer_tools::git_usable() {
        return None;
    }
    let range = format!("{base_sha}..{tip_sha}");
    let output = crate::effective_path::new_command("git")
        .build()
        .arg("-C")
        .arg(repo_root)
        .args([
            "-c",
            "core.quotepath=false",
            "diff",
            "--name-status",
            "-z",
            "-M",
            "--no-ext-diff",
            &range,
        ])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse_name_status(&output.stdout))
}

fn parse_name_status(output: &[u8]) -> Vec<GitFileChange> {
    let fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let mut changes = Vec::new();
    let mut index = 0;

    while let Some(status) = fields.get(index) {
        index += 1;
        let kind = status.first().copied();
        match kind {
            Some(b'R') => {
                let (Some(old), Some(new)) = (fields.get(index), fields.get(index + 1)) else {
                    break;
                };
                changes.push(GitFileChange::Renamed {
                    old: (*old).to_vec(),
                    new: (*new).to_vec(),
                });
                index += 2;
            }
            Some(b'C') => {
                let (Some(old), Some(new)) = (fields.get(index), fields.get(index + 1)) else {
                    break;
                };
                changes.push(GitFileChange::Copied {
                    old: (*old).to_vec(),
                    new: (*new).to_vec(),
                });
                index += 2;
            }
            Some(kind) => {
                let Some(path) = fields.get(index) else {
                    break;
                };
                let change = match kind {
                    b'A' => GitFileChange::Added((*path).to_vec()),
                    b'D' => GitFileChange::Removed((*path).to_vec()),
                    _ => GitFileChange::Modified((*path).to_vec()),
                };
                changes.push(change);
                index += 1;
            }
            None => break,
        }
    }

    changes
}

fn read_git_blob(
    repo_root: &Path,
    batch: &mut Option<GitBlobReader>,
    sha: &str,
    path: &[u8],
) -> Option<Vec<u8>> {
    if let Some(reader) = batch.as_mut() {
        match reader.read(sha, path) {
            Ok(content) => return content,
            Err(_) => {
                batch.take();
            }
        }
    }
    // Older Git versions or a failed batch transport retain the per-blob
    // fallback, including its existing unreadable-blob behavior.
    read_git_blob_fallback(repo_root, sha, path)
}

struct GitBlobReader {
    child: std::process::Child,
    input: Option<std::process::ChildStdin>,
    output: std::io::BufReader<std::process::ChildStdout>,
}

impl GitBlobReader {
    fn new(root: &Path) -> Option<Self> {
        use std::process::Stdio;
        if !crate::developer_tools::git_usable() {
            return None;
        }
        let mut child = crate::effective_path::new_command("git")
            .build()
            .arg("-C")
            .arg(root)
            .args(["cat-file", "--batch", "-z"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        #[cfg(test)]
        BLOB_READER_SPAWNS.with(|count| count.set(count.get() + 1));
        let input = child.stdin.take();
        let output = std::io::BufReader::new(child.stdout.take()?);
        Some(Self {
            child,
            input,
            output,
        })
    }

    fn read(&mut self, sha: &str, path: &[u8]) -> std::io::Result<Option<Vec<u8>>> {
        use std::io::{BufRead, Read, Write};
        let object = git_object_spec(sha, path);
        let request = object.as_encoded_bytes();
        if request.contains(&0) {
            return Ok(None);
        }
        let input = self.input.as_mut().expect("batch stdin is open");
        input.write_all(request)?;
        input.write_all(b"\0")?;
        input.flush()?;
        let mut header = Vec::new();
        if self.output.read_until(b'\n', &mut header)? == 0 {
            return Err(std::io::Error::other("git batch exited before response"));
        }
        let fields: Vec<&str> = std::str::from_utf8(&header)
            .unwrap_or("")
            .split_whitespace()
            .collect();
        if fields.len() == 3 && matches!(fields[1], "blob" | "tree" | "commit" | "tag") {
            let size = fields[2].parse::<usize>().map_err(std::io::Error::other)?;
            let mut bytes = vec![0; size];
            self.output.read_exact(&mut bytes)?;
            let mut separator = [0];
            self.output.read_exact(&mut separator)?;
            if separator != *b"\n" {
                return Err(std::io::Error::other("invalid git batch separator"));
            }
            return Ok((fields[1] == "blob").then_some(bytes));
        }
        // Missing-object replies echo the request. Use its byte length to drain
        // even a missing pathname containing embedded newlines before continuing.
        let missing_len = request.len() + b" missing\n".len();
        if header.len() <= missing_len {
            let used = header.len();
            header.resize(missing_len, 0);
            self.output.read_exact(&mut header[used..])?;
            if header.strip_suffix(b" missing\n") == Some(request) {
                return Ok(None);
            }
        }
        Err(std::io::Error::other("invalid git batch header"))
    }
}

impl Drop for GitBlobReader {
    fn drop(&mut self) {
        self.input.take();
        // Kill before waiting so a broken response cannot leave a child blocked
        // writing unread bytes into its pipe during error cleanup.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_git_blob_fallback(repo_root: &Path, sha: &str, path_bytes: &[u8]) -> Option<Vec<u8>> {
    if !crate::developer_tools::git_usable() {
        return None;
    }
    #[cfg(test)]
    BLOB_READER_SPAWNS.with(|count| count.set(count.get() + 1));
    let object = git_object_spec(sha, path_bytes);
    let output = crate::effective_path::new_command("git")
        .build()
        .arg("-C")
        .arg(repo_root)
        .args(["-c", "core.quotepath=false", "show", "--no-textconv"])
        .arg(object)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

fn git_path_display(path_bytes: &[u8]) -> String {
    String::from_utf8_lossy(path_bytes).replace('\\', "/")
}

fn path_from_git_bytes(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        PathBuf::from(OsString::from_vec(bytes.to_vec()))
    }

    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn git_object_spec(sha: &str, path_bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;

        let mut bytes = sha.as_bytes().to_vec();
        bytes.push(b':');
        bytes.extend_from_slice(path_bytes);
        OsString::from_vec(bytes)
    }

    #[cfg(not(unix))]
    {
        OsString::from(format!("{sha}:{}", String::from_utf8_lossy(path_bytes)))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn h_header_revision_symbols_use_each_blob_language() {
        let old = b"int old_c_function(void);\n";
        let new = b"template <typename T> class Widget { public: T run(T value); };\n";
        let diff = super::symbol_diff_file(
            crate::parser::LangId::C,
            std::path::Path::new("widget.h"),
            Some(old),
            Some(new),
        );
        assert!(!diff.symbols_unavailable);
        assert!(diff.added.iter().any(|symbol| symbol.name == "Widget"));
        assert!(diff.added.iter().any(|symbol| symbol.name == "run"));
        assert!(diff
            .removed
            .iter()
            .any(|symbol| symbol.name == "old_c_function"));
    }

    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, SystemTime};

    use filetime::FileTime;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn audit11_overload_changes_do_not_collapse_into_first_identity() {
        let old =
            b"class Box {\n int run(int x) { return 1; }\n int run(String x) { return 2; }\n}\n";
        let new =
            b"class Box {\n int run(int x) { return 1; }\n int run(String x) { return 3; }\n}\n";
        let diff = symbol_diff_file(LangId::Java, Path::new("Box.java"), Some(old), Some(new));
        let methods: Vec<_> = diff
            .modified
            .iter()
            .filter(|entry| entry.name == "run")
            .collect();
        assert_eq!(methods.len(), 1);
        assert!(methods[0]
            .signature_line
            .as_ref()
            .unwrap()
            .contains("String"));
    }

    #[test]
    fn added_overload_does_not_take_a_body_changed_overloads_old_declaration() {
        let old = b"class Box {\n int run(int x) { return 1; }\n int run(String x) {\n  return 2;\n }\n}\n";
        let new = b"class Box {\n int run(int x) { return 1; }\n int run(long x) { return 4; }\n int run(String x) {\n  return 3;\n }\n}\n";
        let diff = symbol_diff_file(LangId::Java, Path::new("Box.java"), Some(old), Some(new));
        let signatures = |entries: &[SymbolDiffEntry]| -> Vec<String> {
            entries
                .iter()
                .filter(|entry| entry.name == "run")
                .map(|entry| entry.signature_line.clone().unwrap_or_default())
                .collect()
        };
        let added = signatures(&diff.added);
        let modified = signatures(&diff.modified);
        assert_eq!(added.len(), 1, "added: {added:?}");
        assert!(added[0].contains("long"), "added: {added:?}");
        assert_eq!(modified.len(), 1, "modified: {modified:?}");
        assert!(modified[0].contains("String"), "modified: {modified:?}");
        assert!(signatures(&diff.removed).is_empty());
    }

    #[test]
    fn audit11_git_blobs_share_one_process_per_range() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (temp, base, tip) = committed_range("src/lib.rs", RUST_BASE, RUST_TIP);
        BLOB_READER_SPAWNS.with(|count| count.set(0));
        let packet = symbol_diff_range(temp.path(), &base, &tip);
        assert_eq!(packet.files.len(), 1);
        assert!(!packet.files[0].diff.symbols_unavailable);
        assert_eq!(BLOB_READER_SPAWNS.with(|count| count.get()), 1);
    }

    #[cfg(unix)]
    #[test]
    fn audit11_batch_blob_reads_preserve_bytes_and_newline_path_framing() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = init_git_fixture();
        // APFS refuses names that aren't valid UTF-8 (EILSEQ), so macOS keeps
        // the newline framing check with a valid multi-byte name instead.
        #[cfg(target_os = "macos")]
        let name: &[u8] = "odd\nname\u{ff}.bin".as_bytes();
        #[cfg(not(target_os = "macos"))]
        let name: &[u8] = b"odd\nname\xff.bin";
        let bytes = b"\xef\xbb\xbfline\r\n\xfftail";
        fs::write(temp.path().join(path_from_git_bytes(name)), bytes).unwrap();
        let revision = commit_all(temp.path(), "bytes");
        let mut batch = GitBlobReader::new(temp.path());
        assert!(batch.is_some());
        assert_eq!(
            read_git_blob(temp.path(), &mut batch, &revision, name).unwrap(),
            bytes
        );
        assert!(read_git_blob(temp.path(), &mut batch, &revision, b"missing\nname").is_none());
        assert_eq!(
            read_git_blob(temp.path(), &mut batch, &revision, name).unwrap(),
            bytes
        );
    }

    const RUST_BASE: &str = r#"
pub fn unchanged() -> i32 { 0 }
pub fn body_changed() -> i32 { 1 }
pub fn signature_changed(value: i32) -> i32 { value }
pub fn removed() -> i32 { 4 }
"#;
    const RUST_TIP: &str = r#"
pub fn unchanged() -> i32 { 0 }
pub fn body_changed() -> i32 { 2 }
pub fn signature_changed(value: i64) -> i64 { value }
pub fn added() -> i32 { 3 }
"#;
    const TS_BASE: &str = r#"
export function unchanged(): number { return 0; }
export function bodyChanged(): number { return 1; }
export function signatureChanged(value: number): number { return value; }
export function removed(): number { return 4; }
"#;
    const TS_TIP: &str = r#"
export function unchanged(): number { return 0; }
export function bodyChanged(): number { return 2; }
export function signatureChanged(value: string): string { return value; }
export function added(): number { return 3; }
"#;

    #[test]
    fn rust_classifies_added_removed_and_both_kinds_of_modification() {
        let diff = symbol_diff_file(
            LangId::Rust,
            Path::new("multi.rs"),
            Some(RUST_BASE.as_bytes()),
            Some(RUST_TIP.as_bytes()),
        );

        assert!(!diff.symbols_unavailable);
        assert_eq!(entry_names(&diff.added), ["added"]);
        assert_eq!(entry_names(&diff.removed), ["removed"]);
        assert_eq!(
            entry_names(&diff.modified),
            ["body_changed", "signature_changed"]
        );
        assert_eq!(
            diff.modified[1].signature_line.as_deref(),
            Some("pub fn signature_changed(value: i64) -> i64 { value }")
        );
    }

    #[test]
    fn typescript_classifies_added_removed_and_both_kinds_of_modification() {
        let diff = symbol_diff_file(
            LangId::TypeScript,
            Path::new("multi.ts"),
            Some(TS_BASE.as_bytes()),
            Some(TS_TIP.as_bytes()),
        );

        assert!(!diff.symbols_unavailable);
        assert_eq!(entry_names(&diff.added), ["added"]);
        assert_eq!(entry_names(&diff.removed), ["removed"]);
        assert_eq!(
            entry_names(&diff.modified),
            ["bodyChanged", "signatureChanged"]
        );
        assert_eq!(
            diff.modified[1].signature_line.as_deref(),
            Some("function signatureChanged(value: string): string { return value; }")
        );
    }

    #[test]
    fn indexed_byte_offsets_match_the_scanning_contract() {
        let sources = ["", "a", "a\n", "alpha\nbeta", "α\nβ\r\nlast"];
        let columns = [0, 1, 2, 3, 8, 32];

        for source in sources {
            let line_starts = line_start_offsets(source);
            let last_line = source.bytes().filter(|byte| *byte == b'\n').count() as u32;
            for line in 0..=last_line + 2 {
                for column in columns {
                    assert_eq!(
                        byte_offset_at(source, &line_starts, line, column),
                        scanning_byte_offset_at(source, line, column),
                        "source={source:?}, line={line}, column={column}"
                    );
                }
            }
        }
    }

    #[test]
    fn json_returns_an_honest_line_count_only_fallback() {
        let diff = symbol_diff_file(
            LangId::Json,
            Path::new("data.json"),
            Some(b"{\"one\": 1}\n"),
            Some(b"{\"one\": 1}\n{\"two\": 2}\n"),
        );

        assert!(diff.symbols_unavailable);
        assert_eq!(diff.old_line_count, 1);
        assert_eq!(diff.new_line_count, 2);
        assert_eq!(diff.line_count_delta, 1);
        assert!(diff.added.is_empty());
        assert!(diff.removed.is_empty());
        assert!(diff.modified.is_empty());
    }

    #[test]
    fn range_is_stable_across_renders_and_worktree_mtime_changes() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (temp, base, tip) = committed_range("src/lib.rs", RUST_BASE, RUST_TIP);
        let first = serde_json::to_vec(&symbol_diff_range(temp.path(), &base, &tip))
            .expect("serialize first packet");
        let second = serde_json::to_vec(&symbol_diff_range(temp.path(), &base, &tip))
            .expect("serialize second packet");
        assert_eq!(first, second);

        let source = temp.path().join("src/lib.rs");
        filetime::set_file_mtime(
            &source,
            FileTime::from_system_time(SystemTime::now() + Duration::from_secs(60)),
        )
        .expect("change worktree mtime");
        let third = serde_json::to_vec(&symbol_diff_range(temp.path(), &base, &tip))
            .expect("serialize third packet");
        assert_eq!(first, third);
    }

    #[test]
    fn range_uses_nul_delimited_unicode_paths_and_expands_renames() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let temp = init_git_fixture();
        let old_name = "src/before ü name.rs";
        let new_name = "src/after ü name.rs";
        write_file(temp.path(), old_name, "pub fn renamed() {}\n");
        let base = commit_all(temp.path(), "base");
        fs::rename(temp.path().join(old_name), temp.path().join(new_name)).expect("rename fixture");
        let tip = commit_all(temp.path(), "rename");

        let packet = symbol_diff_range(temp.path(), &base, &tip);
        assert_eq!(packet.files.len(), 2);
        assert_eq!(packet.files[0].path, new_name);
        assert_eq!(packet.files[0].change, FileChangeKind::Added);
        assert_eq!(packet.files[0].renamed_from.as_deref(), Some(old_name));
        assert_eq!(entry_names(&packet.files[0].diff.added), ["renamed"]);
        assert_eq!(packet.files[1].path, old_name);
        assert_eq!(packet.files[1].change, FileChangeKind::Removed);
        assert_eq!(packet.files[1].renamed_to.as_deref(), Some(new_name));
        assert_eq!(entry_names(&packet.files[1].diff.removed), ["renamed"]);
    }

    #[test]
    fn range_packet_disclaimer_is_byte_exact_and_bare_repositories_work() {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let (temp, base, tip) = committed_range("src/lib.rs", RUST_BASE, RUST_TIP);
        let bare = tempfile::tempdir().expect("create bare parent");
        let bare_repo = bare.path().join("range.git");
        let mut command = Command::new("git");
        let status = crate::test_env::apply_hermetic_git_env(command.current_dir(temp.path()))
            .args(["clone", "--bare", "."])
            .arg(&bare_repo)
            .status()
            .expect("clone bare fixture");
        assert!(status.success(), "clone bare fixture failed");
        let packet = symbol_diff_range(&bare_repo, &base, &tip);
        let bytes = serde_json::to_vec(&packet).expect("serialize packet");

        assert_eq!(packet.disclaimer, RANGE_SYMBOL_PACKET_DISCLAIMER);
        assert!(bytes
            .windows(RANGE_SYMBOL_PACKET_DISCLAIMER.len())
            .any(|window| window == RANGE_SYMBOL_PACKET_DISCLAIMER.as_bytes()));
        assert!(!String::from_utf8(bytes)
            .expect("packet is UTF-8 JSON")
            .contains(&temp.path().display().to_string()));
    }

    fn entry_names(entries: &[SymbolDiffEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.name.as_str()).collect()
    }

    fn scanning_byte_offset_at(source: &str, line: u32, column: u32) -> Option<usize> {
        let bytes = source.as_bytes();
        let mut line_start = 0;
        for _ in 0..line {
            let newline = bytes[line_start..].iter().position(|byte| *byte == b'\n')?;
            line_start = line_start.checked_add(newline + 1)?;
        }
        let offset = line_start.checked_add(column as usize)?;
        (offset <= bytes.len()).then_some(offset)
    }

    fn committed_range(
        path: &str,
        base_source: &str,
        tip_source: &str,
    ) -> (TempDir, String, String) {
        let temp = init_git_fixture();
        write_file(temp.path(), path, base_source);
        let base = commit_all(temp.path(), "base");
        write_file(temp.path(), path, tip_source);
        let tip = commit_all(temp.path(), "tip");
        (temp, base, tip)
    }

    fn init_git_fixture() -> TempDir {
        let temp = tempfile::tempdir().expect("create git fixture");
        run_git(temp.path(), ["init"].as_slice());
        run_git(
            temp.path(),
            ["config", "user.email", "test@example.com"].as_slice(),
        );
        run_git(temp.path(), ["config", "user.name", "AFT Test"].as_slice());
        temp
    }

    fn write_file(root: &Path, relative_path: &str, content: &str) {
        let path = root.join(relative_path);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
        fs::write(path, content).expect("write fixture");
    }

    fn commit_all(root: &Path, message: &str) -> String {
        run_git(root, ["add", "."].as_slice());
        run_git(root, ["commit", "-m", message].as_slice());
        run_git(root, ["rev-parse", "HEAD"].as_slice())
    }

    fn run_git(root: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        let output = crate::test_env::apply_hermetic_git_env(command.current_dir(root))
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("git stdout is UTF-8")
            .trim()
            .to_string()
    }
}

#[cfg(test)]
thread_local! {
    static BLOB_READER_SPAWNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
