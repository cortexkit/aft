//! Fail-closed metadata and policy evidence for checkout reconciliation.
//! Git index tuples alone never authorize reuse of working-tree bytes.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::RelPath;

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
    AttributeSourcesGuard(ATTRIBUTE_SOURCES.with(|paths| paths.replace(Some(sources))))
}
use crate::blob_store::v2::{ContentHash, FamilyKey, FamilyPlane};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum FileIdentity {
    Unix {
        dev: u64,
        ino: u64,
        birth: Option<u128>,
    },
    Windows {
        volume: u64,
        id: [u8; 16],
        creation: i64,
    },
    StrictOnly {
        volume: u64,
        index: u64,
        birth: Option<u128>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct NativeStat {
    pub identity: FileIdentity,
    pub size: u64,
    pub modified_ns: i128,
    pub changed_ns: i128,
    pub mode: u32,
    pub reuse_supported: bool,
}

impl NativeStat {
    pub fn read(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m = fs::symlink_metadata(path)?;
            Ok(Self {
                identity: FileIdentity::Unix {
                    dev: m.dev(),
                    ino: m.ino(),
                    birth: m
                        .created()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_nanos()),
                },
                size: m.len(),
                modified_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
                changed_ns: i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()),
                mode: m.mode(),
                reuse_supported: local_precise_filesystem(path),
            })
        }
        #[cfg(windows)]
        {
            windows_stat(path)
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = path;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "stable file identity unavailable",
            ))
        }
    }
    pub fn reusable(&self, current: &Self, observed_ns: u128) -> bool {
        self.reuse_supported
            && self == current
            && self.modified_ns >= 0
            && (self.modified_ns as u128) < observed_ns
    }

    pub fn strict_read(path: &Path) -> io::Result<Self> {
        match Self::read(path) {
            Ok(stat) => Ok(stat),
            Err(error) => {
                let (metadata, identity) = crate::file_identity::symlink_metadata(path)?;
                let crate::file_identity::FileIdentity::Known { volume, index } = identity else {
                    return Err(error);
                };
                Ok(Self {
                    identity: FileIdentity::StrictOnly {
                        volume,
                        index,
                        birth: metadata
                            .created()
                            .ok()
                            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                            .map(|d| d.as_nanos()),
                    },
                    size: metadata.len(),
                    modified_ns: metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map_or(-1, |d| d.as_nanos() as i128),
                    changed_ns: -1,
                    mode: 0,
                    reuse_supported: false,
                })
            }
        }
    }
}

#[cfg(unix)]
fn local_precise_filesystem(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(path.as_ptr(), &mut stat) } != 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        matches!(
            stat.f_type as u64,
            0xef53 | 0x9123683e | 0x58465342 | 0x794c7630 | 0x01021994 | 0x2fc12fc1
        )
    }
    #[cfg(target_os = "macos")]
    {
        unsafe { std::ffi::CStr::from_ptr(stat.f_fstypename.as_ptr()) }.to_bytes() == b"apfs"
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        false
    }
}

#[cfg(windows)]
fn windows_stat(path: &Path) -> io::Result<NativeStat> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        FileBasicInfo, FileIdInfo, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
        FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        FILE_READ_ATTRIBUTES,
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(7)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let handle = file.as_raw_handle();
    let mut id: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    let mut filesystem = [0u16; 32];
    // All fields come from one metadata-only handle. A 64-bit file index is
    // insufficient on ReFS, and creation time is not a substitute for ChangeTime.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            handle,
            FileIdInfo,
            &mut id as *mut _ as _,
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        ) != 0
            && GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                &mut basic as *mut _ as _,
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            ) != 0
            && GetVolumeInformationByHandleW(
                handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            ) != 0
    };
    if !ok {
        return Err(io::Error::last_os_error());
    }
    let end = filesystem
        .iter()
        .position(|c| *c == 0)
        .unwrap_or(filesystem.len());
    let name = String::from_utf16_lossy(&filesystem[..end]);
    if !matches!(name.as_str(), "NTFS" | "ReFS") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "stat reuse requires NTFS or ReFS",
        ));
    }
    let unix_ns = |ticks: i64| (i128::from(ticks) - 116_444_736_000_000_000i128) * 100;
    Ok(NativeStat {
        identity: FileIdentity::Windows {
            volume: id.VolumeSerialNumber,
            id: id.FileId.Identifier,
            creation: basic.CreationTime,
        },
        size: file.metadata()?.len(),
        modified_ns: unix_ns(basic.LastWriteTime),
        changed_ns: unix_ns(basic.ChangeTime),
        mode: basic.FileAttributes,
        reuse_supported: true,
    })
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
    pub digest: String,
    /// Missing or unscopable conversion sources forbid stat-based byte reuse.
    pub reusable: bool,
}

