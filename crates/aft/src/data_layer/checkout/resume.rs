//! Resuming a cold build that was killed.
//!
//! A cold build keeps the work of two stages in its workspace as
//! checkpoints, so a build killed later (by memory pressure, a restart or a
//! crash) does not start again from nothing:
//! - `records`: the content records the scan staged. They are keyed by
//!   content, language and extractor version, so they stay valid whatever
//!   the checkout's files are now.
//! - `passes-2-3`: every parsed file's pass-2/3 resolution (the spool).
//!   Valid only for the exact scanned inputs it was resolved from, so each
//!   of its files names the digest of those inputs (the snapshot digest).
//!
//! Pass 4, fragment assembly and the published segment are not
//! checkpointed: a build killed there redoes them.
//!
//! Every checkpoint file is self-describing ([`Header`]): it names the
//! build's identity digest, the snapshot digest when it depends on one, its
//! stage and the paths or record keys it covers. It ends with its length and
//! a blake3 of everything before it, and is written as `<name>.part`,
//! closed, then renamed. A stage is complete only once its marker
//! `STAGE-<stage>` exists; the marker lists every file of the stage with its
//! length and checksum, and is renamed into place last. Nothing is synced: a
//! file torn by a power loss fails the checks below, and its stage is redone.
//!
//! A build that finds a workspace an earlier build of the checkout left
//! (see [`claim`]) adopts it only when its `IDENTITY` equals its own, then
//! deletes every file no complete stage lists, and checks every file of
//! every complete stage in full before using any of it. One `INFO` line is
//! logged for each decision. Deaths are counted in `attempts`, beside the
//! checkout's `checkout.lock` and outside the workspace: a build that died
//! twice with the same stages complete starts once from nothing.

use std::collections::HashSet;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use aft_store::family::workspace::{self as ws, WorkspaceIdentity};
use aft_store::family::{BuildScratch, ScratchClaim};
use aft_store::ids::{CheckoutId, FamilyId};
use aft_store::scan::ScanPolicy;

use super::{DataError, Planes};

/// The stage of the scan's staged content records.
pub(crate) const RECORDS: &str = "records";
/// The stage of every file's pass-2/3 resolution.
pub(crate) const PASSES: &str = "passes-2-3";

/// A blake3 digest.
pub(crate) type Digest = [u8; 32];

const MAGIC: &[u8; 8] = b"AFTCKPT1";
/// Length and checksum after the body.
const TRAILER: usize = 8 + 32;

/// What a checkpoint file says about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    /// The digest of the build's identity (see [`BuildIdentity`]).
    pub(crate) identity: Digest,
    /// The digest of the scanned inputs the file depends on, if it does.
    pub(crate) snapshot: Option<Digest>,
    pub(crate) stage: String,
    /// What the file covers, explicitly: paths, record keys or file names.
    pub(crate) members: Vec<Vec<u8>>,
}

/// A whole file's length and blake3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileSum {
    pub(crate) len: u64,
    pub(crate) checksum: Digest,
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    let length = u32::try_from(bytes.len())
        .map_err(|_| io::Error::other("a checkpoint header part is 4 GiB or larger"))?;
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// Reads length-prefixed parts from a byte slice.
pub(crate) struct Reader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    pub(crate) fn new(bytes: &'b [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub(crate) fn take(&mut self, len: usize) -> io::Result<&'b [u8]> {
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| io::Error::other("a checkpoint file ends early"))?;
        let part = &self.bytes[self.at..end];
        self.at = end;
        Ok(part)
    }

    pub(crate) fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    pub(crate) fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    pub(crate) fn digest(&mut self) -> io::Result<Digest> {
        Ok(self.take(32)?.try_into().expect("32 bytes"))
    }

    pub(crate) fn part(&mut self) -> io::Result<&'b [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    pub(crate) fn is_done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

impl Header {
    fn encode(&self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.identity);
        match &self.snapshot {
            Some(snapshot) => {
                out.push(1);
                out.extend_from_slice(snapshot);
            }
            None => out.push(0),
        }
        push_bytes(&mut out, self.stage.as_bytes())?;
        let count = u32::try_from(self.members.len())
            .map_err(|_| io::Error::other("too many checkpoint members"))?;
        out.extend_from_slice(&count.to_le_bytes());
        for member in &self.members {
            push_bytes(&mut out, member)?;
        }
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> io::Result<Self> {
        let mut reader = Reader::new(bytes);
        let identity = reader.digest()?;
        let snapshot = match reader.u8()? {
            0 => None,
            1 => Some(reader.digest()?),
            _ => return Err(io::Error::other("a checkpoint header is malformed")),
        };
        let stage = String::from_utf8(reader.part()?.to_vec())
            .map_err(|_| io::Error::other("a checkpoint stage name is not UTF-8"))?;
        let count = reader.u32()? as usize;
        let mut members = Vec::with_capacity(count.min(bytes.len()));
        for _ in 0..count {
            members.push(reader.part()?.to_vec());
        }
        if !reader.is_done() {
            return Err(io::Error::other("a checkpoint header has trailing bytes"));
        }
        Ok(Self {
            identity,
            snapshot,
            stage,
            members,
        })
    }
}

/// The `.part` name a checkpoint file is written under before its rename.
pub(crate) fn part_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// The bytes [`write_framed`] writes for `header` and `body`.
fn framed(header: &Header, body: &[u8]) -> io::Result<Vec<u8>> {
    let header = header.encode()?;
    let mut bytes = Vec::with_capacity(MAGIC.len() + 4 + header.len() + body.len() + TRAILER);
    bytes.extend_from_slice(MAGIC);
    push_bytes(&mut bytes, &header)?;
    bytes.extend_from_slice(body);
    let length = bytes.len() as u64;
    let checksum = blake3::hash(&bytes);
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(checksum.as_bytes());
    Ok(bytes)
}

