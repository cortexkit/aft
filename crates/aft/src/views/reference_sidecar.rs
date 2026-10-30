//! Binary sidecars holding the blob keys each published manifest references.
//!
//! Storage-wide blob GC must know every key named by every manifest in every
//! view, and a manifest lists every file of its checkout. Decoding all of that
//! JSON on each retention pass dominated the pass and never finished on large
//! storage. Instead, `manifest-<generation>.keys` beside each manifest holds the
//! generation's keys as sorted, unique 32-byte records, so GC can search them
//! with positioned reads and keep only its candidate batch in memory.
//!
//! File layout (little endian):
//!
//! | bytes    | field                                                       |
//! |----------|-------------------------------------------------------------|
//! | 0..8     | magic `AFTREFK1`                                            |
//! | 8..12    | format version                                              |
//! | 12..16   | origin: 1 written by the publisher, 2 derived from the JSON |
//! | 16..24   | key count                                                   |
//! | 24..32   | time the keys were last proven to match, ns since 1970      |
//! | 32..96   | [`FileStamp`] of the manifest the keys came from            |
//! | 96..128  | BLAKE3 of the manifest's bytes                              |
//! | 128..160 | BLAKE3 of bytes 0..128 followed by every key                |
//! | 160..    | keys, strictly ascending                                    |
//!
//! Safety rule: a sidecar is used only when its header, size, checksum and
//! ordering are valid, the manifest's current stamp equals the recorded one,
//! and the keys provably belong to the manifest's current bytes. Timestamps
//! alone are not proof: a same-length replacement written within one tick of
//! the filesystem clock keeps size and times (and, through Windows creation-
//! time tunnelling, even the creation time). Proof is either the manifest's
//! content digest, re-read and compared, or, without reading it, a known file
//! identity plus timestamps that had settled long before the keys were last
//! proven. Anything else — no sidecar, a damaged one, a replaced manifest —
//! means the JSON is decoded again, and a new sidecar is written when the
//! manifest's stamp is usable, unchanged and settled and the directory has
//! room. A sidecar
//! problem can cost time; it can never make GC treat a referenced blob as
//! unreferenced.
use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, File},
    io::Read as _,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime},
};

const MAGIC: [u8; 8] = *b"AFTREFK1";
// Version 1 recorded no manifest digest; such sidecars are re-derived.
const VERSION: u32 = 2;
const HEADER_LEN: usize = 160;
const DIGEST_AT: usize = 96;
const CHECKSUM_AT: usize = 128;
const KEY_LEN: usize = 32;
const ORIGIN_PUBLISHER: u32 = 1;
const ORIGIN_DERIVED: u32 = 2;
/// Bytes read at a time while verifying a sidecar.
const VERIFY_CHUNK: usize = 64 * 1024;
/// Sidecars already verified by this process, keyed by path. Bounded so a
/// huge storage tree cannot grow it without limit; it is simply cleared when
/// full and sidecars are re-verified on their next use.
const MAX_VERIFIED: usize = 65_536;
/// A sidecar whose manifest's timestamps had settled for at least this long
/// before its keys were last proven can be trusted on the manifest's stamp and
/// file identity alone; any later change to that file then carries a newer
/// timestamp, even on a filesystem with coarse (one or two second) times.
/// Otherwise the manifest is re-read and its digest compared. The same window
/// decides when the sweep may derive a sidecar from a manifest at all.
pub(crate) const SETTLE: Duration = Duration::from_secs(5);

/// The name of the sidecar for `generation`'s manifest.
pub(crate) fn sidecar_name(generation: &str) -> String {
    format!("manifest-{generation}.keys")
}

/// Identity of a manifest file on disk: which file it is and the last time its
/// contents or metadata changed. Stored as plain numbers so it can be written
/// into a sidecar header and compared exactly.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileStamp {
    len: u64,
    modified: (i64, u32),
    /// Device and inode on Unix, volume serial number and file index on
    /// Windows (see `crate::file_identity`). A replacement is a different
    /// file, so this changes even when size and every time match.
    file: (u64, u64),
    /// Whether `file` was reported at all; zeros otherwise.
    identity_known: bool,
    /// Status-change time on unix, which no ordinary API can set back: renames,
    /// rewrites, link changes and timestamp edits all advance it. Creation time
    /// elsewhere, when the platform reports one.
    changed: Option<(i64, u32)>,
}

fn epoch_parts(time: SystemTime) -> Option<(i64, u32)> {
    let since = time.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    Some((i64::try_from(since.as_secs()).ok()?, since.subsec_nanos()))
}

fn parts_nanos((seconds, nanos): (i64, u32)) -> i128 {
    i128::from(seconds) * 1_000_000_000 + i128::from(nanos)
}

fn now_nanos() -> Option<i128> {
    epoch_parts(SystemTime::now()).map(parts_nanos)
}

impl FileStamp {
    /// The stamp of the file at `path`, following symlinks. `Ok(None)` when
    /// the platform cannot report a usable modification time (or reports one
    /// before 1970); such a file never gets a trusted sidecar.
    pub(crate) fn of_path(path: &Path) -> std::io::Result<Option<Self>> {
        let (metadata, identity) = crate::file_identity::metadata(path)?;
        Ok(Self::of(&metadata, identity))
    }