fn metadata_source(path: &Path, hash: &mut blake3::Hasher) -> io::Result<()> {
    let name = path.as_os_str().as_encoded_bytes();
    hash.update(&(name.len() as u64).to_le_bytes());
    hash.update(name);
    match fs::read(path) {
        Ok(bytes) => {
            hash.update(&[1]);
            hash.update(&(bytes.len() as u64).to_le_bytes());
            hash.update(&bytes);
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            hash.update(&[0]);
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

impl Policy {
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
        let mut hash = blake3::Hasher::new();
        hash.update(b"aft/views-v3/conversion-policy/1");
        let mut reusable = output.as_ref().is_ok_and(|output| output.status.success());
        hash.update(&[u8::from(reusable)]);
        let config = output.map(|output| output.stdout).unwrap_or_default();
        hash.update(&config);
        for output in sources {
            match output {
                Ok(output) if output.status.success() => {
                    let bytes = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
                    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
                    if bytes.is_empty() || bytes.contains(&b'\n') {
                        reusable = false;
                        continue;
                    }
                    let path = path_from_bytes(bytes)?;
                    let path = if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    };
                    metadata_source(&path, &mut hash)?;
                    reusable &= !path.exists();
                }
                _ => reusable = false,
            }
        }
        // Config --show-origin records include targets and worktree config.
        // Hash their bytes as well as the effective values, including empty files.
        for item in config.split(|b| *b == 0) {
            if let Some(path) = item.strip_prefix(b"file:") {
                let path = path_from_bytes(path)?;
                metadata_source(
                    &if path.is_absolute() {
                        path
                    } else {
                        root.join(path)
                    },
                    &mut hash,
                )?;
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
                    let path = path_from_bytes(path)?;
                    let path =
                        if let Some(suffix) = path.to_str().and_then(|p| p.strip_prefix("~/")) {
                            std::env::var_os("HOME")
                                .or_else(|| std::env::var_os("USERPROFILE"))
                                .map(|home| PathBuf::from(home).join(suffix))
                                .ok_or_else(|| {
                                    io::Error::other("cannot resolve Git policy home directory")
                                })?
                        } else if path.is_absolute() {
                            path
                        } else {
                            root.join(path)
                        };
                    metadata_source(&path, &mut hash)?;
                    reusable &= !conversion;
                }
            }
        }
        for path in metadata {
            metadata_source(path, &mut hash)?;
            if path.exists()
                && path
                    .file_name()
                    .is_some_and(|name| name == ".gitattributes" || name == "attributes")
            {
                // Until path-scoped attribute provenance is available, every
                // attribute source is conservatively an unscopable conversion.
                reusable = false;
            }
        }
        Ok(Self {
            digest: hash.finalize().to_hex().to_string(),
            reusable,
        })
    }
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

pub(crate) fn now_ns() -> u128 {
    u128::from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    ) * 1_000_000_000
}