/// How many bytes [`write_framed`] writes for `header` and `body_len`
/// bytes of body, for disk admission before writing.
pub(crate) fn framed_len(header: &Header, body_len: usize) -> u64 {
    let header = header.encode().map_or(0, |bytes| bytes.len());
    (MAGIC.len() + 4 + header + body_len + TRAILER) as u64
}

/// Writes a checkpoint file at `path`: as `<path>.part`, closed, then
/// renamed. Returns the whole file's length and checksum.
///
/// # Errors
///
/// When the file cannot be written or renamed.
pub(crate) fn write_framed(path: &Path, header: &Header, body: &[u8]) -> io::Result<FileSum> {
    let bytes = framed(header, body)?;
    let part = part_path(path);
    {
        // A `.part` left by a killed build holds nothing anyone reads.
        let mut file = std::fs::File::create(&part)?;
        file.write_all(&bytes)?;
    }
    std::fs::rename(&part, path)?;
    Ok(FileSum {
        len: bytes.len() as u64,
        checksum: *blake3::hash(&bytes).as_bytes(),
    })
}

/// Reads the checkpoint file `path` already read into `bytes`, checking its
/// trailer.
///
/// # Errors
///
/// When the bytes are not exactly a whole checkpoint file.
pub(crate) fn parse_framed(path: &Path, bytes: &[u8]) -> io::Result<(Header, Vec<u8>)> {
    let damaged = || io::Error::other(format!("{} is not a whole checkpoint file", path.display()));
    let body_end = bytes.len().checked_sub(TRAILER).ok_or_else(damaged)?;
    let mut trailer = Reader::new(&bytes[body_end..]);
    if trailer.u64()? != body_end as u64
        || trailer.digest()? != *blake3::hash(&bytes[..body_end]).as_bytes()
        || bytes.get(..MAGIC.len()) != Some(MAGIC.as_slice())
    {
        return Err(damaged());
    }
    let mut reader = Reader::new(&bytes[MAGIC.len()..body_end]);
    let header = Header::decode(reader.part()?)?;
    let body = bytes[MAGIC.len() + reader.at..body_end].to_vec();
    Ok((header, body))
}

/// Reads the checkpoint file at `path`, checking its trailer.
///
/// # Errors
///
/// When the file cannot be read or is not exactly as written.
pub(crate) fn read_framed(path: &Path) -> io::Result<(Header, Vec<u8>)> {
    parse_framed(path, &std::fs::read(path)?)
}

/// The length and blake3 of the whole file at `path`, read in full.
///
/// # Errors
///
/// When the file cannot be read.
pub(crate) fn sum_file(path: &Path) -> io::Result<FileSum> {
    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(path)?;
    let len = io::copy(&mut file, &mut hasher)?;
    Ok(FileSum {
        len,
        checksum: *hasher.finalize().as_bytes(),
    })
}

/// The name of a checkpoint file covering `members`, derived from them and
/// its stage, never from the order or thread that wrote it.
pub(crate) fn member_name<'m>(prefix: &str, members: impl IntoIterator<Item = &'m [u8]>) -> String {
    let mut hasher = blake3::Hasher::new_derive_key("aft 2026-11 cold build checkpoint name");
    hasher.update(prefix.as_bytes());
    let mut count = 0u64;
    for member in members {
        hasher.update(&(member.len() as u64).to_le_bytes());
        hasher.update(member);
        count += 1;
    }
    hasher.update(&count.to_le_bytes());
    let hash = hasher.finalize();
    let hex = hash.to_hex();
    format!("{prefix}-{}", &hex.as_str()[..32])
}

/// A complete stage's marker: what it lists and each file's sum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Marker {
    pub(crate) header: Header,
    pub(crate) sums: Vec<FileSum>,
}

impl Marker {
    /// Each listed file, relative to the workspace, with its sum.
    pub(crate) fn files(&self) -> impl Iterator<Item = (&str, FileSum)> + '_ {
        self.header
            .members
            .iter()
            .zip(&self.sums)
            .map(|(name, sum)| (std::str::from_utf8(name).unwrap_or(""), *sum))
    }
}

/// Where the marker of `stage` lies in `workspace`.
pub(crate) fn marker_path(workspace: &Path, stage: &str) -> PathBuf {
    workspace.join(format!("STAGE-{stage}"))
}

/// Completes `stage`: writes its marker listing `files` (relative to the
/// workspace, `/`-separated) with their sums. Disk admission is asked
/// first.
///
/// # Errors
///
/// When admission refuses, or the marker cannot be written.
pub(crate) fn write_marker(
    workspace: &BuildScratch<DataError>,
    stage: &str,
    identity: Digest,
    snapshot: Option<Digest>,
    files: &[(String, FileSum)],
) -> Result<(), DataError> {
    let header = Header {
        identity,
        snapshot,
        stage: stage.to_string(),
        members: files
            .iter()
            .map(|(name, _)| name.as_bytes().to_vec())
            .collect(),
    };
    let mut body = Vec::with_capacity(files.len() * 40);
    for (_, sum) in files {
        body.extend_from_slice(&sum.len.to_le_bytes());
        body.extend_from_slice(&sum.checksum);
    }
    workspace.check_space(framed_len(&header, body.len()))?;
    write_framed(&marker_path(workspace.path(), stage), &header, &body)
        .map_err(|error| DataError::Store(format!("the {stage} checkpoint: {error}")))?;
    Ok(())
}

