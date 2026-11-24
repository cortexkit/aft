//! Canonical observations used by the bash differential campaign.
//!
//! The adapters keep raw process output available for failure reports while
//! reducers compare structured values. No reducer is applied implicitly: the
//! corpus names every basis and every presentation normalization it permits.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ManifestKind {
    Directory,
    File,
    Symlink,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestEntry {
    pub kind: ManifestKind,
    pub size: u64,
    pub sha256: Option<String>,
    pub link_target: Option<String>,
}

pub type FilesystemManifest = BTreeMap<String, ManifestEntry>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct StructuredObservation {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i64>,
    pub entries: Vec<(String, bool)>,
    pub selected_paths: Vec<String>,
    pub matches: Vec<(String, u32, String)>,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Observation {
    pub raw_stdout: Vec<u8>,
    pub raw_stderr: Vec<u8>,
    pub exit_code: Option<i64>,
    pub structured: StructuredObservation,
    pub filesystem: FilesystemManifest,
}

pub trait ObservationAdapter {
    fn adapt(&self, stdout: &[u8], stderr: &[u8], exit_code: Option<i64>) -> Observation;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ByteObservationAdapter;

impl ObservationAdapter for ByteObservationAdapter {
    fn adapt(&self, stdout: &[u8], stderr: &[u8], exit_code: Option<i64>) -> Observation {
        Observation {
            raw_stdout: stdout.to_vec(),
            raw_stderr: stderr.to_vec(),
            exit_code,
            structured: StructuredObservation {
                stdout: stdout.to_vec(),
                stderr: stderr.to_vec(),
                exit_code,
                ..StructuredObservation::default()
            },
            filesystem: FilesystemManifest::default(),
        }
    }
}

pub fn observation_from_process(
    stdout: &[u8],
    stderr: &[u8],
    exit_code: Option<i64>,
    root: &Path,
) -> Observation {
    let mut observation = ByteObservationAdapter.adapt(stdout, stderr, exit_code);
    observation.filesystem = deterministic_filesystem_manifest(root).unwrap_or_default();
    observation
}

/// Adapt structured fields returned by an AFT command without losing the raw
/// rendered output. Unknown fields remain in `response` for report callers.
pub fn observation_from_aft_response(
    response: &Value,
    root: &Path,
    exit_code: Option<i64>,
) -> Observation {
    let output = response
        .get("output")
        .or_else(|| response.get("text"))
        .or_else(|| response.get("content"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .as_bytes()
        .to_vec();
    let mut observation = observation_from_process(&output, &[], exit_code, root);
    if let Some(entries) = response.get("entries").and_then(Value::as_array) {
        observation.structured.entries = entries
            .iter()
            .filter_map(|entry| {
                if let Some(name) = entry.as_str() {
                    Some((name.to_string(), false))
                } else {
                    let object = entry.as_object()?;
                    Some((
                        object.get("name")?.as_str()?.to_string(),
                        object
                            .get("is_dir")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    ))
                }
            })
            .collect();
    }
    if let Some(files) = response.get("files").and_then(Value::as_array) {
        let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        observation.structured.selected_paths = files
            .iter()
            .filter_map(Value::as_str)
            .map(|path| {
                let path = Path::new(path);
                let relative = path
                    .strip_prefix(root)
                    .or_else(|_| path.strip_prefix(&canonical_root))
                    .unwrap_or(path);
                relative
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
            })
            .collect();
    }
    observation
}

pub fn deterministic_filesystem_manifest(root: &Path) -> std::io::Result<FilesystemManifest> {
    let mut manifest = FilesystemManifest::new();
    if !root.exists() {
        return Ok(manifest);
    }
    walk_manifest(root, root, &mut manifest)?;
    Ok(manifest)
}

fn walk_manifest(
    root: &Path,
    current: &Path,
    manifest: &mut FilesystemManifest,
) -> std::io::Result<()> {
    let mut entries = fs::read_dir(current)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| left.file_name().cmp(&right.file_name()));
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace(std::path::MAIN_SEPARATOR, "/");
        let metadata = fs::symlink_metadata(&path)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            manifest.insert(
                relative,
                ManifestEntry {
                    kind: ManifestKind::Symlink,
                    size: metadata.len(),
                    sha256: None,
                    link_target: fs::read_link(&path)
                        .ok()
                        .map(|target| target.to_string_lossy().into_owned()),
                },
            );
        } else if file_type.is_dir() {
            manifest.insert(
                relative,
                ManifestEntry {
                    kind: ManifestKind::Directory,
                    size: 0,
                    sha256: None,
                    link_target: None,
                },
            );
            walk_manifest(root, &path, manifest)?;
        } else if file_type.is_file() {
            let bytes = fs::read(&path)?;
            let digest = Sha256::digest(&bytes);
            manifest.insert(
                relative,
                ManifestEntry {
                    kind: ManifestKind::File,
                    size: bytes.len() as u64,
                    sha256: Some(hex_digest(&digest)),
                    link_target: None,
                },
            );
        }
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn manifests_equal(left: &FilesystemManifest, right: &FilesystemManifest) -> bool {
    left == right
}

pub fn manifest_unchanged(before: &FilesystemManifest, after: &FilesystemManifest) -> bool {
    manifests_equal(before, after)
}

pub fn apply_presentation_normalizations(
    bytes: &[u8],
    normalizations: &[&str],
) -> Result<Vec<u8>, String> {
    let mut output = bytes.to_vec();
    for normalization in normalizations {
        output = match *normalization {
            "footer-removal" => remove_footer(&output),
            "gutter-removal" => remove_gutter(&output),
            other => return Err(format!("unknown presentation normalization: {other}")),
        };
    }
    Ok(output)
}

fn remove_footer(bytes: &[u8]) -> Vec<u8> {
    let had_newline = bytes.ends_with(b"\n");
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines().collect::<Vec<_>>();
    if let Some(index) = lines.iter().position(|line| {
        line.contains("Prefer `") || line.contains("DO NOT search code by running grep/rg in bash")
    }) {
        lines.truncate(index);
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
    }
    let mut output = lines.join("\n").into_bytes();
    if had_newline {
        output.push(b'\n');
    }
    output
}

fn remove_gutter(bytes: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(bytes);
    let had_newline = bytes.ends_with(b"\n")
        || text.lines().any(|line| {
            line.contains("Prefer `")
                || line.contains("DO NOT search code by running grep/rg in bash")
        });
    let mut output = text
        .lines()
        .map(|line| {
            let Some((number, content)) = line.split_once(": ") else {
                return line;
            };
            if !number.is_empty() && number.chars().all(|char| char.is_ascii_digit()) {
                content
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .into_bytes();
    if had_newline {
        output.push(b'\n');
    }
    output
}

pub fn reduce_observation(
    observation: &Observation,
    basis: &str,
    normalizations: &[&str],
) -> Result<Value, String> {
    let stdout = apply_presentation_normalizations(&observation.raw_stdout, normalizations)?;
    let stderr = apply_presentation_normalizations(&observation.raw_stderr, normalizations)?;
    let stdout_text = String::from_utf8_lossy(&stdout);
    match basis {
        "bytes" => Ok(json!({
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": observation.exit_code,
        })),
        "ls-entry-set" => {
            let mut entries = if observation.structured.entries.is_empty() {
                stdout_text
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| (line.to_string(), false))
                    .collect::<Vec<_>>()
            } else {
                observation.structured.entries.clone()
            };
            entries.sort();
            entries.dedup();
            Ok(json!(entries))
        }
        "ls-entry-sequence" => {
            let entries = if observation.structured.entries.is_empty() {
                stdout_text
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(|line| (line.to_string(), false))
                    .collect::<Vec<_>>()
            } else {
                observation.structured.entries.clone()
            };
            Ok(json!(entries))
        }
        "find-path-set" => {
            let paths = if observation.structured.selected_paths.is_empty() {
                stdout_text
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            } else {
                observation.structured.selected_paths.clone()
            };
            let paths = paths
                .into_iter()
                .map(|path| path.strip_prefix("./").unwrap_or(&path).to_string())
                .collect::<Vec<_>>();
            let mut paths = paths;
            paths.sort();
            paths.dedup();
            Ok(json!(paths))
        }
        "grep-match-set" => {
            let mut matches = if observation.structured.matches.is_empty() {
                parse_grep_matches(&stdout_text)
            } else {
                observation.structured.matches.clone()
            };
            matches.sort();
            matches.dedup();
            Ok(json!(matches))
        }
        "grep-value-multiset" => {
            let mut values = if observation.structured.values.is_empty() {
                stdout_text.lines().map(str::to_owned).collect::<Vec<_>>()
            } else {
                observation.structured.values.clone()
            };
            values.sort();
            Ok(json!(values))
        }
        other => Err(format!("unknown comparison basis: {other}")),
    }
}

fn parse_grep_matches(text: &str) -> Vec<(String, u32, String)> {
    text.lines()
        .filter_map(|line| {
            let (file, rest) = line.split_once(':')?;
            let (number, content) = rest.split_once(':')?;
            Some((file.to_string(), number.parse().ok()?, content.to_string()))
        })
        .collect()
}

/// Return a compact, JSON-safe value for failure reports without discarding
/// the exact raw bytes that the caller keeps in `Observation`.
pub fn observation_summary(observation: &Observation) -> Value {
    json!({
        "stdout": String::from_utf8_lossy(&observation.raw_stdout),
        "stderr": String::from_utf8_lossy(&observation.raw_stderr),
        "exit_code": observation.exit_code,
        "filesystem_entries": observation.filesystem.len(),
        "structured": observation.structured,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footer_and_gutter_normalizations_are_explicit() {
        let raw = b"1: alpha\nPrefer `read` tool over bash.\n";
        let normalized =
            apply_presentation_normalizations(raw, &["gutter-removal", "footer-removal"]).unwrap();
        assert_eq!(normalized, b"alpha\n");
    }

    #[test]
    fn filesystem_manifest_is_sorted_and_exact() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("z")).unwrap();
        fs::write(root.path().join("z/a"), b"a").unwrap();
        fs::write(root.path().join("b"), b"b").unwrap();
        let manifest = deterministic_filesystem_manifest(root.path()).unwrap();
        assert_eq!(manifest.keys().collect::<Vec<_>>(), vec!["b", "z", "z/a"]);
        assert!(manifest_unchanged(&manifest, &manifest));
    }

    #[test]
    fn reducers_do_not_accept_unknown_vocabularies() {
        let observation = ByteObservationAdapter.adapt(b"x\n", b"", Some(0));
        assert!(reduce_observation(&observation, "not-a-basis", &[]).is_err());
        assert!(apply_presentation_normalizations(b"x", &["bytes"]).is_err());
    }
}