    /// The stamp of an open file.
    fn of_file(file: &File) -> Option<Self> {
        let metadata = file.metadata().ok()?;
        Self::of(&metadata, crate::file_identity::FileIdentity::of_file(file))
    }

    fn of(metadata: &fs::Metadata, identity: crate::file_identity::FileIdentity) -> Option<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        #[cfg(unix)]
        let changed = Some((metadata.ctime(), u32::try_from(metadata.ctime_nsec()).ok()?))
            .filter(|(seconds, _)| *seconds >= 0);
        #[cfg(not(unix))]
        let changed = metadata.created().ok().and_then(epoch_parts);
        #[cfg(unix)]
        if changed.is_none() {
            return None;
        }
        let (file, identity_known) = match identity {
            crate::file_identity::FileIdentity::Known { volume, index } => ((volume, index), true),
            crate::file_identity::FileIdentity::Unavailable => ((0, 0), false),
        };
        Some(Self {
            len: metadata.len(),
            modified: epoch_parts(metadata.modified().ok()?)?,
            file,
            identity_known,
            changed,
        })
    }

    /// Whether the file's timestamps were at least `settle` older than `at`.
    fn settled_by(&self, at: i128, settle: Duration) -> bool {
        let settle = settle.as_nanos() as i128;
        parts_nanos(self.modified) + settle <= at
            && self
                .changed
                .is_none_or(|changed| parts_nanos(changed) + settle <= at)
    }

    fn encode(&self) -> [u8; 64] {
        let mut out = [0_u8; 64];
        out[0..8].copy_from_slice(&self.len.to_le_bytes());
        out[8..16].copy_from_slice(&self.modified.0.to_le_bytes());
        out[16..20].copy_from_slice(&self.modified.1.to_le_bytes());
        out[20..28].copy_from_slice(&self.file.0.to_le_bytes());
        out[28..36].copy_from_slice(&self.file.1.to_le_bytes());
        if let Some((seconds, nanos)) = self.changed {
            out[36..44].copy_from_slice(&seconds.to_le_bytes());
            out[44..48].copy_from_slice(&nanos.to_le_bytes());
            out[48] = 1;
        }
        out[49] = u8::from(self.identity_known);
        out
    }
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes.try_into().expect("four bytes"))
}

fn le_u64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes.try_into().expect("eight bytes"))
}

/// Keys in strictly ascending order that can be read by position, either from
/// memory or from a sidecar file.
pub(crate) trait SortedKeys {
    fn len(&self) -> usize;
    /// Fill `out` with the keys starting at index `start`.
    fn read(&self, start: usize, out: &mut [[u8; 32]]) -> std::io::Result<()>;
}

impl SortedKeys for Vec<[u8; 32]> {
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn read(&self, start: usize, out: &mut [[u8; 32]]) -> std::io::Result<()> {
        let end = start
            .checked_add(out.len())
            .filter(|end| *end <= Vec::len(self))
            .ok_or_else(|| std::io::Error::other("sorted key read out of range"))?;
        out.copy_from_slice(&self[start..end]);
        Ok(())
    }
}

/// A verified sidecar, read with positioned reads through the handle that was
/// verified, so a concurrent rewrite (which replaces the name, not the file)
/// cannot change what this pass sees.
pub(crate) struct SidecarKeys {
    file: File,
    count: usize,
}

fn read_exact_at(file: &File, buffer: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(file, buffer, offset)
    }
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < buffer.len() {
            let read = std::os::windows::fs::FileExt::seek_read(
                file,
                &mut buffer[done..],
                offset + done as u64,
            )?;
            if read == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            done += read;
        }
        Ok(())
    }
}

impl SortedKeys for SidecarKeys {
    fn len(&self) -> usize {
        self.count
    }
    fn read(&self, start: usize, out: &mut [[u8; 32]]) -> std::io::Result<()> {
        if start
            .checked_add(out.len())
            .is_none_or(|end| end > self.count)
        {
            return Err(std::io::Error::other("sidecar read out of range"));
        }
        let offset = (HEADER_LEN + start * KEY_LEN) as u64;
        read_exact_at(&self.file, out.as_flattened_mut(), offset)
    }
}

/// Where a generation's keys came from on one visit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum KeySource {
    Sidecar,
    /// The JSON was decoded; `written` says whether a sidecar was saved.
    Decoded {
        written: bool,
    },
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct Verified {
    sidecar: FileStamp,
    manifest: FileStamp,
    count: usize,
}

pub(crate) struct ReferenceSidecars {
    settle: Duration,
    /// Re-hashing every sidecar on every pass would read all of them in full;
    /// a sidecar whose own stamp is unchanged since it was verified is not.
    verified: Mutex<HashMap<PathBuf, Verified>>,
    /// How old a sidecar's own timestamps must be before its verification is
    /// remembered. Where timestamps are coarse (Windows, Linux before
    /// multigrain timestamps, HFS+), a sidecar rewritten in place within the
    /// same tick as its verification keeps its stamp, so remembering a young
    /// sidecar could serve bytes that were never checked. This is the same
    /// reasoning as the settle window for manifests, and it does not depend on
    /// that policy, which tests set to zero.
    memo_settle: Duration,
    /// The view-directory size sidecar writes must stay within.
    directory_limit: usize,
    /// [`SETTLE`], the window that lets a sidecar be trusted on its
    /// manifest's stamp alone; tests shorten it to exercise promotion.
    proof_settle: Duration,
}