/// Reads the marker of `stage` in `workspace`; `None` when there is none.
///
/// # Errors
///
/// When the marker exists but is not exactly as written.
pub(crate) fn read_marker(workspace: &Path, stage: &str) -> io::Result<Option<Marker>> {
    let path = marker_path(workspace, stage);
    if !path.exists() {
        return Ok(None);
    }
    let (header, body) = read_framed(&path)?;
    let mut reader = Reader::new(&body);
    let mut sums = Vec::with_capacity(header.members.len());
    for _ in 0..header.members.len() {
        sums.push(FileSum {
            len: reader.u64()?,
            checksum: reader.digest()?,
        });
    }
    if !reader.is_done() || header.stage != stage {
        return Err(io::Error::other("a stage marker is malformed"));
    }
    Ok(Some(Marker { header, sums }))
}

/// Checks every file `marker` lists in full: present, of the length and
/// checksum it lists, and naming the build's identity.
fn verify_marker(workspace: &Path, marker: &Marker, identity: Digest) -> Result<(), String> {
    if marker.header.identity != identity {
        return Err("it was written by another build".to_string());
    }
    for (name, expected) in marker.files() {
        if !is_member_name(name) {
            return Err(format!("it lists a malformed file name {name:?}"));
        }
        let actual = sum_file(&workspace.join(name)).map_err(|error| format!("{name}: {error}"))?;
        if actual != expected {
            return Err(format!("{name} is not as it was written"));
        }
    }
    Ok(())
}

/// Whether `name` is a file name a stage may list: folders and a file
/// inside the workspace, `/`-separated, each made of ASCII letters, digits,
/// `.`, `_` and `-` only and never `.` or `..`. So no name can reach out of
/// the workspace on any platform: no backslash, drive prefix or root.
fn is_member_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        })
}

/// What a build is: everything its checkpoints depend on besides the
/// scanned files. Two builds with equal identities write equal checkpoints
/// from equal inputs.
#[derive(Clone, Debug)]
pub(crate) struct BuildIdentity {
    pub(crate) root: PathBuf,
    /// One `name=value` line per fact, in a fixed order.
    pub(crate) payload: Vec<u8>,
    pub(crate) digest: Digest,
    /// Whether every fact is known. A build whose identity is not complete
    /// adopts nothing: an unknown fact cannot prove two builds the same.
    pub(crate) complete: bool,
}

impl BuildIdentity {
    pub(crate) fn of(
        family: FamilyId,
        checkout: CheckoutId,
        root: &Path,
        planes: Planes,
        policy: &ScanPolicy,
    ) -> Self {
        let executable = executable_fingerprint();
        let mut features = Vec::new();
        for (name, enabled) in [
            ("test-hooks", cfg!(feature = "test-hooks")),
            (
                "callgraph-shape-census",
                cfg!(feature = "callgraph-shape-census"),
            ),
        ] {
            if enabled {
                features.push(name);
            }
        }
        let policy_digest = {
            let mut hasher = blake3::Hasher::new();
            for value in [policy.max_indexed_len, policy.max_read_len] {
                hasher.update(&value.to_le_bytes());
            }
            hasher.update(&[u8::from(policy.read_large.is_some())]);
            for list in [&policy.ignore_files, &policy.skip_names, &policy.skip_dirs] {
                hasher.update(&(list.len() as u64).to_le_bytes());
                for item in list {
                    hasher.update(&(item.len() as u64).to_le_bytes());
                    hasher.update(item.as_bytes());
                }
            }
            hasher.finalize().to_hex().to_string()
        };
        let lines = [
            ("family", family.to_string()),
            ("checkout", checkout.to_string()),
            ("root", root.to_string_lossy().into_owned()),
            ("aft_version", env!("CARGO_PKG_VERSION").to_string()),
            (
                "source_commit",
                // Unstamped local builds are told apart by the executable.
                option_env!("AFT_BUILD_GIT_SHA")
                    .unwrap_or("unstamped")
                    .to_string(),
            ),
            ("features", features.join(",")),
            (
                "executable",
                // An executable that cannot be examined names nothing
                // another build could equal, so nothing is ever adopted.
                executable.clone().unwrap_or_else(|| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |elapsed| elapsed.as_nanos());
                    format!("unknown-{}-{now}", std::process::id())
                }),
            ),
            ("extractor", super::EXTRACTOR_VERSION.to_string()),
            (
                "fragment_format",
                super::super::fragment::FORMAT_VERSION.to_string(),
            ),
            (
                "graph_sections",
                aft_store::graph::SECTIONS_VERSION.to_string(),
            ),
            (
                "content_sections",
                aft_store::content::SECTIONS_VERSION.to_string(),
            ),
            (
                "segment_format",
                aft_store::segment::FORMAT_VERSION.to_string(),
            ),
            (
                "state_format",
                aft_store::checkout::STATE_FORMAT_VERSION.to_string(),
            ),
            (
                "planes",
                format!("text={},callgraph={}", planes.text, planes.callgraph),
            ),
            ("scan_policy", policy_digest),
        ];
        let mut payload = Vec::new();
        for (name, value) in lines {
            payload.extend_from_slice(name.as_bytes());
            payload.push(b'=');
            payload.extend_from_slice(value.replace('\n', " ").as_bytes());
            payload.push(b'\n');
        }
        let digest = *blake3::hash(&payload).as_bytes();
        Self {
            root: root.to_path_buf(),
            payload,
            digest,
            complete: executable.is_some(),
        }
    }

    fn workspace_identity(&self) -> WorkspaceIdentity {
        WorkspaceIdentity {
            root: self.root.clone(),
            payload: self.payload.clone(),
        }
    }

    /// The first fact `other` names differently, to say why a workspace
    /// was not adopted.
    fn first_difference(&self, other: &WorkspaceIdentity) -> String {
        if other.root != self.root {
            return "root".to_string();
        }
        let ours = self.payload.split(|byte| *byte == b'\n');
        let theirs = other.payload.split(|byte| *byte == b'\n');
        for (a, b) in ours.zip(theirs) {
            if a != b {
                return String::from_utf8_lossy(a.split(|byte| *byte == b'=').next().unwrap_or(a))
                    .into_owned();
            }
        }
        "the number of facts".to_string()
    }
}

