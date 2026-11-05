//! Fail-closed metadata and policy evidence for checkout reconciliation.
//! Git index tuples alone never authorize reuse of working-tree bytes.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::RelPath;
use crate::io_accounting::at;
use crate::native_stat::{FileIdentity, NativeStat};

#[cfg(test)]
thread_local! {
    static ATTRIBUTE_SOURCES: std::cell::RefCell<Option<[PathBuf; 2]>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
pub(crate) struct AttributeSourcesGuard(Option<[PathBuf; 2]>);
#[cfg(test)]
impl Drop for AttributeSourcesGuard {
    fn drop(&mut self) {
        ATTRIBUTE_SOURCES.with(|paths| *paths.borrow_mut() = self.0.take());
    }
}
#[cfg(test)]
pub(crate) fn known_attribute_sources_for_test(root: &Path) -> AttributeSourcesGuard {
    let sources = [
        root.join("absent-system-attributes"),
        root.join("absent-global-attributes"),
    ];
    assert!(sources.iter().all(|p| !p.exists()));
    install_attribute_sources_for_test(Some(sources))
}

#[cfg(test)]
pub(crate) fn attribute_sources_snapshot_for_test() -> Option<[PathBuf; 2]> {
    ATTRIBUTE_SOURCES.with(|paths| paths.borrow().clone())
}

#[cfg(test)]
pub(crate) fn install_attribute_sources_for_test(
    sources: Option<[PathBuf; 2]>,
) -> AttributeSourcesGuard {
    AttributeSourcesGuard(ATTRIBUTE_SOURCES.with(|paths| paths.replace(sources)))
}
use crate::blob_store::v2::{ContentHash, FamilyKey, FamilyPlane};

#[cfg(test)]
thread_local! {
    static POLICY_READ_FAILURE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
    static POLICY_INPUT_VISITS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

#[cfg(test)]
pub(crate) struct PolicyReadFailure(Option<PathBuf>);

#[cfg(test)]
impl Drop for PolicyReadFailure {
    fn drop(&mut self) {
        POLICY_READ_FAILURE.with(|path| *path.borrow_mut() = self.0.take());
    }
}

#[cfg(test)]
pub(crate) fn fail_policy_read_for_test(path: &Path) -> PolicyReadFailure {
    PolicyReadFailure(POLICY_READ_FAILURE.with(|current| current.replace(Some(path.to_path_buf()))))
}

#[cfg(test)]
pub(crate) fn take_policy_input_visits_for_test() -> (usize, usize) {
    POLICY_INPUT_VISITS.with(|counts| counts.replace((0, 0)))
}

fn read_policy_input(path: &Path) -> io::Result<Vec<u8>> {
    #[cfg(test)]
    if POLICY_READ_FAILURE.with(|current| current.borrow().as_deref() == Some(path)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "injected policy read failure",
        ));
    }
    fs::read(path)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Evidence {
    pub version: u32,
    pub stat: NativeStat,
    pub observed_ns: u128,
    pub content: String,
    pub size: u64,
    pub git_oid: Option<String>,
    pub policy: String,
    pub keys: BTreeMap<String, String>,
}

impl Evidence {
    pub fn content(&self) -> Option<ContentHash> {
        ContentHash::from_hex(&self.content)
    }
    pub fn key(&self, plane: FamilyPlane) -> Option<FamilyKey> {
        let name = format!("{plane:?}");
        Some(FamilyKey::new(
            plane,
            crate::blob_store::v2::parse_hex32(self.keys.get(&name)?)?,
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Policy {
    /// Names every input by its absolute path, so it is specific to this
    /// checkout. Two things compare against it: the saved file stats that let
    /// a later bind skip rereading unchanged files, and the bind receipts
    /// that record which policy a published generation was verified under.
    /// Both are only valid in the checkout that recorded them.
    pub digest: String,
    /// The policy that decides how a member's bytes are interpreted, named by
    /// role instead of location: attribute sources under the checkout by
    /// their relative path, Git's info files by name, Git configuration by
    /// its effective entries. Base identity uses this one, so a worktree at a
    /// commit another checkout already built shares that base instead of
    /// building its own.
    ///
    /// Ignore rules are left out. They only decide which files are members,
    /// and base identity already covers the membership digest: every member's
    /// path, mode, content and size, computed by each checkout under its own
    /// rules before it looks a base up. Two checkouts with the same members
    /// hold the same base content whatever rules produced that set. Leaving
    /// them in split bases on files tools write for themselves, such as a
    /// self-ignoring `.opencode/.gitignore` that only the main checkout has.
    pub base_digest: String,
    /// Whether Git is proven to store this checkout's files byte for byte: no
    /// attribute file, configured conversion or unknown attribute source can
    /// change a file between the working tree and its blob. Only evidence
    /// that comes from Git instead of from reading the file depends on it: a
    /// tree-keyed base's blob receipts, which let a bind skip reading a file
    /// whose index entry names a receipt's blob. Stats AFT recorded when it
    /// read a file stay valid under any conversion rules, because they
    /// describe the working-tree bytes AFT hashed; `digest` alone pairs them
    /// with the policy they were read under.
    pub reusable: bool,
}

pub(crate) const POLICY_QUERY_UNDETERMINED: &str = "checkout policy query not determined";

#[derive(Debug)]
struct PolicyQueryUndetermined {
    query: &'static str,
    source: io::Error,
}

impl std::fmt::Display for PolicyQueryUndetermined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{POLICY_QUERY_UNDETERMINED}: {}: {}",
            self.query, self.source
        )
    }
}

impl std::error::Error for PolicyQueryUndetermined {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn undetermined_policy_query(query: &'static str, source: io::Error) -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        PolicyQueryUndetermined { query, source },
    )
}

fn determined_policy_query(
    query: &'static str,
    output: io::Result<std::process::Output>,
) -> io::Result<std::process::Output> {
    let output = output.map_err(|source| undetermined_policy_query(query, source))?;
    if output.status.code().is_none() {
        return Err(undetermined_policy_query(
            query,
            io::Error::other(format!(
                "{}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            )),
        ));
    }
    Ok(output)
}

/// Whether a policy input only holds ignore rules: per-directory ignore
/// files and Git's `info/exclude`. These decide membership, which base
/// identity covers through the membership digest, so `Policy::base_digest`
/// leaves them out. The location-bound `Policy::digest` keeps them, because
/// saved file stats must stop being reused when the rules change.
fn is_ignore_rule_source(path: &Path) -> bool {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(".gitignore" | ".ignore" | ".aftignore") => true,
        Some("exclude") => path
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|parent| parent == "info"),
        _ => false,
    }
}

/// Reads one policy input. A missing file, or a directory or special file
/// with a policy name, has no policy bytes. Symlinks are followed through the
/// normal read path, including its errors.
fn read_policy_source(path: &Path) -> io::Result<Option<Vec<u8>>> {
    let candidate = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.is_file() || metadata.file_type().is_symlink(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(at("reading policy input metadata", path, e)),
    };
    if !candidate {
        return Ok(None);
    }
    match read_policy_input(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(at("reading policy input", path, e)),
    }
}

/// Hashes one input into the location-bound digest: its absolute name, then
/// its bytes or an absence marker.
fn metadata_source(path: &Path, hash: &mut blake3::Hasher) -> io::Result<Option<Vec<u8>>> {
    let name = path.as_os_str().as_encoded_bytes();
    hash.update(&(name.len() as u64).to_le_bytes());
    hash.update(name);
    let source = read_policy_source(path)?;
    match &source {
        Some(bytes) => {
            hash.update(&[1]);
            hash.update(&(bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
        None => {
            hash.update(&[0]);
        }
    }
    Ok(source)
}

/// Where a checkout's Git policy files live, for naming inputs by role.
struct PolicyLocations {
    root: PathBuf,
    /// The `info` directory Git reads: the common directory's for a linked
    /// worktree, otherwise the checkout's own Git directory's.
    git_info: Option<PathBuf>,
    /// A linked worktree's own `info` directory, which Git does not read.
    worktree_info: Option<PathBuf>,
    /// A linked worktree's `commondir` marker. Its bytes name a location.
    commondir: Option<PathBuf>,
}

impl PolicyLocations {
    fn of(root: &Path) -> io::Result<Self> {
        let (git_info, worktree_info, commondir) = match git_policy_dirs(root)? {
            None => (None, None, None),
            Some((gitdir, None)) => (Some(gitdir.join("info")), None, None),
            Some((gitdir, Some(common))) => (
                Some(common.join("info")),
                Some(gitdir.join("info")),
                Some(gitdir.join("commondir")),
            ),
        };
        Ok(Self {
            root: root.to_path_buf(),
            git_info,
            worktree_info,
            commondir,
        })
    }

    /// The location-independent name of `path`, or `None` for an input whose
    /// bytes only name a location. Paths are built exactly as
    /// [`policy_inputs`] builds them, so prefix matching is exact.
    fn logical_name(&self, path: &Path) -> Option<Vec<u8>> {
        if self.commondir.as_deref() == Some(path) {
            return None;
        }
        let named = |role: &[u8], rest: &Path| {
            let mut name = role.to_vec();
            name.extend_from_slice(rest.as_os_str().as_encoded_bytes());
            name
        };
        if let Some(rest) = self
            .git_info
            .as_deref()
            .and_then(|dir| path.strip_prefix(dir).ok())
        {
            return Some(named(b"git-info:", rest));
        }
        if let Some(rest) = self
            .worktree_info
            .as_deref()
            .and_then(|dir| path.strip_prefix(dir).ok())
        {
            return Some(named(b"worktree-git-info:", rest));
        }
        if let Ok(rest) = path.strip_prefix(&self.root) {
            return Some(named(b"tree:", rest));
        }
        Some(named(b"absolute:", path))
    }
}

/// The checkout's Git directory and, for a linked worktree, its common
/// directory, spelled as Git's markers give them (not canonicalized).
fn git_policy_dirs(root: &Path) -> io::Result<Option<(PathBuf, Option<PathBuf>)>> {
    let marker = root.join(".git");
    let gitdir = if marker.is_dir() {
        Some(marker)
    } else {
        fs::read(&marker)
            .ok()
            .and_then(|bytes| {
                bytes
                    .strip_prefix(b"gitdir: ")
                    .map(|p| p.trim_ascii().to_vec())
            })
            .and_then(|p| path_from_bytes(&p).ok())
            .map(|p| if p.is_absolute() { p } else { root.join(p) })
    };
    let Some(gitdir) = gitdir else {
        return Ok(None);
    };
    let common_marker = gitdir.join("commondir");
    let common = match fs::read(&common_marker) {
        Ok(bytes) => {
            let common = path_from_bytes(bytes.trim_ascii())
                .map_err(|e| at("decoding Git common directory marker", &common_marker, e))?;
            Some(if common.is_absolute() {
                common
            } else {
                gitdir.join(common)
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(at(
                "reading Git common directory marker",
                &common_marker,
                error,
            ))
        }
    };
    Ok(Some((gitdir, common)))
}

impl Policy {
    /// An unfinished query returns no policy. A completed refusal still produces
    /// non-reusable evidence, including on Git versions without attribute queries.
    pub fn capture(root: &Path, metadata: &[PathBuf]) -> io::Result<Self> {
        let queries =
            crate::git_index_reader::GitQueries::new(root.to_path_buf(), Duration::from_secs(2));
        #[cfg(test)]
        if let Some(paths) = ATTRIBUTE_SOURCES.with(|paths| paths.borrow().clone()) {
            #[cfg(unix)]
            use std::os::unix::process::ExitStatusExt;
            #[cfg(windows)]
            use std::os::windows::process::ExitStatusExt;
            return Self::capture_sources(
                root,
                metadata,
                queries.config(),
                paths.map(|path| {
                    Ok(std::process::Output {
                        status: std::process::ExitStatus::from_raw(0),
                        stdout: path.as_os_str().as_encoded_bytes().to_vec(),
                        stderr: Vec::new(),
                    })
                }),
            );
        }
        Self::capture_sources(
            root,
            metadata,
            queries.config(),
            [
                queries.attribute_source(false),
                queries.attribute_source(true),
            ],
        )
    }

    fn capture_sources(
        root: &Path,
        metadata: &[PathBuf],
        output: io::Result<std::process::Output>,
        sources: [io::Result<std::process::Output>; 2],
    ) -> io::Result<Self> {
        // An unfinished query supplies no policy evidence. A completed config
        // refusal uses empty config and requires reading checkout files again.
        let output = determined_policy_query("Git config", output)?;
        let mut hash = blake3::Hasher::new();
        hash.update(b"aft/views-v3/conversion-policy/1");
        let mut reusable = output.status.success();
        hash.update(&[u8::from(reusable)]);
        let config_read = reusable;
        let config = if config_read {
            output.stdout
        } else {
            Vec::new()
        };
        hash.update(&config);
        // Inputs for the location-independent digest: present sources by
        // logical name, and Git configuration by effective entry only. Its
        // origin records name file locations (Git prints the repository's own
        // config as `.git/config` in a main checkout and by absolute path in
        // a linked worktree), and the entries already carry their effect.
        let locations = PolicyLocations::of(root)?;
        let mut located = BTreeMap::<Vec<u8>, Vec<u8>>::new();
        let mut locate = |path: &Path, source: Option<Vec<u8>>| {
            if let (Some(name), Some(bytes)) = (locations.logical_name(path), source) {
                located.insert(name, bytes);
            }
        };
        let mut config_entries = Vec::new();
        let mut unknown_sources = Vec::new();
        for (query, output) in ["Git system attributes", "Git global attributes"]
            .into_iter()
            .zip(sources)
        {
            let output = determined_policy_query(query, output)?;
            let bytes = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
            let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
            if !output.status.success() || bytes.is_empty() || bytes.contains(&b'\n') {
                // Older Git versions and GIT_ATTR_NOSYSTEM can refuse these
                // queries. Their completed responses describe unavailable
                // provenance, not pending work, and must hash the same on retry.
                reusable = false;
                let mut unknown = blake3::Hasher::new();
                unknown.update(query.as_bytes());
                unknown.update(
                    &output
                        .status
                        .code()
                        .expect("completed Git query")
                        .to_le_bytes(),
                );
                for bytes in [&output.stdout, &output.stderr] {
                    unknown.update(&(bytes.len() as u64).to_le_bytes());
                    unknown.update(bytes);
                }
                unknown_sources.push(unknown.finalize());
                continue;
            }
            let path = path_from_bytes(bytes)
                .map_err(|e| at("decoding Git attribute source for", root, e))?;
            let path = if path.is_absolute() {
                path
            } else {
                root.join(path)
            };
            let source = metadata_source(&path, &mut hash)?;
            reusable &= source.is_none();
            locate(&path, source);
        }
        // Config --show-origin records include targets and worktree config.
        // Hash their bytes as well as the effective values, including empty files.
        for item in config.split(|b| *b == 0) {
            if let Some(path) = item.strip_prefix(b"file:") {
                let path = path_from_bytes(path)
                    .map_err(|e| at("decoding Git config origin for", root, e))?;
                metadata_source(
                    &if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    },
                    &mut hash,
                )?;
            } else if !is_config_origin(item) && !item.is_empty() && is_base_config_entry(item) {
                config_entries.push(item.to_vec());
            }
            let lower = String::from_utf8_lossy(item).to_lowercase();
            if lower.starts_with("filter.")
                || lower.starts_with("core.autocrlf\n") && lower != "core.autocrlf\nfalse"
                || lower == "core.eol\ncrlf"
            {
                reusable = false;
            }
            for (key, conversion) in [
                (b"core.attributesfile\n".as_slice(), true),
                (b"core.excludesfile\n".as_slice(), false),
            ] {
                if let Some(path) = item.strip_prefix(key) {
                    let path = path_from_bytes(path)
                        .map_err(|e| at("decoding Git policy path for", root, e))?;
                    let path = if let Some(suffix) =
                        path.to_str().and_then(|p| p.strip_prefix("~/"))
                    {
                        std::env::var_os("HOME")
                            .or_else(|| std::env::var_os("USERPROFILE"))
                            .map(|home| PathBuf::from(home).join(suffix))
                            .ok_or_else(|| {
                                at(
                                    "resolving Git policy path",
                                    &path,
                                    io::Error::other("cannot resolve Git policy home directory"),
                                )
                            })?
                    } else if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    };
                    let source = metadata_source(&path, &mut hash)?;
                    // core.excludesFile holds ignore rules, which base
                    // identity leaves out (see `Policy::base_digest`).
                    if conversion {
                        locate(&path, source);
                    }
                    reusable &= !conversion;
                }
            }
        }
        for path in metadata {
            let source = metadata_source(path, &mut hash)?;
            if source.is_some()
                && path
                    .file_name()
                    .is_some_and(|name| name == ".gitattributes" || name == "attributes")
            {
                // Until path-scoped attribute provenance is available, every
                // attribute source is conservatively an unscopable conversion.
                reusable = false;
            }
            if !is_ignore_rule_source(path) {
                locate(path, source);
            }
        }
        let mut base = blake3::Hasher::new();
        base.update(b"aft/views-v3/base-policy/2");
        base.update(&[u8::from(config_read)]);
        base.update(&(config_entries.len() as u64).to_le_bytes());
        for entry in &config_entries {
            base.update(&(entry.len() as u64).to_le_bytes());
            base.update(entry);
        }
        base.update(&(located.len() as u64).to_le_bytes());
        for (name, bytes) in &located {
            base.update(&(name.len() as u64).to_le_bytes());
            base.update(name);
            base.update(&(bytes.len() as u64).to_le_bytes());
            base.update(bytes);
        }
        for unknown in unknown_sources {
            for digest in [&mut hash, &mut base] {
                digest.update(b"unknown-attribute-source\0");
                digest.update(unknown.as_bytes());
            }
        }
        base.update(&[u8::from(reusable)]);
        Ok(Self {
            digest: hash.finalize().to_hex().to_string(),
            base_digest: base.finalize().to_hex().to_string(),
            reusable,
        })
    }
}

/// Whether a Git config entry (`key\nvalue`) can change how a member's bytes
/// or paths are read, so base identity must include it. Everything else in a
/// repository's config (branch tracking, remotes, user identity, editor
/// state) is left out: tools and agents rewrite those all the time, for
/// example on every `git push -u`, and each rewrite would otherwise give
/// every checkout of the repository a new base.
fn is_base_config_entry(item: &[u8]) -> bool {
    let key = item.split(|b| *b == b'\n').next().unwrap_or_default();
    let key = String::from_utf8_lossy(key).to_ascii_lowercase();
    key.starts_with("filter.")
        || matches!(
            key.as_str(),
            "core.autocrlf"
                | "core.eol"
                | "core.safecrlf"
                | "core.checkroundtripencoding"
                | "core.attributesfile"
                | "core.symlinks"
                | "core.ignorecase"
                | "core.precomposeunicode"
        )
}

/// Whether a `git config --list --show-origin -z` record is an origin rather
/// than an entry. Entries begin with a section name, which cannot contain a
/// colon; every origin type Git prints is a word followed by a colon.
fn is_config_origin(item: &[u8]) -> bool {
    [
        b"file:".as_slice(),
        b"blob:",
        b"standard input:",
        b"submodule-blob:",
        b"command line:",
    ]
    .iter()
    .any(|prefix| item.starts_with(prefix))
}

pub(crate) fn path_from_bytes(bytes: &[u8]) -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        Ok(std::ffi::OsString::from_vec(bytes.to_vec()).into())
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Enumerate policy inputs with metadata only. Source files are never opened.
pub(crate) fn policy_inputs(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    // Policy files below an excluded parent cannot affect checkout membership.
    // Use the membership walker so ignore negations and directory admission
    // agree, but retain all four candidate names even when a name is ignored.
    for result in crate::search_index::project_walk_builder(root).build() {
        let entry =
            result.map_err(|e| at("enumerating policy inputs in", root, io::Error::other(e)))?;
        if let Some(error) = entry.error() {
            return Err(at(
                "enumerating policy directory",
                entry.path(),
                io::Error::other(error.to_string()),
            ));
        }
        let directory = entry.file_type().is_some_and(|kind| kind.is_dir());
        #[cfg(test)]
        POLICY_INPUT_VISITS.with(|counts| {
            let (directories, entries) = counts.get();
            counts.set((
                directories + usize::from(directory),
                entries + usize::from(entry.depth() != 0),
            ));
        });
        if directory {
            for name in [".gitattributes", ".gitignore", ".ignore", ".aftignore"] {
                paths.push(entry.path().join(name));
            }
        }
    }
    if let Some((gitdir, common)) = git_policy_dirs(root)? {
        paths.push(gitdir.join("info/attributes"));
        paths.push(gitdir.join("info/exclude"));
        if let Some(common) = common {
            paths.push(common.join("info/attributes"));
            paths.push(common.join("info/exclude"));
        }
        paths.push(gitdir.join("commondir"));
    }
    paths.sort();
    Ok(paths)
}

pub(crate) fn gitdir(root: &Path) -> io::Result<PathBuf> {
    let marker = root.join(".git");
    if marker.is_dir() {
        return fs::canonicalize(&marker).map_err(|e| at("resolving Git directory", &marker, e));
    }
    let bytes = fs::read(&marker).map_err(|e| at("reading Git directory marker", &marker, e))?;
    let path = bytes.strip_prefix(b"gitdir: ").ok_or_else(|| {
        at(
            "decoding Git directory marker",
            &marker,
            io::Error::new(io::ErrorKind::InvalidData, "invalid gitdir marker"),
        )
    })?;
    let path = path_from_bytes(path.trim_ascii())
        .map_err(|e| at("decoding Git directory marker", &marker, e))?;
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    fs::canonicalize(&path).map_err(|e| at("resolving Git directory", &path, e))
}

/// Captured even for strict loads: admission waits cannot transfer an old
/// root, Git state or policy observation to a new pointer publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BindGuard {
    pub root: FileIdentity,
    pub gitdir: Option<(PathBuf, FileIdentity)>,
    pub head_tree: Option<Vec<u8>>,
    pub index: Option<(NativeStat, [u8; 32])>,
    pub policy: Policy,
    pub inputs: Vec<PathBuf>,
    pub subscription: Option<crate::watcher_backend::subscription::Continuity>,
    pub initial_coverage: Option<crate::watcher_backend::InitialCoverage>,
    /// Stable local identities and a native backend allow event-based refresh,
    /// but do not prove that all writes before a cookie reached the driver.
    pub steady_supported: bool,
}

impl BindGuard {
    pub fn capture(root: &Path, known_inputs: Option<&[PathBuf]>) -> io::Result<Self> {
        let inputs = match known_inputs {
            Some(inputs) => inputs.to_vec(),
            None => policy_inputs(root)?,
        };
        let mut policy = Policy::capture(root, &inputs)?;
        let root_stat = NativeStat::strict_read(root)?;
        let registration = crate::watcher_backend::subscription::status(root);
        let steady_supported = root_stat.reuse_supported
            && cfg!(any(target_os = "linux", target_os = "macos", windows))
            && registration
                .as_ref()
                .is_some_and(|status| status.coverage.is_complete());
        let root_identity = root_stat.identity;
        let directory = match gitdir(root) {
            Ok(path) => Some((path.clone(), NativeStat::strict_read(&path)?.identity)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e),
        };
        let mut head_tree = None;
        let mut index = None;
        if let Some((directory, _)) = &directory {
            let queries = crate::git_index_reader::GitQueries::new(
                root.to_path_buf(),
                Duration::from_secs(2),
            );
            let head = queries
                .head_tree()
                .map_err(|e| at("querying Git HEAD tree in", root, e))?;
            if head.status.success() {
                head_tree = Some(head.stdout);
            }
            let path = directory.join("index");
            match NativeStat::strict_read(&path) {
                Ok(before) => {
                    let bytes = fs::read(&path).map_err(|e| at("reading Git index", &path, e))?;
                    let after = NativeStat::strict_read(&path)?;
                    if before != after {
                        return Err(at(
                            "verifying Git index",
                            &path,
                            io::Error::other("Git index changed during verification"),
                        ));
                    }
                    index = Some((before, *blake3::hash(&bytes).as_bytes()));
                    let format = if head_tree
                        .as_ref()
                        .is_some_and(|tree| tree.trim_ascii().len() == 64)
                    {
                        crate::git_index_reader::ObjectFormat::Sha256
                    } else {
                        crate::git_index_reader::ObjectFormat::Sha1
                    };
                    match crate::git_index_reader::read_index(&path, format, Default::default()) {
                        crate::git_index_reader::IndexRead::Snapshot(snapshot) => {
                            let mut digest = blake3::Hasher::new();
                            digest.update(policy.digest.as_bytes());
                            for entry in snapshot.entries.iter().filter(|entry| {
                                entry.path.rsplit(|b| *b == b'/').next() == Some(b".gitattributes")
                            }) {
                                policy.reusable = false;
                                digest.update(&(entry.path.len() as u64).to_le_bytes());
                                digest.update(&entry.path);
                                digest.update(&entry.oid);
                            }
                            policy.digest = digest.finalize().to_hex().to_string();
                        }
                        crate::git_index_reader::IndexRead::Strict(_) => {
                            policy.reusable = false;
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(Self {
            root: root_identity,
            gitdir: directory,
            head_tree,
            index,
            policy,
            inputs,
            subscription: registration.as_ref().map(|status| status.continuity),
            initial_coverage: registration.map(|status| status.coverage),
            steady_supported,
        })
    }
    pub fn validate(&self, root: &Path) -> io::Result<()> {
        let current = Self::capture(root, None)?;
        if &current != self {
            return Err(at(
                "validating checkout",
                root,
                io::Error::other("checkout Git, root identity or conversion policy changed"),
            ));
        }
        Ok(())
    }
}

const EVIDENCE_APP: i64 = 0x41465445;
const EVIDENCE_SCHEMA: &str = "CREATE TABLE evidence_meta(singleton INTEGER PRIMARY KEY CHECK(singleton=1), format_version INTEGER NOT NULL, schema_version INTEGER NOT NULL, generation TEXT NOT NULL, root BLOB NOT NULL, policy TEXT NOT NULL);
CREATE TABLE stat_evidence(path BLOB PRIMARY KEY, record BLOB NOT NULL);";

pub(crate) fn evidence_path(dir: &Path, generation: &str) -> PathBuf {
    dir.join(format!("stat-evidence-{generation}.sqlite"))
}

/// Writes evidence while the caller holds the prepared-generation pin, before
/// the CAS. Generation and content digests are checked on read, so an
/// unpublished or obsolete sidecar supplies no proof.
pub(crate) fn write_evidence(
    route: &super::snapshot::V3Route,
    generation: &str,
    guard: &BindGuard,
    records: &BTreeMap<RelPath, Evidence>,
) -> io::Result<()> {
    let lock = crate::blob_store::v3::ReservationLock::acquire(&route.storage)
        .map_err(io::Error::other)?;
    let registry =
        crate::blob_store::v3::open_verification_registry(&route.storage, &route.family, &lock)
            .map_err(io::Error::other)?;
    drop(registry);
    drop(lock);
    let root = serde_json::to_vec(&guard.root)?;
    let dir = route.member_dir();
    let destination = evidence_path(&dir, generation);
    let staging = dir.join(format!(
        ".stat-evidence-{generation}.sqlite.tmp.{}",
        super::base_refs::fresh_nonce()
    ));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        // Generation names make this path pass Windows' MAX_PATH.
        let mut db = rusqlite::Connection::open(crate::db::lifecycle::sqlite_open_path(&staging))?;
        db.pragma_update(None, "application_id", EVIDENCE_APP)?;
        db.pragma_update(None, "user_version", 1)?;
        db.pragma_update(None, "journal_mode", "DELETE")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.execute_batch(EVIDENCE_SCHEMA)?;
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO evidence_meta VALUES(1,1,1,?1,?2,?3)",
            rusqlite::params![generation, root, guard.policy.digest],
        )?;
        for (path, record) in records {
            tx.execute(
                "INSERT INTO stat_evidence VALUES(?1,?2)",
                rusqlite::params![path.as_bytes(), serde_json::to_vec(record)?],
            )?;
        }
        tx.commit()?;
        drop(db);
        // Windows refuses FlushFileBuffers on a read-only handle, so sync
        // through the views helper, which opens the file for writing there.
        super::sync_file(&staging)?;
        if destination.exists() {
            fs::remove_file(&staging)
                .map_err(|e| at("removing evidence staging file", &staging, e))?;
        } else {
            fs::rename(&staging, &destination).map_err(|e| {
                at(
                    &format!("renaming evidence {} to", staging.display()),
                    &destination,
                    e,
                )
            })?;
        }
        super::sync_directory(&dir)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&staging);
        return Err(at(
            "writing stat evidence",
            &destination,
            io::Error::other(error.to_string()),
        ));
    }
    Ok(())
}

pub(crate) fn read_evidence(
    dir: &Path,
    generation: &str,
    guard: &BindGuard,
) -> Option<BTreeMap<RelPath, Evidence>> {
    let db = rusqlite::Connection::open_with_flags(
        crate::db::lifecycle::sqlite_open_path(&evidence_path(dir, generation)),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    if db
        .pragma_query_value::<i64, _>(None, "application_id", |r| r.get(0))
        .ok()?
        != EVIDENCE_APP
        || db
            .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .ok()?
            != 1
    {
        return None;
    }
    let (format, schema, observed_generation, root, policy): (u32, u32, String, Vec<u8>, String) = db.query_row(
        "SELECT format_version,schema_version,generation,root,policy FROM evidence_meta WHERE singleton=1", [],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
    ).ok()?;
    if (format, schema) != (1, 1)
        || observed_generation != generation
        || root != serde_json::to_vec(&guard.root).ok()?
        || policy != guard.policy.digest
    {
        return None;
    }
    let mut statement = db
        .prepare("SELECT path,record FROM stat_evidence ORDER BY path")
        .ok()?;
    let rows = statement
        .query_map([], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .ok()?;
    let mut result = BTreeMap::new();
    for row in rows {
        let (path, bytes) = row.ok()?;
        let record: Evidence = serde_json::from_slice(&bytes).ok()?;
        if record.version != 1 || record.policy != guard.policy.digest {
            return None;
        }
        result.insert(RelPath::new(path).ok()?, record);
    }
    Some(result)
}

/// SQLite application ID of a tree-keyed base's proof file ("AFTT").
pub(crate) const TREE_PROOF_APP: i64 = 0x41465454;
/// The proof file's name inside a tree-keyed base's member directory.
pub(crate) const TREE_PROOF_FILE: &str = "tree-proof.sqlite";
const TREE_PROOF_SCHEMA: &str = "CREATE TABLE proof_meta(singleton INTEGER PRIMARY KEY CHECK(singleton=1), format_version INTEGER NOT NULL, schema_version INTEGER NOT NULL, base_scope TEXT NOT NULL, identity_digest TEXT NOT NULL, membership_digest TEXT NOT NULL, object_format TEXT NOT NULL, head_tree TEXT NOT NULL, root BLOB NOT NULL, policy TEXT NOT NULL, receipts_digest TEXT NOT NULL);
CREATE TABLE blob_receipts(path BLOB PRIMARY KEY, record BLOB NOT NULL);";
const TREE_RECEIPTS_TAG: &[u8] = b"aft/views-v3/tree-receipts/1";

/// What one stable read of a tree-keyed base member observed: the bytes'
/// content digest and Git blob OID, the native stat taken around the read,
/// and the conversion policy in force at that read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct BlobReceipt {
    pub version: u32,
    pub mode: u32,
    pub size: u64,
    pub content: String,
    pub git_oid: String,
    pub stat: NativeStat,
    pub observed_ns: u128,
    pub policy: String,
    pub delivery_seq: u64,
}

/// The durable proof of one tree-keyed base.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TreeProof {
    pub base_scope: String,
    pub identity_digest: String,
    pub membership_digest: String,
    pub object_format: String,
    pub head_tree: String,
    pub root: FileIdentity,
    pub policy: String,
    pub receipts: BTreeMap<RelPath, BlobReceipt>,
}

fn receipts_digest<'a>(records: impl ExactSizeIterator<Item = (&'a [u8], &'a [u8])>) -> String {
    let mut hash = blake3::Hasher::new();
    let field = |hash: &mut blake3::Hasher, bytes: &[u8]| {
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(bytes);
    };
    field(&mut hash, TREE_RECEIPTS_TAG);
    hash.update(&(records.len() as u64).to_le_bytes());
    for (path, record) in records {
        field(&mut hash, path);
        field(&mut hash, record);
    }
    hash.finalize().to_hex().to_string()
}

/// Writes `proof` into `dir` (the base's member directory). Only the build
/// claim owner calls this, before the base's first publication; afterwards
/// the file is never rewritten. The registry floor is raised to the
/// verification version first, so readers without that support refuse.
pub(crate) fn write_tree_proof(
    storage: &Path,
    family: &str,
    dir: &Path,
    proof: &TreeProof,
) -> io::Result<()> {
    let lock =
        crate::blob_store::v3::ReservationLock::acquire(storage).map_err(io::Error::other)?;
    drop(
        crate::blob_store::v3::open_verification_registry(storage, family, &lock)
            .map_err(io::Error::other)?,
    );
    drop(lock);
    let mut encoded = BTreeMap::new();
    for (path, receipt) in &proof.receipts {
        encoded.insert(path.as_bytes().to_vec(), serde_json::to_vec(receipt)?);
    }
    let digest = receipts_digest(
        encoded
            .iter()
            .map(|(path, record)| (path.as_slice(), record.as_slice())),
    );
    let root = serde_json::to_vec(&proof.root)?;
    let destination = dir.join(TREE_PROOF_FILE);
    let staging = dir.join(format!(
        ".{TREE_PROOF_FILE}.tmp.{}",
        super::base_refs::fresh_nonce()
    ));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut db = rusqlite::Connection::open(crate::db::lifecycle::sqlite_open_path(&staging))?;
        db.pragma_update(None, "application_id", TREE_PROOF_APP)?;
        db.pragma_update(None, "user_version", 1)?;
        db.pragma_update(None, "journal_mode", "DELETE")?;
        db.pragma_update(None, "synchronous", "FULL")?;
        db.execute_batch(TREE_PROOF_SCHEMA)?;
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO proof_meta VALUES(1,1,1,?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                proof.base_scope,
                proof.identity_digest,
                proof.membership_digest,
                proof.object_format,
                proof.head_tree,
                root,
                proof.policy,
                digest
            ],
        )?;
        for (path, record) in &encoded {
            tx.execute(
                "INSERT INTO blob_receipts VALUES(?1,?2)",
                rusqlite::params![path, record],
            )?;
        }
        tx.commit()?;
        drop(db);
        // Windows refuses FlushFileBuffers on a read-only handle, so sync
        // through the views helper, which opens the file for writing there.
        super::sync_file(&staging)?;
        fs::rename(&staging, &destination).map_err(|e| {
            at(
                &format!("renaming tree proof {} to", staging.display()),
                &destination,
                e,
            )
        })?;
        super::sync_directory(dir)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&staging);
        return Err(at(
            "writing tree proof",
            &destination,
            io::Error::other(error.to_string()),
        ));
    }
    Ok(())
}

/// Reads the proof in `dir`. `None` for a missing, foreign, newer, corrupt or
/// internally inconsistent file: the caller then takes the full check.
pub(crate) fn read_tree_proof(dir: &Path) -> Option<TreeProof> {
    let db = rusqlite::Connection::open_with_flags(
        crate::db::lifecycle::sqlite_open_path(&dir.join(TREE_PROOF_FILE)),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    if db
        .pragma_query_value::<i64, _>(None, "application_id", |r| r.get(0))
        .ok()?
        != TREE_PROOF_APP
        || db
            .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .ok()?
            != 1
    {
        return None;
    }
    type Meta = (
        u32,
        u32,
        String,
        String,
        String,
        String,
        String,
        Vec<u8>,
        String,
        String,
    );
    let meta: Meta = db
        .query_row(
            "SELECT format_version,schema_version,base_scope,identity_digest,membership_digest,object_format,head_tree,root,policy,receipts_digest FROM proof_meta WHERE singleton=1",
            [],
            |r| {
                Ok((
                    r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?,
                    r.get(5)?, r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?,
                ))
            },
        )
        .ok()?;
    let (
        format,
        schema,
        base_scope,
        identity_digest,
        membership_digest,
        object_format,
        head_tree,
        root,
        policy,
        digest,
    ) = meta;
    if (format, schema) != (1, 1) {
        return None;
    }
    let mut statement = db
        .prepare("SELECT path,record FROM blob_receipts ORDER BY path")
        .ok()?;
    let rows = statement
        .query_map([], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?))
        })
        .ok()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .ok()?;
    if receipts_digest(
        rows.iter()
            .map(|(path, record)| (path.as_slice(), record.as_slice())),
    ) != digest
    {
        return None;
    }
    let mut receipts = BTreeMap::new();
    for (path, record) in rows {
        let receipt: BlobReceipt = serde_json::from_slice(&record).ok()?;
        if receipt.version != 1 || receipt.policy != policy {
            return None;
        }
        receipts.insert(RelPath::new(path).ok()?, receipt);
    }
    Some(TreeProof {
        base_scope,
        identity_digest,
        membership_digest,
        object_format,
        head_tree,
        root: serde_json::from_slice(&root).ok()?,
        policy,
        receipts,
    })
}