pub(crate) fn global() -> &'static ReferenceSidecars {
    static SIDECARS: OnceLock<ReferenceSidecars> = OnceLock::new();
    SIDECARS.get_or_init(|| ReferenceSidecars::new(SETTLE))
}

fn encode(
    stamp: &FileStamp,
    origin: u32,
    proven_at: i128,
    manifest_digest: &[u8; 32],
    keys: &[[u8; 32]],
) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_LEN + keys.len() * KEY_LEN);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&origin.to_le_bytes());
    bytes.extend_from_slice(&(keys.len() as u64).to_le_bytes());
    bytes.extend_from_slice(&(u64::try_from(proven_at).unwrap_or(0)).to_le_bytes());
    bytes.extend_from_slice(&stamp.encode());
    bytes.extend_from_slice(manifest_digest);
    let mut hasher = blake3::Hasher::new();
    hasher.update(&bytes);
    hasher.update(keys.as_flattened());
    bytes.extend_from_slice(hasher.finalize().as_bytes());
    bytes.extend_from_slice(keys.as_flattened());
    bytes
}

/// The digest a sidecar records for its manifest's bytes.
pub(crate) fn manifest_digest(bytes: &[u8]) -> [u8; 32] {
    *blake3::hash(bytes).as_bytes()
}

/// How many more entries `view` can take without exceeding `limit`, the
/// directory size that blob scans and generation cleanup examine in one pass.
/// One bounded listing; counting stops just past the limit.
pub(crate) fn free_slots(view: &Path, limit: usize) -> usize {
    #[cfg(test)]
    super::generation::note_directory_listing(view);
    let Ok(entries) = fs::read_dir(view) else {
        return 0;
    };
    limit.saturating_sub(entries.take(limit.saturating_add(1)).count())
}

/// Write `keys` (sorted, unique) as the sidecar of the manifest at
/// `view/manifest-<generation>.json` whose stamp is `stamp` and whose bytes
/// hash to `manifest_digest`. `slots` is the caller's running count of entries
/// the directory can still take; a written sidecar spends one, so a scan
/// counts the directory only once.
#[allow(clippy::too_many_arguments)]
fn write_sidecar(
    view: &Path,
    generation: &str,
    stamp: &FileStamp,
    origin: u32,
    proven_at: i128,
    manifest_digest: &[u8; 32],
    keys: &[[u8; 32]],
    slots: &mut usize,
) -> std::io::Result<()> {
    let path = view.join(sidecar_name(generation));
    crate::production_storage::refuse_write(&path)?;
    // The write briefly adds a temporary file and, the first time, the
    // sidecar itself. A sidecar must never be what pushes a view directory
    // over the limit: blob marking would then refuse the whole directory.
    // Without one, blob GC decodes this manifest's JSON instead.
    let new = !path.exists();
    let needed = if new { 2 } else { 1 };
    if *slots < needed {
        return Err(std::io::Error::other(
            "view directory is at the generation directory limit; sidecar not written",
        ));
    }
    // The sidecar is rebuildable from the manifest, so it needs a synced file
    // before the rename but not a directory sync of its own.
    crate::durability::atomic_write(
        &path,
        &encode(stamp, origin, proven_at, manifest_digest, keys),
        crate::durability::AtomicWriteDurability::File,
    )?;
    // The temporary file is gone again; a new sidecar keeps one entry.
    if new {
        *slots -= 1;
    }
    // Generation cleanup may have retired the manifest while the keys were
    // being written. A sidecar must not outlive its manifest, so take it back
    // out when the manifest is no longer the file it describes.
    let manifest = view.join(format!("manifest-{generation}.json"));
    let current = FileStamp::of_path(&manifest).ok().flatten();
    if current != Some(*stamp) {
        let _ = fs::remove_file(&path);
        return Err(std::io::Error::other(
            "manifest changed or was removed while its sidecar was written",
        ));
    }
    Ok(())
}

/// Record the keys of a manifest this process has just published.
/// `manifest_digest` is the digest of the exact bytes the publisher wrote, and
/// `keys` come from the in-memory manifest those bytes were serialized from,
/// so the two describe the same content by construction. Readers compare the
/// digest with the file until its timestamps have settled.
pub(crate) fn write_published(
    view: &Path,
    generation: &str,
    keys: &BTreeSet<[u8; 32]>,
    manifest_digest: &[u8; 32],
) -> std::io::Result<()> {
    let manifest = view.join(format!("manifest-{generation}.json"));
    let stamp = FileStamp::of_path(&manifest)?
        .ok_or_else(|| std::io::Error::other("manifest has no usable timestamp"))?;
    let proven_at = now_nanos().ok_or_else(|| std::io::Error::other("clock before 1970"))?;
    let keys: Vec<[u8; 32]> = keys.iter().copied().collect();
    write_sidecar(
        view,
        generation,
        &stamp,
        ORIGIN_PUBLISHER,
        proven_at,
        manifest_digest,
        &keys,
        &mut free_slots(view, super::generation::GENERATION_DIRECTORY_LIMIT),
    )
}