/// Which executable this process runs: the length, modification time and
/// (on Unix) file identity of the image it started from, taken once. A
/// source commit alone does not tell two local builds of a changed tree
/// apart, and a resumed build must be the same build. The image, not the
/// file now at the executable's path: a new binary may be renamed over
/// that path while this process runs. `None` when the image cannot be
/// examined.
fn executable_fingerprint() -> Option<String> {
    static FINGERPRINT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    FINGERPRINT
        .get_or_init(|| {
            let metadata = crate::pinned_executable::startup_image_metadata()?;
            let modified = metadata
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_nanos();
            #[cfg(unix)]
            let file = {
                use std::os::unix::fs::MetadataExt as _;
                format!(":{}:{}", metadata.dev(), metadata.ino())
            };
            #[cfg(not(unix))]
            let file = String::new();
            Some(format!("{}:{modified}{file}", metadata.len()))
        })
        .clone()
}

/// The digest of everything resolution reads from the scan (see the
/// module's comment): each path with its content, kind, mode, link target
/// and language, and every configuration file's bytes. A file's timestamps
/// are left out, since nothing after the scan reads them.
pub(crate) fn snapshot_digest(inputs: &super::super::callgraph::CheckoutInputs) -> Digest {
    let mut hasher = blake3::Hasher::new_derive_key("aft 2026-11 cold build scanned inputs");
    let mut part = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    for record in inputs.state.paths() {
        part(&record.path);
        part(record.content.digest().as_bytes());
        part(format!("{:?}", record.kind).as_bytes());
        part(&record.mode.to_le_bytes());
        part(&record.link);
        part(record.language.as_bytes());
    }
    part(b"configs");
    let mut configs = inputs.configs.iter().collect::<Vec<_>>();
    configs.sort_by(|a, b| a.0.cmp(b.0));
    for (path, bytes) in configs {
        part(path);
        part(bytes);
    }
    *hasher.finalize().as_bytes()
}

// ---------------------------------------------------------------------------
// Deaths

/// The checkout's record of its build attempts, `builds/<checkout>/attempts`.
/// It lives beside `checkout.lock`, outside every workspace, so it survives
/// a workspace's deletion; it is deleted when a build publishes or stands
/// down. It names the build identity it applies to: a build of another
/// identity starts a record of its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Attempts {
    /// The digest of the build identity the attempts belong to.
    pub(crate) identity: Digest,
    /// The workspace folder the attempts worked in.
    pub(crate) build: String,
    /// An attempt started and has not ended. Found so at the start of the
    /// next attempt, it died.
    pub(crate) running: bool,
    /// How many stages were complete when the last attempt that died
    /// died, and how many attempts in a row died with that many.
    pub(crate) death_stage: u32,
    pub(crate) deaths: u32,
    /// Whether a fresh start was already made because attempts kept dying.
    pub(crate) fresh_used: bool,
    /// A fresh start was decided and `build` is condemned: written before
    /// the workspace is deleted, so a build killed during the fresh start
    /// finishes it instead of counting deaths anew.
    pub(crate) restarting: bool,
}

const ATTEMPTS: &str = "attempts";

impl Attempts {
    fn encode(&self) -> Vec<u8> {
        format!(
            "build={}\nrunning={}\ndeath_stage={}\ndeaths={}\nfresh_used={}\nrestarting={}\n",
            self.build,
            u8::from(self.running),
            self.death_stage,
            self.deaths,
            u8::from(self.fresh_used),
            u8::from(self.restarting)
        )
        .into_bytes()
    }

    fn decode(body: &[u8]) -> io::Result<Self> {
        let text = std::str::from_utf8(body)
            .map_err(|_| io::Error::other("the attempts record is not UTF-8"))?;
        let mut attempts = Self::default();
        let malformed = || io::Error::other("the attempts record is malformed");
        for line in text.lines() {
            let (name, value) = line.split_once('=').ok_or_else(malformed)?;
            match name {
                "build" => attempts.build = value.to_string(),
                "running" => attempts.running = value == "1",
                "death_stage" => attempts.death_stage = value.parse().map_err(|_| malformed())?,
                "deaths" => attempts.deaths = value.parse().map_err(|_| malformed())?,
                "fresh_used" => attempts.fresh_used = value == "1",
                "restarting" => attempts.restarting = value == "1",
                _ => return Err(malformed()),
            }
        }
        Ok(attempts)
    }

    fn header(identity: Digest) -> Header {
        Header {
            identity,
            snapshot: None,
            stage: ATTEMPTS.to_string(),
            members: Vec::new(),
        }
    }