/// Stat-evidence candidates from a tree-keyed base's blob receipts, or `None`
/// when the base is membership-keyed or its proof does not hold now (every
/// path then takes the full check).
///
/// The proof must match the authenticated base, the current root and the
/// current reusable policy. Git must still agree: `rev-parse HEAD^{tree}`
/// (taken by the guard) names the base's tree, and a coherent index read of
/// the same index file the guard observed computes that tree. A receipt is
/// offered only for a path whose stage-0 index entry has no exception flag
/// and the receipt's mode and OID. Callers still apply the stat-evidence
/// reuse rules to every candidate before skipping a read.
pub(crate) fn tree_receipt_evidence(
    base: &super::base_refs::AuthenticatedBase,
    guard: &BindGuard,
) -> Option<BTreeMap<RelPath, Evidence>> {
    use crate::blob_store::v2::to_hex;
    use crate::git_index_reader::{index_tree, read_index, IndexRead, ObjectFormat};
    if base.identity().kind() != super::base_refs::BaseKind::Tree || !guard.policy.reusable {
        return None;
    }
    let manifest = base.manifest();
    let head_tree = manifest.header().head_tree.as_deref()?;
    let format = ObjectFormat::from_hex_len(head_tree.len())?;
    let proof = read_tree_proof(base.view_dir())?;
    if proof.base_scope != base.scope()
        || proof.identity_digest != to_hex(&base.identity().identity)
        || proof.membership_digest != to_hex(&base.identity().membership)
        || proof.object_format != format.name()
        || proof.head_tree != head_tree
        || proof.root != guard.root
        || proof.policy != guard.policy.digest
        || proof.receipts.len() != manifest.len()
    {
        return None;
    }
    if guard.head_tree.as_deref()?.trim_ascii() != head_tree.as_bytes() {
        return None;
    }
    let index_path = guard.gitdir.as_ref()?.0.join("index");
    let IndexRead::Snapshot(snapshot) = read_index(&index_path, format, Default::default()) else {
        return None;
    };
    // The snapshot must come from the index file the guard recorded, which
    // final validation re-checks before publication.
    if NativeStat::strict_read(&index_path).ok()? != guard.index.as_ref()?.0
        || to_hex(&index_tree(&snapshot).ok()?) != head_tree
    {
        return None;
    }
    let index: BTreeMap<&[u8], &crate::git_index_reader::IndexEntry> = snapshot
        .entries
        .iter()
        .map(|entry| (entry.path.as_slice(), entry))
        .collect();
    let mut candidates = BTreeMap::new();
    for (path, entry) in manifest.entries() {
        let super::manifest_v2::EntryV2::Regular {
            mode,
            content,
            size,
            git_oid: Some(git_oid),
            ..
        } = entry
        else {
            return None;
        };
        let receipt = proof.receipts.get(path)?;
        if receipt.mode != *mode
            || receipt.size != *size
            || receipt.content != content.to_hex()
            || &receipt.git_oid != git_oid
        {
            return None;
        }
        let Some(current) = index.get(path.as_bytes()) else {
            continue;
        };
        if !current.exceptions().is_empty()
            || current.stat.mode != *mode
            || to_hex(&current.oid) != *git_oid
        {
            continue;
        }
        candidates.insert(
            path.clone(),
            Evidence {
                version: 1,
                stat: receipt.stat.clone(),
                observed_ns: receipt.observed_ns,
                content: receipt.content.clone(),
                size: receipt.size,
                git_oid: Some(receipt.git_oid.clone()),
                policy: receipt.policy.clone(),
                keys: BTreeMap::new(),
            },
        );
    }
    Some(candidates)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_stat::now_ns;

    fn success(bytes: &[u8]) -> io::Result<std::process::Output> {
        #[cfg(unix)]
        use std::os::unix::process::ExitStatusExt;
        #[cfg(windows)]
        use std::os::windows::process::ExitStatusExt;
        Ok(std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: bytes.to_vec(),
            stderr: Vec::new(),
        })
    }

    fn refused(code: u8, bytes: &[u8]) -> io::Result<std::process::Output> {
        let mut output = success(bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            output.status = std::process::ExitStatus::from_raw(i32::from(code) << 8);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::ExitStatusExt;
            output.status = std::process::ExitStatus::from_raw(u32::from(code));
        }
        output.stderr = b"unsupported Git source query".to_vec();
        Ok(output)
    }

    #[test]
    fn conversion_or_unknown_attribute_sources_disable_stat_provenance() {
        let root = tempfile::tempdir().unwrap();
        let sources = || {
            [
                success(
                    root.path()
                        .join("system-attributes")
                        .as_os_str()
                        .as_encoded_bytes(),
                ),
                success(
                    root.path()
                        .join("global-attributes")
                        .as_os_str()
                        .as_encoded_bytes(),
                ),
            ]
        };
        assert!(
            Policy::capture_sources(root.path(), &[], success(b""), sources())
                .unwrap()
                .reusable
        );
        for config in [
            b"core.autocrlf\ntrue\0".as_slice(),
            b"core.eol\ncrlf\0",
            b"filter.custom.clean\ncustom-driver\0",
        ] {
            assert!(
                !Policy::capture_sources(root.path(), &[], success(config), sources())
                    .unwrap()
                    .reusable
            );
        }
        assert!(
            !Policy::capture_sources(
                root.path(),
                &[],
                success(b""),
                [refused(129, b""), success(b"unused")],
            )
            .unwrap()
            .reusable
        );
        let attribute = root.path().join(".gitattributes");
        fs::write(&attribute, b"*.rs text\n").unwrap();
        assert!(
            !Policy::capture_sources(root.path(), &[attribute], success(b""), sources())
                .unwrap()
                .reusable
        );
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn policy_inputs_prunes_large_ignored_tree() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        fs::write(root.path().join(".gitignore"), b"excluded/\n").unwrap();
        fs::create_dir(root.path().join("visible")).unwrap();
        fs::write(root.path().join("visible/source.rs"), b"fn visible() {}\n").unwrap();
        for directory in 0..128 {
            let path = root
                .path()
                .join("excluded")
                .join(format!("child-{directory}"));
            fs::create_dir_all(&path).unwrap();
            for file in 0..16 {
                fs::write(path.join(format!("file-{file}.rs")), b"fn excluded() {}\n").unwrap();
            }
        }
        take_policy_input_visits_for_test();
        let paths = policy_inputs(root.path()).unwrap();
        let (directories, entries) = take_policy_input_visits_for_test();
        println!("policy_inputs visits: {directories} directories, {entries} entries");
        assert_eq!((directories, entries), (2, 3));
        assert!(paths.contains(&root.path().join("visible").join(".gitignore")));
        assert!(!paths
            .iter()
            .any(|path| path.starts_with(root.path().join("excluded"))));
    }

    #[test]
    fn nonregular_policy_names_are_absent_but_regular_files_still_contribute() {
        let root = tempfile::tempdir().unwrap();
        for name in [".ignore", ".gitignore", ".gitattributes", ".aftignore"] {
            let path = root.path().join(name);
            let mut absent = blake3::Hasher::new();
            assert!(metadata_source(&path, &mut absent).unwrap().is_none());
            fs::create_dir(&path).unwrap();
            let mut directory = blake3::Hasher::new();
            assert!(metadata_source(&path, &mut directory).unwrap().is_none());
            assert_eq!(directory.finalize(), absent.finalize());
            fs::remove_dir(&path).unwrap();
            fs::write(&path, b"ignored.rs\n").unwrap();
            let mut regular = blake3::Hasher::new();
            assert!(metadata_source(&path, &mut regular).unwrap().is_some());
            assert_ne!(regular.finalize(), absent.finalize());
        }
    }

    #[cfg(unix)]
    #[test]
    fn fifo_and_socket_policy_names_are_absent_without_opening_them() {
        use std::os::unix::ffi::OsStrExt;
        let root = tempfile::tempdir().unwrap();
        let fifo = root.path().join(".ignore");
        let socket = root.path().join(".gitignore");
        let mut absent_fifo = blake3::Hasher::new();
        let mut absent_socket = blake3::Hasher::new();
        assert!(metadata_source(&fifo, &mut absent_fifo).unwrap().is_none());
        assert!(metadata_source(&socket, &mut absent_socket)
            .unwrap()
            .is_none());
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // The path is NUL-terminated and the FIFO remains inside the fixture.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut fifo_hash = blake3::Hasher::new();
        let mut socket_hash = blake3::Hasher::new();
        assert!(metadata_source(&fifo, &mut fifo_hash).unwrap().is_none());
        assert!(metadata_source(&socket, &mut socket_hash)
            .unwrap()
            .is_none());
        assert_eq!(fifo_hash.finalize(), absent_fifo.finalize());
        assert_eq!(socket_hash.finalize(), absent_socket.finalize());
    }

    #[cfg(unix)]
    #[test]
    fn policy_symlinks_keep_their_existing_follow_and_error_behavior() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let link = root.path().join(".ignore");
        fs::write(&target, b"ignored.rs\n").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(metadata_source(&link, &mut blake3::Hasher::new())
            .unwrap()
            .is_some());
        fs::remove_file(&target).unwrap();
        assert!(metadata_source(&link, &mut blake3::Hasher::new())
            .unwrap()
            .is_none());
        fs::create_dir(&target).unwrap();
        let error = metadata_source(&link, &mut blake3::Hasher::new()).unwrap_err();
        assert!(error.to_string().contains("reading policy input"));
        assert!(error.to_string().contains(&link.display().to_string()));
    }

    #[test]
    fn failed_policy_queries_do_not_produce_evidence() {
        let root = tempfile::tempdir().unwrap();
        let expect_undetermined = |query: usize, failure: io::Result<std::process::Output>| {
            let mut outputs = [
                success(b""),
                success(b"system-attributes"),
                success(b"global-attributes"),
            ];
            outputs[query] = failure;
            let [config, system, global] = outputs;
            let error =
                Policy::capture_sources(root.path(), &[], config, [system, global]).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::WouldBlock, "{query}: {error}");
            assert!(error.get_ref().unwrap().is::<PolicyQueryUndetermined>());
            assert!(
                error.to_string().contains(POLICY_QUERY_UNDETERMINED),
                "{error}"
            );
            assert!(
                super::super::trigram::bind_is_busy(&error.to_string()),
                "{error}"
            );
        };
        for query in 0..3 {
            expect_undetermined(
                query,
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "metadata deadline exceeded",
                )),
            );
            expect_undetermined(
                query,
                Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "could not spawn Git",
                )),
            );
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                let mut killed = success(b"").unwrap();
                killed.status = std::process::ExitStatus::from_raw(libc::SIGTERM);
                assert!(killed.status.code().is_none());
                expect_undetermined(query, Ok(killed));
            }
        }
    }

    #[test]
    fn completed_attribute_refusals_and_missing_paths_are_stable_non_reusable_evidence() {
        let root = tempfile::tempdir().unwrap();
        let healthy = || [success(b"system-attributes"), success(b"global-attributes")];
        let known = Policy::capture_sources(root.path(), &[], success(b""), healthy()).unwrap();
        let mut fingerprints = std::collections::BTreeSet::new();
        for query in 0..2 {
            for response in 0..4 {
                let capture = || {
                    let mut sources = healthy();
                    sources[query] = match response {
                        0 => refused(1, b""),
                        1 => refused(129, b"usage: git var"),
                        2 => success(b""),
                        _ => success(b"first\nsecond\n"),
                    };
                    Policy::capture_sources(root.path(), &[], success(b""), sources).unwrap()
                };
                let before = capture();
                assert!(!before.reusable);
                assert_eq!(before, capture());
                assert_ne!(before.digest, known.digest);
                assert_ne!(before.base_digest, known.base_digest);
                assert!(fingerprints.insert(before.digest));
            }
        }
    }

    #[test]
    fn completed_config_refusal_is_empty_but_still_hashes_known_policy_sources() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join(".gitattributes");
        fs::write(&source, b"*.rs text\n").unwrap();
        let capture = |output| {
            Policy::capture_sources(
                root.path(),
                std::slice::from_ref(&source),
                output,
                [success(b"system-attributes"), success(b"global-attributes")],
            )
            .unwrap()
        };
        let before = capture(refused(1, b"filter.custom.clean\nignored-output\0"));
        assert!(!before.reusable);
        assert_eq!(before, capture(refused(129, b"")));
        fs::write(&source, b"*.rs filter=custom\n").unwrap();
        let after = capture(refused(1, b""));
        assert!(!after.reusable);
        assert_ne!(before.digest, after.digest);
        assert_ne!(before.base_digest, after.base_digest);
    }

    #[test]
    fn completed_attribute_refusal_from_real_git_is_stable_policy_evidence() {
        let root = tempfile::tempdir().unwrap();
        let queries = crate::git_index_reader::GitQueries::new(
            root.path().to_owned(),
            Duration::from_secs(2),
        );
        let capture = || {
            let sources = ["GIT_ATTR_SYSTEM", "GIT_ATTR_GLOBAL"].map(|variable| {
                let mut command = std::process::Command::new("git");
                command
                    .current_dir(root.path())
                    .args(["var", variable])
                    .env("GIT_ATTR_NOSYSTEM", "1")
                    .env("LC_ALL", "C");
                crate::test_env::apply_hermetic_git_env(&mut command);
                let output = command.output().unwrap();
                if variable == "GIT_ATTR_SYSTEM" {
                    assert!(output.status.code().is_some(), "{output:?}");
                    assert!(
                        !output.status.success(),
                        "the test must exercise a completed refusal: {output:?}"
                    );
                }
                Ok(output)
            });
            Policy::capture_sources(root.path(), &[], queries.config(), sources)
                .expect("a completed attribute refusal is not an unfinished query")
        };
        let before = capture();
        let after = capture();
        assert!(!before.reusable);
        assert_eq!(before, after);
    }

    #[test]
    fn final_policy_digest_covers_git_stat_settings_and_include_targets() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        // `git init` already writes some of these keys, with values that
        // depend on the platform (Git for Windows records
        // `core.ignorecase=true` and `core.fileMode=false`). Setting a key to
        // the value it already has is no change at all, so each key is moved
        // to whichever candidate differs from its current value.
        for (key, values) in [
            ("core.autocrlf", ["true", "false"]),
            ("core.eol", ["crlf", "lf"]),
            ("core.symlinks", ["false", "true"]),
            ("core.ignorecase", ["true", "false"]),
            ("core.checkStat", ["minimal", "default"]),
            ("core.trustctime", ["false", "true"]),
            ("core.fileMode", ["false", "true"]),
        ] {
            let current = std::process::Command::new("git")
                .current_dir(root.path())
                .args(["config", "--get", key])
                .output()
                .unwrap();
            let current = String::from_utf8_lossy(&current.stdout)
                .trim()
                .to_lowercase();
            let value = values.into_iter().find(|value| *value != current).unwrap();
            let before = BindGuard::capture(root.path(), None).unwrap();
            git(root.path(), &["config", key, value]);
            assert!(before.validate(root.path()).is_err(), "{key}");
        }
        let external = tempfile::tempdir().unwrap();
        let include = external.path().join("included.config");
        fs::write(&include, b"[core]\n eol = lf\n").unwrap();
        git(
            root.path(),
            &["config", "include.path", include.to_str().unwrap()],
        );
        let before = BindGuard::capture(root.path(), None).unwrap();
        fs::write(&include, b"[core]\n eol = crlf\n").unwrap();
        assert!(before.validate(root.path()).is_err());
    }

    #[test]
    fn stat_evidence_is_generation_root_policy_and_version_paired() {
        let _serial = crate::blob_store::v3::serial_for_test();
        let storage = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file.rs");
        fs::write(&file, b"stable").unwrap();
        let guard = BindGuard::capture(root.path(), None).unwrap();
        let route = super::super::snapshot::V3Route {
            storage: storage.path().to_owned(),
            family: "evidence".into(),
            scope: "worker".into(),
        };
        fs::create_dir_all(route.member_dir()).unwrap();
        let path = RelPath::new(b"file.rs".to_vec()).unwrap();
        let content = ContentHash::of(b"stable");
        let record = Evidence {
            version: 1,
            stat: NativeStat::read(&file).unwrap(),
            observed_ns: now_ns(),
            content: content.to_hex(),
            size: 6,
            git_oid: None,
            policy: guard.policy.digest.clone(),
            keys: BTreeMap::from([("Trigram".into(), "ab".repeat(32))]),
        };
        write_evidence(
            &route,
            "g-one",
            &guard,
            &BTreeMap::from([(path.clone(), record.clone())]),
        )
        .unwrap();
        let restored = read_evidence(&route.member_dir(), "g-one", &guard).unwrap();
        assert_eq!(restored[&path].content(), Some(content));
        assert_eq!(
            restored[&path].key(FamilyPlane::Trigram).unwrap().to_hex(),
            "ab".repeat(32)
        );
        assert!(restored[&path].git_oid.is_none());
        assert_eq!(
            crate::reader_floor::read(storage.path())
                .unwrap()
                .unwrap()
                .stores["views_v3_registry"],
            3
        );
        let mut wrong = guard.clone();
        wrong.policy.digest = "changed".into();
        assert!(read_evidence(&route.member_dir(), "g-one", &wrong).is_none());
        wrong = guard.clone();
        wrong.root = FileIdentity::Windows {
            volume: 1,
            id: [2; 16],
            creation: 3,
        };
        assert!(read_evidence(&route.member_dir(), "g-one", &wrong).is_none());
        fs::copy(
            evidence_path(&route.member_dir(), "g-one"),
            evidence_path(&route.member_dir(), "g-two"),
        )
        .unwrap();
        assert!(read_evidence(&route.member_dir(), "g-two", &guard).is_none());
        let db = rusqlite::Connection::open(evidence_path(&route.member_dir(), "g-one")).unwrap();
        db.pragma_update(None, "user_version", 2).unwrap();
        assert!(read_evidence(&route.member_dir(), "g-one", &guard).is_none());
        db.pragma_update(None, "user_version", 1).unwrap();
        db.pragma_update(None, "application_id", 0x1234).unwrap();
        assert!(read_evidence(&route.member_dir(), "g-one", &guard).is_none());
    }

    /// Branch tracking, remotes and user settings never change how a member
    /// is read, so rewriting them (as `git push -u` does) keeps the base
    /// identity; a line-ending or filter setting changes it.
    #[test]
    fn base_identity_ignores_config_that_cannot_change_member_bytes() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        fs::write(root.path().join("file.rs"), b"fn main() {}\n").unwrap();
        git(root.path(), &["add", "."]);
        let base = |root: &Path| BindGuard::capture(root, None).unwrap().policy.base_digest;
        let before = base(root.path());
        for (key, value) in [
            ("branch.feature.remote", "origin"),
            ("branch.feature.merge", "refs/heads/feature"),
            ("remote.origin.url", "https://example.invalid/repo.git"),
            ("user.name", "someone else"),
            ("rerere.enabled", "true"),
        ] {
            git(root.path(), &["config", key, value]);
        }
        assert_eq!(
            base(root.path()),
            before,
            "branch, remote and user settings must not change the base identity"
        );
        git(root.path(), &["config", "core.autocrlf", "input"]);
        let line_endings = base(root.path());
        assert_ne!(
            line_endings, before,
            "a line-ending setting changes the base"
        );
        git(root.path(), &["config", "filter.demo.clean", "cat"]);
        assert_ne!(
            base(root.path()),
            line_endings,
            "a filter setting changes the base"
        );
    }

    #[test]
    fn final_revalidation_refuses_git_index_head_root_and_policy_changes() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["config", "user.name", "test"]);
        git(
            root.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(root.path().join("file.rs"), b"initial").unwrap();
        git(root.path(), &["add", "."]);
        git(root.path(), &["commit", "-qm", "first"]);
        let guard = BindGuard::capture(root.path(), None).unwrap();
        guard.validate(root.path()).unwrap();
        fs::write(root.path().join("file.rs"), b"staged").unwrap();
        git(root.path(), &["add", "."]);
        assert!(guard.validate(root.path()).is_err());
        let staged = BindGuard::capture(root.path(), None).unwrap();
        git(root.path(), &["commit", "-qm", "second"]);
        assert!(staged.validate(root.path()).is_err());
        let committed = BindGuard::capture(root.path(), None).unwrap();
        git(root.path(), &["config", "core.autocrlf", "true"]);
        assert!(committed.validate(root.path()).is_err());
        assert!(
            !BindGuard::capture(root.path(), None)
                .unwrap()
                .policy
                .reusable
        );
        git(root.path(), &["config", "core.autocrlf", "false"]);
        let before_attribute = BindGuard::capture(root.path(), None).unwrap();
        fs::write(root.path().join(".gitattributes"), b"*.rs text\n").unwrap();
        assert!(before_attribute.validate(root.path()).is_err());
        assert!(
            !BindGuard::capture(root.path(), None)
                .unwrap()
                .policy
                .reusable
        );
        let replacement = tempfile::tempdir().unwrap();
        assert!(committed.validate(replacement.path()).is_err());
    }
}