impl ReferenceSidecars {
    pub(crate) fn new(settle: Duration) -> Self {
        Self {
            settle,
            verified: Mutex::new(HashMap::new()),
            memo_settle: SETTLE,
            directory_limit: super::generation::GENERATION_DIRECTORY_LIMIT,
            proof_settle: SETTLE,
        }
    }

    #[cfg(test)]
    fn with_proof_settle(settle: Duration, proof_settle: Duration) -> Self {
        Self {
            proof_settle,
            ..Self::new(settle)
        }
    }

    #[cfg(test)]
    pub(crate) fn with_directory_limit(settle: Duration, directory_limit: usize) -> Self {
        Self {
            directory_limit,
            ..Self::new(settle)
        }
    }

    #[cfg(test)]
    fn with_memo_settle(settle: Duration, memo_settle: Duration) -> Self {
        Self {
            memo_settle,
            ..Self::new(settle)
        }
    }

    fn verified(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, Verified>> {
        self.verified
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Forget which sidecars were verified, so each is fully re-checked on its
    /// next use.
    pub(crate) fn forget_verified(&self) {
        self.verified().clear();
    }

    /// Entries a view directory holding `listed` entries can still take for
    /// new sidecars during one scan.
    pub(crate) fn scan_slots(&self, listed: usize) -> usize {
        self.directory_limit.saturating_sub(listed)
    }

    /// Show `generation`'s keys to `visit` exactly once, from its sidecar when
    /// that is valid for the manifest on disk, and otherwise by decoding the
    /// manifest JSON with `decode` (and saving a sidecar for next time, if
    /// `free_slots` leaves room for it).
    pub(crate) fn visit(
        &self,
        view: &Path,
        generation: &str,
        decode: impl FnOnce(&[u8]) -> super::Result<BTreeSet<[u8; 32]>>,
        visit: &mut dyn FnMut(&dyn SortedKeys) -> std::io::Result<()>,
        free_slots: &mut usize,
    ) -> super::Result<KeySource> {
        let manifest = view.join(format!("manifest-{generation}.json"));
        let stamp = FileStamp::of_path(&manifest)?;
        if let Some(stamp) = &stamp {
            let sidecar = view.join(sidecar_name(generation));
            if let Some(trusted) = self.open_trusted(&sidecar, &manifest, stamp) {
                visit(&trusted.keys)?;
                if let Some(digest) = trusted.promote {
                    self.promote(view, generation, stamp, &digest, &trusted.keys, free_slots);
                }
                return Ok(KeySource::Sidecar);
            }
        }
        let bytes = fs::read(&manifest)?;
        let digest = manifest_digest(&bytes);
        let keys: Vec<[u8; 32]> = decode(&bytes)?.into_iter().collect();
        drop(bytes);
        visit(&keys)?;
        // Save the keys only if the file kept one identity across the read and
        // is past the settle window; otherwise a later pass decodes it again.
        let Some(stamp) = stamp else {
            return Ok(KeySource::Decoded { written: false });
        };
        let unchanged = FileStamp::of_path(&manifest).ok().flatten() == Some(stamp);
        let Some(now) = now_nanos() else {
            return Ok(KeySource::Decoded { written: false });
        };
        if !unchanged || !stamp.settled_by(now, self.settle) {
            return Ok(KeySource::Decoded { written: false });
        }
        let written = match write_sidecar(
            view,
            generation,
            &stamp,
            ORIGIN_DERIVED,
            now,
            &digest,
            &keys,
            free_slots,
        ) {
            Ok(()) => true,
            Err(error) => {
                log::debug!(
                    "views: reference sidecar not written view={} generation={generation} error={error}",
                    view.display()
                );
                false
            }
        };
        Ok(KeySource::Decoded { written })
    }

    /// Rewrite a sidecar whose keys were just proven by digest against a
    /// manifest whose timestamps have since settled, recording the new proof
    /// time so later visits can trust it without re-reading the manifest.
    fn promote(
        &self,
        view: &Path,
        generation: &str,
        stamp: &FileStamp,
        digest: &[u8; 32],
        keys: &SidecarKeys,
        free_slots: &mut usize,
    ) {
        let Some(now) = now_nanos() else {
            return;
        };
        let mut all = vec![[0_u8; 32]; keys.len()];
        if keys.read(0, &mut all).is_err() {
            return;
        }
        if let Err(error) = write_sidecar(
            view,
            generation,
            stamp,
            ORIGIN_DERIVED,
            now,
            digest,
            &all,
            free_slots,
        ) {
            log::debug!(
                "views: reference sidecar not promoted view={} generation={generation} error={error}",
                view.display()
            );
        }
    }

    /// The sidecar at `path`, if it is intact and provably describes the
    /// manifest at `manifest_path`, whose current stamp is `manifest`. Every
    /// failure means "decode the JSON".
    fn open_trusted(
        &self,
        path: &Path,
        manifest_path: &Path,
        manifest: &FileStamp,
    ) -> Option<Trusted> {
        let mut file = File::open(path).ok()?;
        let sidecar = FileStamp::of_file(&file)?;
        if let Some(verified) = self.verified().get(path) {
            if verified.sidecar == sidecar && verified.manifest == *manifest {
                return Some(Trusted {
                    keys: SidecarKeys {
                        file,
                        count: verified.count,
                    },
                    promote: None,
                });
            }
        }
        let mut header = [0_u8; HEADER_LEN];
        file.read_exact(&mut header).ok()?;
        if header[0..8] != MAGIC || le_u32(&header[8..12]) != VERSION {
            return None;
        }
        if !matches!(le_u32(&header[12..16]), ORIGIN_PUBLISHER | ORIGIN_DERIVED) {
            return None;
        }
        let count = usize::try_from(le_u64(&header[16..24])).ok()?;
        let proven_at = i128::from(le_u64(&header[24..32]));
        if header[32..DIGEST_AT] != manifest.encode() {
            return None;
        }
        let recorded_digest: [u8; 32] = header[DIGEST_AT..CHECKSUM_AT].try_into().ok()?;
        let expected_len = count.checked_mul(KEY_LEN)?.checked_add(HEADER_LEN)? as u64;
        if sidecar.len != expected_len {
            return None;
        }
        // Stream the keys once: the checksum catches damage, and the order
        // check guarantees the searches GC runs over this file are sound.
        let mut hasher = blake3::Hasher::new();
        hasher.update(&header[..CHECKSUM_AT]);
        let mut buffer = vec![0_u8; VERIFY_CHUNK];
        let mut previous: Option<[u8; 32]> = None;
        let mut remaining = count * KEY_LEN;
        while remaining > 0 {
            let take = remaining.min(VERIFY_CHUNK);
            file.read_exact(&mut buffer[..take]).ok()?;
            hasher.update(&buffer[..take]);
            for key in buffer[..take].chunks_exact(KEY_LEN) {
                let key: [u8; 32] = key.try_into().ok()?;
                if previous.is_some_and(|previous| previous >= key) {
                    return None;
                }
                previous = Some(key);
            }
            remaining -= take;
        }
        if hasher.finalize().as_bytes()[..] != header[CHECKSUM_AT..HEADER_LEN] {
            return None;
        }
        // Proof that these keys belong to the manifest's current bytes. A
        // matching stamp is proof only if the file's identity is known and its
        // times had settled before the keys were last proven: any later change
        // to that file would then carry newer times. Otherwise (a sidecar
        // written right after its manifest, or no file identity), re-read the
        // manifest and compare its digest.
        let now = now_nanos()?;
        let by_stamp = manifest.identity_known && manifest.settled_by(proven_at, self.proof_settle);
        let mut promote = None;
        if !by_stamp {
            let bytes = fs::read(manifest_path).ok()?;
            if manifest_digest(&bytes) != recorded_digest {
                return None;
            }
            // The digest describes the bytes read only if the file kept one
            // identity across the read.
            if FileStamp::of_path(manifest_path).ok()?? != *manifest {
                return None;
            }
            if manifest.identity_known && manifest.settled_by(now, self.proof_settle) {
                promote = Some(recorded_digest);
            }
        }
        // Remember the verification only for a sidecar and a manifest whose
        // stamps would both change on any later write (see `memo_settle`).
        let memorable = sidecar.settled_by(now, self.memo_settle)
            && (by_stamp
                || (manifest.identity_known && manifest.settled_by(now, self.memo_settle)));
        let mut verified = self.verified();
        if !memorable {
            // Verified for this visit only; checked again on the next one.
            verified.remove(path);
        } else {
            if verified.len() >= MAX_VERIFIED {
                verified.clear();
            }
            verified.insert(
                path.to_path_buf(),
                Verified {
                    sidecar,
                    manifest: *manifest,
                    count,
                },
            );
        }
        Some(Trusted {
            keys: SidecarKeys { file, count },
            promote,
        })
    }
}

/// A sidecar proven to describe its manifest. `promote` carries the manifest
/// digest when the proof needed a re-read and the manifest has settled since,
/// so the sidecar can be rewritten with a fresh proof time.
struct Trusted {
    keys: SidecarKeys,
    promote: Option<[u8; 32]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test manifests are raw concatenated keys; decoding them is trivial so
    /// these tests exercise only the sidecar rules.
    fn decode(bytes: &[u8]) -> super::super::Result<BTreeSet<[u8; 32]>> {
        Ok(bytes
            .chunks_exact(KEY_LEN)
            .map(|key| key.try_into().unwrap())
            .collect())
    }

    fn keys_of(bytes: &[u8]) -> Vec<[u8; 32]> {
        decode(bytes).unwrap().into_iter().collect()
    }

    /// Replace through a rename: the new file gets a new inode while the old
    /// one still exists.
    fn write_manifest(view: &Path, generation: &str, bytes: &[u8]) {
        let staging = view.join("staging.tmp");
        fs::write(&staging, bytes).unwrap();
        fs::rename(&staging, view.join(format!("manifest-{generation}.json"))).unwrap();
    }

    fn visit_keys(
        sidecars: &ReferenceSidecars,
        view: &Path,
        generation: &str,
    ) -> (KeySource, Vec<[u8; 32]>) {
        let mut seen = Vec::new();
        let source = sidecars
            .visit(
                view,
                generation,
                decode,
                &mut |keys| {
                    seen = vec![[0_u8; 32]; keys.len()];
                    keys.read(0, &mut seen)
                },
                &mut free_slots(view, sidecars.directory_limit),
            )
            .unwrap();
        (source, seen)
    }

    fn manifest_bytes() -> Vec<u8> {
        // Deliberately unsorted and with a duplicate.
        [[9_u8; 32], [1; 32], [5; 32], [1; 32]].concat()
    }

    #[test]
    fn reference_sidecar_round_trips_and_is_reused() {
        let view = tempfile::tempdir().unwrap();
        let sidecars = ReferenceSidecars::new(Duration::ZERO);
        write_manifest(view.path(), "g", &manifest_bytes());
        let expected = keys_of(&manifest_bytes());
        assert_eq!(
            visit_keys(&sidecars, view.path(), "g"),
            (KeySource::Decoded { written: true }, expected.clone())
        );
        let sidecar = view.path().join(sidecar_name("g"));
        assert_eq!(
            fs::metadata(&sidecar).unwrap().len() as usize,
            HEADER_LEN + expected.len() * KEY_LEN
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777,
                crate::private_storage::FILE_MODE
            );
        }
        assert_eq!(
            visit_keys(&sidecars, view.path(), "g"),
            (KeySource::Sidecar, expected.clone())
        );
        // A fresh process (no verification memo) re-verifies and reuses it.
        sidecars.forget_verified();
        assert_eq!(
            visit_keys(&sidecars, view.path(), "g"),
            (KeySource::Sidecar, expected)
        );
    }

    #[test]
    fn reference_sidecar_is_never_written_inside_the_settle_window_and_is_trusted_by_digest() {
        let view = tempfile::tempdir().unwrap();
        let strict = ReferenceSidecars::new(Duration::from_secs(3600));
        write_manifest(view.path(), "g", &manifest_bytes());
        let expected = keys_of(&manifest_bytes());
        assert_eq!(
            visit_keys(&strict, view.path(), "g"),
            (KeySource::Decoded { written: false }, expected.clone())
        );
        assert!(!view.path().join(sidecar_name("g")).exists());
        // A sidecar written moments after its manifest changed cannot be
        // trusted on timestamps, but it is proven by the manifest's digest.
        let eager = ReferenceSidecars::new(Duration::ZERO);
        assert_eq!(
            visit_keys(&eager, view.path(), "g").0,
            KeySource::Decoded { written: true }
        );
        assert_eq!(
            visit_keys(&strict, view.path(), "g"),
            (KeySource::Sidecar, expected)
        );
    }

    #[test]
    fn reference_sidecar_proven_by_digest_is_promoted_once_its_manifest_settles() {
        let view = tempfile::tempdir().unwrap();
        let sidecars = ReferenceSidecars::with_proof_settle(Duration::ZERO, Duration::ZERO);
        write_manifest(view.path(), "g", &manifest_bytes());
        let stamp = FileStamp::of_path(&view.path().join("manifest-g.json"))
            .unwrap()
            .unwrap();
        // A proof time before the manifest's last change: only the digest can
        // prove this sidecar.
        let sidecar = view.path().join(sidecar_name("g"));
        fs::write(
            &sidecar,
            encode(
                &stamp,
                ORIGIN_PUBLISHER,
                0,
                &manifest_digest(&manifest_bytes()),
                &keys_of(&manifest_bytes()),
            ),
        )
        .unwrap();
        assert_eq!(
            visit_keys(&sidecars, view.path(), "g"),
            (KeySource::Sidecar, keys_of(&manifest_bytes()))
        );
        // Rewritten with a fresh proof time and the same keys and digest.
        let header = fs::read(&sidecar).unwrap();
        assert!(le_u64(&header[24..32]) > 0);
        assert_eq!(le_u32(&header[12..16]), ORIGIN_DERIVED);
        assert_eq!(
            header[DIGEST_AT..CHECKSUM_AT],
            manifest_digest(&manifest_bytes())
        );
        assert_eq!(
            visit_keys(&sidecars, view.path(), "g"),
            (KeySource::Sidecar, keys_of(&manifest_bytes()))
        );
    }

    #[test]
    fn reference_sidecar_with_a_matching_stamp_is_trusted_only_for_the_bytes_it_was_proven_against()
    {
        // A sidecar whose recorded stamp equals the manifest's current stamp,
        // but whose keys and digest belong to other bytes. This is what a
        // same-size rewrite that no recorded field reveals would look like;
        // constructing it directly makes the digest check the only defence on
        // every platform.
        let view = tempfile::tempdir().unwrap();
        let sidecars = ReferenceSidecars::new(Duration::ZERO);
        let current = [[2_u8; 32], [3; 32], [4; 32], [6; 32]].concat();
        write_manifest(view.path(), "g", &current);
        let stamp = FileStamp::of_path(&view.path().join("manifest-g.json"))
            .unwrap()
            .unwrap();
        let stale = encode(
            &stamp,
            ORIGIN_PUBLISHER,
            now_nanos().unwrap(),
            &manifest_digest(&manifest_bytes()),
            &keys_of(&manifest_bytes()),
        );
        fs::write(view.path().join(sidecar_name("g")), stale).unwrap();
        let (source, keys) = visit_keys(&sidecars, view.path(), "g");
        assert_eq!(keys, keys_of(&current));
        assert_ne!(source, KeySource::Sidecar);
    }

    #[test]
    fn reference_sidecar_from_the_publisher_is_trusted_at_once() {
        let view = tempfile::tempdir().unwrap();
        let strict = ReferenceSidecars::new(Duration::from_secs(3600));
        write_manifest(view.path(), "g", &manifest_bytes());
        write_published(
            view.path(),
            "g",
            &decode(&manifest_bytes()).unwrap(),
            &manifest_digest(&manifest_bytes()),
        )
        .unwrap();
        assert_eq!(
            visit_keys(&strict, view.path(), "g"),
            (KeySource::Sidecar, keys_of(&manifest_bytes()))
        );
    }

    #[test]
    fn reference_sidecar_from_the_publisher_is_dropped_when_the_manifest_is_replaced() {
        let view = tempfile::tempdir().unwrap();
        let strict = ReferenceSidecars::new(Duration::from_secs(3600));
        write_manifest(view.path(), "g", &manifest_bytes());
        write_published(
            view.path(),
            "g",
            &decode(&manifest_bytes()).unwrap(),
            &manifest_digest(&manifest_bytes()),
        )
        .unwrap();
        assert_eq!(visit_keys(&strict, view.path(), "g").0, KeySource::Sidecar);
        // Same name and length, different keys, new file: a publisher sidecar
        // is trusted without a settle window, so only the recorded manifest
        // stamp can reveal that it now describes another file.
        let replacement = [[2_u8; 32], [3; 32], [4; 32], [6; 32]].concat();
        assert_eq!(replacement.len(), manifest_bytes().len());
        write_manifest(view.path(), "g", &replacement);
        assert_eq!(
            visit_keys(&strict, view.path(), "g"),
            (KeySource::Decoded { written: false }, keys_of(&replacement))
        );
    }

    #[test]
    fn reference_sidecar_memo_never_remembers_a_sidecar_inside_the_settle_window() {
        let view = tempfile::tempdir().unwrap();
        // Trust and write sidecars at once; only the memo rule is under test.
        let sidecars = ReferenceSidecars::new(Duration::ZERO);
        write_manifest(view.path(), "g", &manifest_bytes());
        assert_eq!(
            visit_keys(&sidecars, view.path(), "g").0,
            KeySource::Decoded { written: true }
        );
        assert_eq!(
            visit_keys(&sidecars, view.path(), "g").0,
            KeySource::Sidecar
        );
        // The sidecar was fully verified, but it changed moments ago: on a
        // filesystem with coarse timestamps an in-place rewrite within the
        // same tick would keep its stamp, so it must not be remembered.
        assert_eq!(sidecars.verified().len(), 0);
        // Once the sidecar is old enough, its verification is remembered.
        let settled = ReferenceSidecars::with_memo_settle(Duration::ZERO, Duration::ZERO);
        assert_eq!(visit_keys(&settled, view.path(), "g").0, KeySource::Sidecar);
        assert_eq!(settled.verified().len(), 1);
        assert_eq!(
            visit_keys(&settled, view.path(), "g"),
            (KeySource::Sidecar, keys_of(&manifest_bytes()))
        );
    }

    #[test]
    fn reference_sidecar_damaged_in_place_with_its_timestamps_restored_is_re_derived() {
        let view = tempfile::tempdir().unwrap();
        let sidecars = ReferenceSidecars::new(Duration::ZERO);
        write_manifest(view.path(), "g", &manifest_bytes());
        let expected = keys_of(&manifest_bytes());
        visit_keys(&sidecars, view.path(), "g");
        assert_eq!(
            visit_keys(&sidecars, view.path(), "g").0,
            KeySource::Sidecar
        );
        // Damage the last key in place, keeping the length, then put the
        // modification time back. Where the stamp has no change time
        // (Windows), this is what a same-tick rewrite looks like.
        let sidecar = view.path().join(sidecar_name("g"));
        let modified = fs::metadata(&sidecar).unwrap().modified().unwrap();
        let mut bytes = fs::read(&sidecar).unwrap();
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&sidecar, &bytes).unwrap();
        filetime::set_file_mtime(&sidecar, filetime::FileTime::from_system_time(modified)).unwrap();
        let (source, keys) = visit_keys(&sidecars, view.path(), "g");
        assert_ne!(source, KeySource::Sidecar);
        assert_eq!(keys, expected);
    }