    /// The record in the checkout's build folder `dir`.
    ///
    /// # Errors
    ///
    /// When a record exists and cannot be read.
    fn read(dir: &Path) -> io::Result<Option<Self>> {
        let path = dir.join(ATTEMPTS);
        if !path.exists() {
            return Ok(None);
        }
        let (header, body) = read_framed(&path)?;
        if header != Self::header(header.identity) {
            return Err(io::Error::other("the attempts record is malformed"));
        }
        let mut attempts = Self::decode(&body)?;
        attempts.identity = header.identity;
        Ok(Some(attempts))
    }

    fn write(&self, dir: &Path) -> io::Result<()> {
        write_framed(
            &dir.join(ATTEMPTS),
            &Self::header(self.identity),
            &self.encode(),
        )
        .map(drop)
    }

    /// What the attempts read from disk (`read`) say about the build whose
    /// identity is `identity`, given the names of the workspaces earlier
    /// builds left (`leftovers`). A record of another identity is not this
    /// build's. Once the workspace a record names is gone (expired or
    /// reclaimed, not condemned by a fresh start), nothing the record
    /// counted applies any more, the fresh start included.
    fn applying_to(read: Option<Self>, identity: Digest, leftovers: &[String]) -> Self {
        let fresh = Self {
            identity,
            ..Self::default()
        };
        let Some(read) = read else {
            return fresh;
        };
        if read.identity != identity {
            log::info!(
                "callgraph build resume: reset the attempts record: it belongs to another build"
            );
            return fresh;
        }
        if !read.restarting && !read.build.is_empty() && !leftovers.contains(&read.build) {
            return fresh;
        }
        read
    }
}

/// Writes or deletes the attempts record, logging a failure once: the
/// record never decides whether a build may run.
fn log_attempts_failure(what: &str, error: &io::Error) {
    static LOGGED: std::sync::Once = std::sync::Once::new();
    LOGGED.call_once(|| {
        log::warn!("callgraph build resume: could not {what} the attempts record: {error}");
    });
}

// ---------------------------------------------------------------------------
// Claiming a workspace

/// A build's workspace, adopted or new, with what it may reuse.
pub(crate) struct Claimed {
    pub(crate) scratch: Arc<BuildScratch<DataError>>,
    pub(crate) identity: BuildIdentity,
    /// The complete `records` stage, checked in full.
    pub(crate) records: Option<Marker>,
    /// The complete `passes-2-3` stage, checked in full; usable only for
    /// the snapshot its header names.
    pub(crate) passes: Option<Marker>,
    attempts_dir: PathBuf,
    attempts: Attempts,
    /// Set once the build published or stood down.
    done: std::sync::atomic::AtomicBool,
}

impl Claimed {
    /// Records that this attempt ended without dying: it failed, paused
    /// for lack of disk, or was cancelled. A pause also marks the workspace
    /// paused, so reclaiming disk deletes it after every dead one.
    ///
    /// An attempt that failed for any other reason may have failed on what
    /// it adopted: a file that passed its whole-file checks can still fail
    /// its record's own (one written damaged and summed as written). So the
    /// stages it adopted are no longer complete, and the next attempt
    /// builds them again rather than failing on them every time.
    pub(crate) fn ended(&self, error: &DataError) {
        if self.done.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        match error {
            DataError::DiskFloor(_) => {
                let _ = self.scratch.mark_paused();
            }
            DataError::Cancelled | DataError::Busy(_) | DataError::NewerFormat(_) => {}
            _ => {
                for marker in [&self.records, &self.passes].into_iter().flatten() {
                    let stage = &marker.header.stage;
                    match std::fs::remove_file(marker_path(self.scratch.path(), stage)) {
                        Err(failure) if failure.kind() != io::ErrorKind::NotFound => {}
                        _ => log::info!(
                            "callgraph build resume: discarded stage {stage}: the build that adopted it failed ({error})"
                        ),
                    }
                }
            }
        }
        let attempts = Attempts {
            running: false,
            ..self.attempts.clone()
        };
        if let Err(error) = attempts.write(&self.attempts_dir) {
            log_attempts_failure("write", &error);
        }
    }

    /// Deletes the workspace and the attempts record, once the build
    /// published or stood down.
    pub(crate) fn finished(&self) {
        self.done.store(true, std::sync::atomic::Ordering::SeqCst);
        #[cfg(all(test, target_os = "linux"))]
        note_open_files_for_test(self.scratch.path());
        let _ = self.scratch.remove();
        remove_attempts(&self.attempts_dir);
    }
}

/// The files under each build workspace this process still had open when
/// the workspace was deleted, by workspace.
#[cfg(all(test, target_os = "linux"))]
pub(crate) static OPEN_AT_FINISH: std::sync::Mutex<Vec<(PathBuf, Vec<PathBuf>)>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(all(test, target_os = "linux"))]
fn note_open_files_for_test(workspace: &Path) {
    let open = std::fs::read_dir("/proc/self/fd")
        .into_iter()
        .flatten()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| target.starts_with(workspace))
        .collect();
    OPEN_AT_FINISH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((workspace.to_path_buf(), open));
}

fn remove_attempts(dir: &Path) {
    match std::fs::remove_file(dir.join(ATTEMPTS)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            log_attempts_failure("delete", &error);
        }
        _ => {}
    }
}