/// Enumerate policy inputs with metadata only. Source files are never opened.
pub(crate) fn policy_inputs(root: &Path) -> io::Result<Vec<PathBuf>> {
    fn walk(root: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
        for name in [".gitattributes", ".gitignore", ".ignore", ".aftignore"] {
            out.push(root.join(name));
        }
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let name = entry.file_name();
            if name == ".git" {
                continue;
            }
            if kind.is_dir() {
                walk(&entry.path(), out)?;
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    walk(root, &mut paths)?;
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
    if let Some(gitdir) = gitdir {
        paths.push(gitdir.join("info/attributes"));
        paths.push(gitdir.join("info/exclude"));
        let common_marker = gitdir.join("commondir");
        match fs::read(&common_marker) {
            Ok(bytes) => {
                let common = path_from_bytes(bytes.trim_ascii())?;
                let common = if common.is_absolute() {
                    common
                } else {
                    gitdir.join(common)
                };
                paths.push(common.join("info/attributes"));
                paths.push(common.join("info/exclude"));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        paths.push(common_marker);
    }
    paths.sort();
    Ok(paths)
}

pub(crate) fn gitdir(root: &Path) -> io::Result<PathBuf> {
    let marker = root.join(".git");
    if marker.is_dir() {
        return fs::canonicalize(marker);
    }
    let bytes = fs::read(marker)?;
    let path = bytes
        .strip_prefix(b"gitdir: ")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid gitdir marker"))?;
    let path = path_from_bytes(path.trim_ascii())?;
    fs::canonicalize(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
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
        let steady_supported = root_stat.reuse_supported
            && cfg!(any(target_os = "linux", target_os = "macos", windows));
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
            let head = queries.head_tree()?;
            if head.status.success() {
                head_tree = Some(head.stdout);
            }
            let path = directory.join("index");
            match NativeStat::strict_read(&path) {
                Ok(before) => {
                    let bytes = fs::read(&path)?;
                    let after = NativeStat::strict_read(&path)?;
                    if before != after {
                        return Err(io::Error::other("Git index changed during verification"));
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
            subscription: crate::watcher_backend::subscription::current(root),
            steady_supported,
        })
    }
    pub fn validate(&self, root: &Path) -> io::Result<()> {
        let current = Self::capture(root, None)?;
        if &current != self {
            return Err(io::Error::other(
                "checkout Git, root identity or conversion policy changed",
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
        let mut db = rusqlite::Connection::open(&staging)?;
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
        fs::File::open(&staging)?.sync_all()?;
        if destination.exists() {
            fs::remove_file(&staging)?;
        } else {
            fs::rename(&staging, &destination)?;
        }
        super::sync_directory(&dir)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&staging);
        return Err(io::Error::other(error.to_string()));
    }
    Ok(())
}

pub(crate) fn read_evidence(
    dir: &Path,
    generation: &str,
    guard: &BindGuard,
) -> Option<BTreeMap<RelPath, Evidence>> {
    let db = rusqlite::Connection::open_with_flags(
        evidence_path(dir, generation),
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

#[cfg(test)]
mod tests {
    use super::*;

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
                [
                    Err(io::Error::other("unsupported Git source query")),
                    success(b"unused")
                ]
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
    fn failed_config_disables_reuse_but_still_hashes_known_policy_sources() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join(".gitattributes");
        fs::write(&source, b"*.rs text\n").unwrap();
        let capture = || {
            Policy::capture_sources(
                root.path(),
                std::slice::from_ref(&source),
                Err(io::Error::other("config unavailable")),
                [
                    Err(io::Error::other("unknown system attributes")),
                    Err(io::Error::other("unknown global attributes")),
                ],
            )
            .unwrap()
        };
        let before = capture();
        assert!(!before.reusable);
        fs::write(&source, b"*.rs filter=custom\n").unwrap();
        let after = capture();
        assert!(!after.reusable);
        assert_ne!(before.digest, after.digest);
    }

    #[test]
    fn final_policy_digest_covers_git_stat_settings_and_include_targets() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        for (key, value) in [
            ("core.autocrlf", "true"),
            ("core.eol", "crlf"),
            ("core.symlinks", "false"),
            ("core.ignorecase", "true"),
            ("core.checkStat", "minimal"),
            ("core.trustctime", "false"),
            ("core.fileMode", "false"),
        ] {
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

    #[test]
    fn stat_evidence_discards_identity_change_and_racy_observations() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file.rs");
        fs::write(&file, b"one").unwrap();
        let stat = NativeStat::read(&file).unwrap();
        let observed = u128::try_from(stat.modified_ns).unwrap();
        assert!(!stat.reusable(&stat, observed));
        assert!(stat.reusable(&stat, observed + 1));
        let mut changed = stat.clone();
        changed.reuse_supported = false;
        assert!(!changed.reusable(&changed, observed + 1));
        changed = stat.clone();
        changed.changed_ns += 1;
        assert!(!stat.reusable(&changed, observed + 1));
        changed = stat.clone();
        changed.identity = FileIdentity::Windows {
            volume: 42,
            id: [3; 16],
            creation: 0,
        };
        assert!(!stat.reusable(&changed, observed + 1));
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