    /// Rewrite `path` in place with `contents` of the same length and put its
    /// modification (and, on Windows, creation) time back. The file identity
    /// is unchanged. On Windows every recorded field then matches the original;
    /// on Unix the status-change time still moves.
    fn rewrite_in_place_keeping_times(path: &Path, contents: &[u8]) {
        use std::io::Write as _;
        let metadata = fs::metadata(path).unwrap();
        assert_eq!(metadata.len(), contents.len() as u64);
        let mut file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.write_all(contents).unwrap();
        let times = fs::FileTimes::new()
            .set_accessed(metadata.accessed().unwrap())
            .set_modified(metadata.modified().unwrap());
        #[cfg(windows)]
        let times = {
            use std::os::windows::fs::FileTimesExt;
            times.set_created(metadata.created().unwrap())
        };
        file.set_times(times).unwrap();
        drop(file);
        let after = fs::metadata(path).unwrap();
        assert_eq!(after.modified().unwrap(), metadata.modified().unwrap());
    }

    #[test]
    fn reference_sidecar_rejects_a_same_size_same_mtime_manifest_with_different_keys() {
        let replacement = [[2_u8; 32], [3; 32], [4; 32], [6; 32]].concat();
        assert_eq!(replacement.len(), manifest_bytes().len());
        type Change = fn(&Path, &[u8]);
        let changes: [(&str, Change); 2] = [
            (
                "replaced keeping times",
                crate::file_identity::replace_keeping_times,
            ),
            (
                "rewritten in place keeping times",
                rewrite_in_place_keeping_times,
            ),
        ];
        for (label, change) in changes {
            let view = tempfile::tempdir().unwrap();
            let sidecars = ReferenceSidecars::new(Duration::ZERO);
            write_manifest(view.path(), "g", &manifest_bytes());
            assert_eq!(
                visit_keys(&sidecars, view.path(), "g").0,
                KeySource::Decoded { written: true },
                "{label}"
            );
            assert_eq!(
                visit_keys(&sidecars, view.path(), "g").0,
                KeySource::Sidecar,
                "{label}"
            );
            change(&view.path().join("manifest-g.json"), &replacement);
            let (source, keys) = visit_keys(&sidecars, view.path(), "g");
            assert_eq!(keys, keys_of(&replacement), "{label}");
            assert_ne!(source, KeySource::Sidecar, "{label}");
        }
    }