/// Deletes every workspace earlier builds of the claimed checkout left, and
/// its attempts record, once the checkout no longer needs a cold build.
pub(crate) fn discard(claim: ScratchClaim) {
    if let Ok(leftovers) = claim.leftovers() {
        for leftover in leftovers {
            if claim.remove(&leftover).is_ok() {
                log::info!(
                    "callgraph build resume: discarded workspace {}: the checkout no longer needs a cold build",
                    leftover.file_name().unwrap_or_default().to_string_lossy()
                );
            }
        }
    }
    remove_attempts(claim.dir());
}

/// The folder name of `workspace`.
fn workspace_name(workspace: &Path) -> String {
    workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Why a leftover workspace cannot be adopted, or `None` when it can.
fn adoption_refusal(workspace: &Path, identity: &BuildIdentity) -> Option<String> {
    if !identity.complete {
        return Some("this build's executable cannot be examined".to_string());
    }
    match ws::read_identity(workspace) {
        None => Some("it has no readable IDENTITY".to_string()),
        Some(found) if found != identity.workspace_identity() => Some(format!(
            "its identity differs ({})",
            identity.first_difference(&found)
        )),
        Some(_) => None,
    }
}

/// Deletes every file of `workspace` that no stage in `keep` lists, besides
/// the workspace's own identity files, and every folder left empty.
fn tidy(workspace: &Path, keep: &HashSet<PathBuf>) -> io::Result<()> {
    fn walk(dir: &Path, keep: &HashSet<PathBuf>, top: bool) -> io::Result<bool> {
        let mut empty = true;
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let own = top
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| [ws::IDENTITY, ws::ADOPTED, ws::PAUSED].contains(&name));
            if keep.contains(&path) || own {
                empty = false;
            } else if path.is_dir() && !path.is_symlink() {
                if walk(&path, keep, false)? {
                    std::fs::remove_dir(&path)?;
                } else {
                    empty = false;
                }
            } else {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(empty)
    }
    walk(workspace, keep, true).map(drop)
}

/// The complete stages of `workspace`, each checked in full; a stage whose
/// marker or files fail a check is dropped, with an `INFO` line saying why.
fn complete_stages(workspace: &Path, identity: &BuildIdentity) -> (Option<Marker>, Option<Marker>) {
    let mut found = [None, None];
    for (slot, stage) in found.iter_mut().zip([RECORDS, PASSES]) {
        match read_marker(workspace, stage) {
            Ok(None) => {}
            Ok(Some(marker)) => match verify_marker(workspace, &marker, identity.digest) {
                Ok(()) => *slot = Some(marker),
                Err(reason) => {
                    log::info!("callgraph build resume: discarded stage {stage}: {reason}")
                }
            },
            Err(error) => log::info!(
                "callgraph build resume: discarded stage {stage}: its marker is damaged ({error})"
            ),
        }
    }
    let [records, passes] = found;
    (records, passes)
}

/// Claims the checkout's build workspace for a cold build: adopts the one
/// an earlier build left when its identity equals `identity`, keeping
/// whatever complete stages pass every check, or starts a new one. `admit`
/// is the build's disk admission; a new workspace is made only once it
/// admits the identity file, so a refusal writes nothing.
///
/// # Errors
///
/// When admission refuses, or a folder cannot be made, listed or deleted.
pub(crate) fn claim(
    claim: ScratchClaim,
    identity: BuildIdentity,
    admit: impl Fn(u64) -> Result<(), DataError> + Send + Sync + 'static,
) -> Result<Claimed, DataError> {
    let store = |error: aft_store::family::FamilyError| DataError::Store(error.to_string());
    let attempts_dir = claim.dir().to_path_buf();
    let leftovers = claim.leftovers().map_err(store)?;
    let names = leftovers
        .iter()
        .map(|path| workspace_name(path))
        .collect::<Vec<_>>();
    let (mut attempts, readable) = match Attempts::read(&attempts_dir) {
        Ok(found) => (Attempts::applying_to(found, identity.digest, &names), true),
        Err(error) => {
            log_attempts_failure("read", &error);
            (Attempts::applying_to(None, identity.digest, &names), false)
        }
    };
    // The newest workspace this build may adopt; every other leftover is
    // deleted when one is adopted or a new one made.
    let mut candidate = None;
    for leftover in leftovers.into_iter().rev() {
        let name = workspace_name(&leftover);
        let refusal = if candidate.is_some() {
            Some("a newer workspace is adopted".to_string())
        } else if attempts.restarting && attempts.build == name {
            Some("a fresh start condemned it and did not finish".to_string())
        } else {
            adoption_refusal(&leftover, &identity)
        };
        match refusal {
            Some(reason) => {
                log::info!("callgraph build resume: discarded workspace {name}: {reason}");
            }
            None => candidate = Some((leftover, name)),
        }
    }
    if !readable {
        if let Some((_, name)) = candidate.take() {
            log::info!(
                "callgraph build resume: discarded workspace {name}: its attempts record cannot be read, so the build starts fresh"
            );
        }
    }
    let mut adopted = None;
    if let Some((path, name)) = candidate {
        let (records, passes) = complete_stages(&path, &identity);
        let complete = u32::from(records.is_some()) + u32::from(passes.is_some());
        // An attempt still marked running died. Two deaths in a row with the
        // same stages complete start the build once from nothing.
        if attempts.running && attempts.build == name {
            if attempts.deaths > 0 && attempts.death_stage == complete {
                attempts.deaths += 1;
            } else {
                attempts.death_stage = complete;
                attempts.deaths = 1;
            }
        } else {
            attempts.deaths = 0;
        }
        if attempts.deaths >= 2 && !attempts.fresh_used {
            log::info!(
                "callgraph build resume: discarded workspace {name}: two attempts died with {complete} stage(s) complete, so the build starts fresh once"
            );
            // Recorded before the workspace is deleted, so the fresh start
            // happens once even when this attempt dies during it.
            attempts = Attempts {
                identity: identity.digest,
                build: name.clone(),
                fresh_used: true,
                restarting: true,
                ..Attempts::default()
            };
            if let Err(error) = attempts.write(&attempts_dir) {
                log_attempts_failure("write", &error);
            }
        } else {
            let mut keep = HashSet::new();
            for marker in [&records, &passes].into_iter().flatten() {
                keep.insert(marker_path(&path, &marker.header.stage));
                for (file, _) in marker.files() {
                    keep.insert(path.join(file));
                }
            }
            tidy(&path, &keep).map_err(|error| {
                DataError::Store(format!(
                    "tidying the build workspace {}: {error}",
                    path.display()
                ))
            })?;
            for (stage, marker) in [(RECORDS, &records), (PASSES, &passes)] {
                if let Some(marker) = marker {
                    log::info!(
                        "callgraph build resume: reused stage {stage} from workspace {name} ({} file(s))",
                        marker.sums.len()
                    );
                }
            }
            adopted = Some((path, name, records, passes));
        }
    }
    let (scratch, records, passes, build) = match adopted {
        Some((path, name, records, passes)) => (
            claim.adopt(&path, admit).map_err(store)?,
            records,
            passes,
            name,
        ),
        None => {
            for leftover in claim.leftovers().map_err(store)? {
                claim.remove(&leftover).map_err(store)?;
            }
            admit(ws::IDENTITY.len() as u64 + identity.payload.len() as u64 + 256)?;
            let scratch = claim.fresh(admit).map_err(store)?;
            ws::write_identity(scratch.path(), &identity.workspace_identity()).map_err(
                |error| DataError::Store(format!("the build workspace's identity: {error}")),
            )?;
            if attempts.restarting {
                kill_point("claim.restarting");
            }
            let name = workspace_name(scratch.path());
            (scratch, None, None, name)
        }
    };
    if build != attempts.build {
        // A new workspace: deaths of another one do not count against it.
        attempts.deaths = 0;
        attempts.death_stage = 0;
    }
    attempts.identity = identity.digest;
    attempts.build = build;
    attempts.running = true;
    attempts.restarting = false;
    if let Err(error) = attempts.write(&attempts_dir) {
        log_attempts_failure("write", &error);
    }
    Ok(Claimed {
        scratch: Arc::new(scratch),
        identity,
        records,
        passes,
        attempts_dir,
        attempts,
        done: std::sync::atomic::AtomicBool::new(false),
    })
}

/// Whether the `passes-2-3` stage in `marker` was resolved from the inputs
/// whose digest is `snapshot`. Logs the decision.
pub(crate) fn passes_match(marker: &Marker, snapshot: Digest) -> bool {
    let matches = marker.header.snapshot == Some(snapshot);
    if !matches {
        log::info!(
            "callgraph build resume: discarded stage {PASSES}: the scanned files changed since it was written"
        );
    }
    matches
}

/// Deletes the stage `marker` completes from `workspace`: the marker first,
/// so the stage is never complete with a file missing, then its files.
///
/// # Errors
///
/// When a file exists and cannot be deleted.
pub(crate) fn discard_stage(workspace: &Path, marker: &Marker) -> io::Result<()> {
    let remove = |path: PathBuf| match std::fs::remove_file(&path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    };
    remove(marker_path(workspace, &marker.header.stage))?;
    for (name, _) in marker.files() {
        remove(workspace.join(name))?;
    }
    Ok(())
}

/// Kills the process with SIGKILL when `AFT_TEST_KILL_AT` names `point`,
/// for tests that resume a build killed there. `point:N` fires on the
/// N-th time the point is reached.
#[cfg(any(test, feature = "test-hooks"))]
pub(crate) fn kill_point(point: &str) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static REACHED: std::sync::Mutex<Vec<(String, u64)>> = std::sync::Mutex::new(Vec::new());
    static ARMED: AtomicU64 = AtomicU64::new(0);
    let Ok(wanted) = std::env::var("AFT_TEST_KILL_AT") else {
        hide_root(point);
        return;
    };
    let (name, count) = match wanted.split_once(':') {
        Some((name, count)) => (name, count.parse::<u64>().unwrap_or(1)),
        None => (wanted.as_str(), 1),
    };
    if name != point {
        return;
    }
    let reached = {
        let mut reached = REACHED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = reached.iter_mut().find(|(seen, _)| seen == point) {
            entry.1 += 1;
            entry.1
        } else {
            reached.push((point.to_string(), 1));
            1
        }
    };
    if reached != count || ARMED.fetch_add(1, Ordering::SeqCst) != 0 {
        return;
    }
    eprintln!("KILL POINT {wanted}");
    #[cfg(unix)]
    // SAFETY: sending a signal to this process has no memory effects here.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    // As abruptly as SIGKILL: no destructor, buffer flush or exit handler
    // runs. The exit code tells the test the process died here.
    #[cfg(windows)]
    // SAFETY: the pseudo-handle of this process needs no closing, and
    // terminating it has no memory effects here.
    unsafe {
        windows_sys::Win32::System::Threading::TerminateProcess(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            KILLED_EXIT_CODE,
        );
    }
    std::process::abort();
}

/// The exit code of a test build [`kill_point`] terminated on Windows.
#[cfg(all(windows, any(test, feature = "test-hooks")))]
pub(crate) const KILLED_EXIT_CODE: u32 = 0x4B11;

#[cfg(not(any(test, feature = "test-hooks")))]
#[inline]
pub(crate) fn kill_point(_point: &str) {}

/// When `AFT_TEST_HIDE_ROOT` names a checkout's root folder, moves it away
/// once the build has scanned it and back once the segment is written,
/// for the test that proves no stage after the scan reads the checkout.
#[cfg(any(test, feature = "test-hooks"))]
fn hide_root(point: &str) {
    let Ok(root) = std::env::var("AFT_TEST_HIDE_ROOT") else {
        return;
    };
    let hidden = format!("{root}.hidden");
    let moved = match point {
        "stage.resolve" => std::fs::rename(&root, &hidden),
        "stage.publish" => std::fs::rename(&hidden, &root),
        _ => return,
    };
    moved.expect("the checkout's root can be moved");
    eprintln!("HIDE ROOT at {point}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(stage: &str) -> Header {
        Header {
            identity: [7; 32],
            snapshot: Some([9; 32]),
            stage: stage.to_string(),
            members: vec![b"src/a.rs".to_vec(), Vec::new()],
        }
    }

    #[test]
    fn a_checkpoint_file_reads_back_and_a_changed_or_torn_one_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file");
        let sum = write_framed(&path, &header("passes-2-3"), b"body").unwrap();
        assert_eq!(sum, sum_file(&path).unwrap());
        assert!(!part_path(&path).exists());
        assert_eq!(
            framed_len(&header("passes-2-3"), 4),
            std::fs::metadata(&path).unwrap().len()
        );
        let (read, body) = read_framed(&path).unwrap();
        assert_eq!(read, header("passes-2-3"));
        assert_eq!(body, b"body");
        let bytes = std::fs::read(&path).unwrap();
        let mut changed = bytes.clone();
        changed[bytes.len() - TRAILER - 1] ^= 1;
        std::fs::write(&path, &changed).unwrap();
        assert!(read_framed(&path).is_err());
        std::fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
        assert!(read_framed(&path).is_err());
        std::fs::write(part_path(&path), b"torn").unwrap();
        assert!(
            write_framed(&path, &header("x"), b"").is_ok(),
            "a stale .part does not block a rewrite"
        );
    }

    #[test]
    fn checkpoint_names_follow_membership_not_order_of_writing() {
        let a = member_name("resolutions", [b"a".as_slice(), b"b"]);
        assert_eq!(a, member_name("resolutions", [b"a".as_slice(), b"b"]));
        assert_ne!(a, member_name("resolutions", [b"ab".as_slice()]));
        assert_ne!(a, member_name("records", [b"a".as_slice(), b"b"]));
    }

    #[test]
    fn a_stage_may_list_only_names_inside_its_workspace() {
        for good in ["runs/resolutions-0a1b", "records/records-ff.seg", "STAGE-x"] {
            assert!(is_member_name(good), "{good}");
        }
        for bad in [
            "",
            "/etc/passwd",
            "../outside",
            "runs/../../outside",
            "runs//x",
            "./x",
            "..\\outside",
            "C:/outside",
            "C:outside",
            "runs\\x",
        ] {
            assert!(!is_member_name(bad), "{bad}");
        }
        let dir = tempfile::tempdir().unwrap();
        let marker = Marker {
            header: Header {
                identity: [1; 32],
                snapshot: None,
                stage: RECORDS.to_string(),
                members: vec![b"..\\outside".to_vec()],
            },
            sums: vec![FileSum {
                len: 0,
                checksum: *blake3::hash(b"").as_bytes(),
            }],
        };
        assert!(verify_marker(dir.path(), &marker, [1; 32]).is_err());
    }

    #[test]
    fn attempts_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(Attempts::read(dir.path()).unwrap(), None);
        let attempts = Attempts {
            identity: [3; 32],
            build: "abc".to_string(),
            running: true,
            death_stage: 1,
            deaths: 2,
            fresh_used: true,
            restarting: true,
        };
        attempts.write(dir.path()).unwrap();
        assert_eq!(Attempts::read(dir.path()).unwrap(), Some(attempts));
        std::fs::write(dir.path().join(ATTEMPTS), b"garbage").unwrap();
        assert!(Attempts::read(dir.path()).is_err());
    }

    #[test]
    fn attempts_of_another_identity_or_a_vanished_workspace_no_longer_apply() {
        let used = Attempts {
            identity: [3; 32],
            build: "old".to_string(),
            running: true,
            death_stage: 2,
            deaths: 1,
            fresh_used: true,
            restarting: false,
        };
        let names = ["old".to_string()];
        let applying = |read: &Attempts, identity, names: &[String]| {
            Attempts::applying_to(Some(read.clone()), identity, names)
        };
        assert_eq!(applying(&used, [3; 32], &names), used, "still this build's");
        let fresh = Attempts {
            identity: [4; 32],
            ..Attempts::default()
        };
        assert_eq!(applying(&used, [4; 32], &names), fresh, "another identity");
        let reset = Attempts {
            identity: [3; 32],
            ..Attempts::default()
        };
        assert_eq!(
            applying(&used, [3; 32], &[]),
            reset,
            "its workspace expired"
        );
        let restarting = Attempts {
            restarting: true,
            ..used.clone()
        };
        assert_eq!(
            applying(&restarting, [3; 32], &[]),
            restarting,
            "a fresh start in progress keeps counting as used"
        );
        assert_eq!(
            Attempts::applying_to(None, [3; 32], &names),
            reset,
            "no record"
        );
    }
}