    #[test]
    fn reference_sidecar_damage_is_re_derived_never_read_as_empty() {
        let view = tempfile::tempdir().unwrap();
        let sidecars = ReferenceSidecars::new(Duration::ZERO);
        write_manifest(view.path(), "g", &manifest_bytes());
        let expected = keys_of(&manifest_bytes());
        let sidecar = view.path().join(sidecar_name("g"));
        let stamp = FileStamp::of_path(&view.path().join("manifest-g.json"))
            .unwrap()
            .unwrap();
        let now = now_nanos().unwrap();
        let digest = manifest_digest(&manifest_bytes());
        let valid = encode(&stamp, ORIGIN_DERIVED, now, &digest, &expected);
        let mut unsorted_keys = expected.clone();
        unsorted_keys.reverse();
        let damaged: Vec<(&str, Vec<u8>)> = vec![
            ("flipped key byte", {
                let mut bytes = valid.clone();
                *bytes.last_mut().unwrap() ^= 1;
                bytes
            }),
            ("flipped derived-time byte", {
                let mut bytes = valid.clone();
                bytes[25] ^= 1;
                bytes
            }),
            ("truncated", valid[..valid.len() - KEY_LEN].to_vec()),
            ("count rewritten to zero", {
                let mut bytes = valid.clone();
                bytes[16..24].copy_from_slice(&0_u64.to_le_bytes());
                bytes
            }),
            ("bad magic", {
                let mut bytes = valid.clone();
                bytes[0] = b'X';
                bytes
            }),
            ("future version", {
                let mut bytes = valid.clone();
                bytes[8..12].copy_from_slice(&(VERSION + 1).to_le_bytes());
                bytes
            }),
            ("unknown origin", encode(&stamp, 7, now, &digest, &expected)),
            (
                "unsorted with a valid checksum",
                encode(&stamp, ORIGIN_DERIVED, now, &digest, &unsorted_keys),
            ),
            ("empty file", Vec::new()),
        ];
        for (label, bytes) in damaged {
            fs::write(&sidecar, &bytes).unwrap();
            assert_eq!(
                visit_keys(&sidecars, view.path(), "g"),
                (KeySource::Decoded { written: true }, expected.clone()),
                "{label}"
            );
            assert_eq!(
                visit_keys(&sidecars, view.path(), "g"),
                (KeySource::Sidecar, expected.clone()),
                "{label}: rewritten"
            );
        }
    }

    #[test]
    fn sidecar_positioned_reads_return_the_stored_keys() {
        let view = tempfile::tempdir().unwrap();
        let sidecars = ReferenceSidecars::new(Duration::ZERO);
        let keys: Vec<[u8; 32]> = (0..1000_u32)
            .map(|index| {
                let mut key = [0_u8; 32];
                key[..4].copy_from_slice(&index.to_be_bytes());
                key
            })
            .collect();
        write_manifest(view.path(), "g", &keys.concat());
        visit_keys(&sidecars, view.path(), "g");
        // Room is not under test here.
        let mut unlimited = usize::MAX;
        sidecars
            .visit(
                view.path(),
                "g",
                decode,
                &mut |run| {
                    assert_eq!(run.len(), keys.len());
                    let mut window = [[0_u8; 32]; 7];
                    run.read(500, &mut window)?;
                    assert_eq!(window, keys[500..507]);
                    assert!(run.read(995, &mut window).is_err());
                    Ok(())
                },
                &mut unlimited,
            )
            .unwrap();
    }
}
