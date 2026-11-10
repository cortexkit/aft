//! Integrity pin for the ONNX Runtime library AFT loads at run time.
//!
//! AFT does not link ONNX Runtime: it loads the shared library by path
//! (`dlopen` / `LoadLibrary`) once semantic search or the reranker first needs
//! it. The usual path is the runtime AFT's installer downloaded into a
//! user-writable folder (`<storage>/onnxruntime/<version>/`).
//!
//! On macOS the supervised module binary is signed with hardened runtime and
//! the `com.apple.security.cs.disable-library-validation` entitlement. Hardened
//! runtime is what keeps other processes of the same user from attaching a
//! debugger and reading the module's launch secret; the entitlement is needed
//! because Microsoft's library carries no Team ID and library validation would
//! refuse it. With library validation off, the loader no longer checks who
//! signed the file, so without this pin any same-user process could replace
//! the downloaded library and run its code inside AFT.
//!
//! The rule enforced here, on every platform, before any load:
//! - a library whose sha256 is on [`PINNED_ORT_LIBRARIES`] is accepted;
//! - otherwise the library is accepted only when `ORT_DYLIB_PATH` named it in
//!   the environment this process was spawned with, and the path is outside
//!   AFT's own managed runtime folder. That is the operator's explicit choice
//!   (a Homebrew or distro runtime), accepted unpinned and logged once. A value
//!   AFT exported itself after resolving its managed download never counts:
//!   the spawn-time value is captured before AFT sets anything;
//! - anything else is refused with `onnx runtime library hash mismatch at
//!   <path>` and is never handed to the loader.
//!
//! With no path at all the library would be found by bare name on the loader
//! search path, where there is no file to hash first. The supervised module
//! (`aft --subc`), the only mode that holds a launch secret, refuses that;
//! a standalone process keeps it, logged once as unpinned, so users relying on
//! a system-installed runtime keep working.
//!
//! # Hashing and loading the same file
//!
//! Hashing a path and then loading the same path leaves a window: a process
//! that can write the folder could replace the file after the hash matched and
//! before the loader opened it. A pinned library is therefore opened once,
//! hashed through that open file, and loaded through the same open file, so
//! the loader maps the file that was hashed whatever the path names by then:
//! - macOS: `dlopen("/dev/fd/<n>")`. Opening `/dev/fd/<n>` duplicates the
//!   descriptor, so dyld maps the inode that was hashed. Measured on macOS 27
//!   with the 1.24.4 library, from a plain binary and from one signed with
//!   hardened runtime and `disable-library-validation`: the load succeeds, a
//!   second `dlopen` of the same `/dev/fd` path returns the same handle, and
//!   renaming another library over the original path between `open` and
//!   `dlopen` still loads the hashed 1.24.4 file (the replacement's
//!   constructor never runs; loading the same swap by path runs it). dyld
//!   records the file's real path for the image (`dladdr` names it), so
//!   `@loader_path` still resolves as for a load by path.
//! - Linux: `dlopen("/proc/self/fd/<n>")`, which reopens the inode behind the
//!   descriptor.
//! - Windows: the file stays open with only read sharing (no write or delete
//!   sharing) until the load is over, so it cannot be rewritten, replaced,
//!   renamed or deleted, and `LoadLibrary` of its path loads the bytes that
//!   were hashed.
//!
//! Residual, macOS and Linux: the same user can still rewrite the hashed file
//! in place (same inode) between the hash and the moment the loader maps it;
//! no user-space lock stops a same-user writer. After loading, the file is
//! checked again (same device, inode, size, change time, and a fresh sha256
//! through the open file) and the load is reported as failed if anything
//! changed, so the library is never used. That detects the rewrite but cannot
//! undo it: the library's initializers have already run by then. The same
//! check also fails a load whose file was replaced by a rename meanwhile (the
//! unlinked original's change time moves), even though the checked bytes were
//! the ones loaded: a library replaced mid-load means something is writing to
//! the folder, and the load fails closed. Windows residual: a directory
//! junction on the path could be re-pointed during the load; AFT's own runtime
//! folder contains none.
//!
//! An operator's library (named by `ORT_DYLIB_PATH` at spawn, outside the
//! managed folder) is accepted whatever its bytes are, so there is no check to
//! race; it is loaded by its path, as before, which keeps any sibling
//! libraries it resolves relative to its own location loadable.
//!
//! No private copy of the library is made, so there is nothing left behind in
//! AFT's storage to clean up.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once, OnceLock};

use sha2::{Digest, Sha256};

use crate::semantic_index::ONNX_RUNTIME_MISSING_PREFIX;

/// Environment variable naming the ONNX Runtime library to load.
pub const ORT_DYLIB_PATH_ENV: &str = "ORT_DYLIB_PATH";

/// One ONNX Runtime library build AFT's installer downloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedOrtLibrary {
    /// ONNX Runtime release version.
    pub version: &'static str,
    /// Microsoft's release asset the library comes from.
    pub asset: &'static str,
    /// Path of the library inside that asset.
    pub library: &'static str,
    /// Lowercase hex sha256 of the library file's bytes (symlinks followed).
    pub sha256: &'static str,
}

/// The ONNX Runtime libraries AFT accepts without an operator override.
///
/// Each entry is the main library inside the release asset that
/// `packages/aft-bridge/src/onnx-runtime.ts` downloads (`ORT_PLATFORM_MAP`,
/// `ORT_VERSION`), fetched from
/// `https://github.com/microsoft/onnxruntime/releases/download/v1.24.4/<asset>`
/// and hashed after extraction. On Linux `lib/libonnxruntime.so` is a symlink
/// chain ending at `libonnxruntime.so.1.24.4`; the hash is of the file it
/// resolves to, which is what the installer copies and what a load maps.
/// The installer checks the same hashes (`librarySha256`) before it publishes
/// a download. Bumping `ORT_VERSION` there means adding the new builds here.
///
/// Every entry is accepted on every platform: a library built for another
/// platform cannot load anyway, and all of them are genuine Microsoft builds.
pub const PINNED_ORT_LIBRARIES: &[PinnedOrtLibrary] = &[
    PinnedOrtLibrary {
        version: "1.24.4",
        asset: "onnxruntime-osx-arm64-1.24.4.tgz",
        library: "onnxruntime-osx-arm64-1.24.4/lib/libonnxruntime.dylib",
        sha256: "872533f130f1839a5bc01788ddb4f75c83a189763441ba1178788ed965449289",
    },
    PinnedOrtLibrary {
        version: "1.24.4",
        asset: "onnxruntime-linux-x64-1.24.4.tgz",
        library: "onnxruntime-linux-x64-1.24.4/lib/libonnxruntime.so",
        sha256: "d132535d051344ff5c64c9c200004150559049a81ed330eb4422c1962fb6b7e4",
    },
    PinnedOrtLibrary {
        version: "1.24.4",
        asset: "onnxruntime-linux-aarch64-1.24.4.tgz",
        library: "onnxruntime-linux-aarch64-1.24.4/lib/libonnxruntime.so",
        sha256: "52b2a0e75e79468404284fec38ad5ee1a7a996232274f5e1b84f3e793fb07554",
    },
    PinnedOrtLibrary {
        version: "1.24.4",
        asset: "onnxruntime-win-x64-1.24.4.zip",
        library: "onnxruntime-win-x64-1.24.4/lib/onnxruntime.dll",
        sha256: "b95efb2113b603bbbf3f191061c5516a871ed546893c820e4f3b7b6c358dbf2a",
    },
    PinnedOrtLibrary {
        version: "1.24.4",
        asset: "onnxruntime-win-arm64-1.24.4.zip",
        library: "onnxruntime-win-arm64-1.24.4/lib/onnxruntime.dll",
        sha256: "724d81ac50b11bfaa01ab3ce01b99fb6734a4b762259a8be4395ca662ce99fe6",
    },
];

/// Why a library load was allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrtLoadAuthorization {
    /// The file's sha256 is a pinned build.
    Pinned { path: PathBuf, sha256: String },
    /// Not a pinned build, but named by `ORT_DYLIB_PATH` at spawn and outside
    /// AFT's managed runtime folder.
    OperatorUnpinned { path: PathBuf, sha256: String },
    /// No path configured; the loader finds the library by bare name.
    /// Only allowed outside the supervised module.
    LoaderSearchUnpinned,
}

/// The value `ORT_DYLIB_PATH` had when this process was spawned.
///
/// The first capture wins for the life of the process, so a value AFT exports
/// later (after resolving its own managed download) can never be mistaken for
/// the operator's. Empty values count as unset, matching how the resolver
/// treats them.
pub(crate) struct SpawnEnvValue(OnceLock<Option<OsString>>);

impl SpawnEnvValue {
    pub(crate) const fn new() -> Self {
        Self(OnceLock::new())
    }

    pub(crate) fn capture_with(&self, lookup: impl FnOnce() -> Option<OsString>) -> Option<&OsStr> {
        self.0
            .get_or_init(|| lookup().filter(|value| !value.is_empty()))
            .as_deref()
    }
}

static SPAWN_ORT_DYLIB_PATH: SpawnEnvValue = SpawnEnvValue::new();

/// Record `ORT_DYLIB_PATH` as this process received it. Call it before
/// anything in the process may set the variable: `main` does so first thing,
/// and the managed-runtime resolver does so again right before it exports its
/// own path (the call is idempotent, the first one wins).
pub fn capture_spawn_ort_dylib_path() -> Option<&'static OsStr> {
    SPAWN_ORT_DYLIB_PATH.capture_with(|| std::env::var_os(ORT_DYLIB_PATH_ENV))
}

static PINNED_RUNTIME_REQUIRED: AtomicBool = AtomicBool::new(false);

/// Refuse bare-name loads from the loader search path for the rest of the
/// process. Called when AFT runs as the supervised module (`--subc`), which
/// holds a launch secret; a standalone process keeps the bare-name fallback.
pub fn require_pinned_onnx_runtime() {
    PINNED_RUNTIME_REQUIRED.store(true, Ordering::SeqCst);
}

fn pinned_runtime_required() -> bool {
    PINNED_RUNTIME_REQUIRED.load(Ordering::SeqCst)
}

static MANAGED_STORAGE_DIRS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Remember a storage directory whose `onnxruntime/` folder holds AFT's own
/// managed runtime, so a spawn-time `ORT_DYLIB_PATH` pointing into it is held
/// to the pin instead of being treated as an operator choice. The bridge
/// passes the managed path it resolved at spawn; that is AFT's own download,
/// not the operator's decision.
pub fn register_managed_storage_dir(storage_dir: &Path) {
    let mut dirs = MANAGED_STORAGE_DIRS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !dirs.iter().any(|known| known == storage_dir) {
        dirs.push(storage_dir.to_path_buf());
    }
}

/// `<storage>/onnxruntime` for every storage directory this process knows,
/// plus the default one (the bridge's storage when no override is set).
fn managed_runtime_roots() -> Vec<PathBuf> {
    let mut storage_dirs = MANAGED_STORAGE_DIRS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let default = crate::storage_path::storage_dir(None);
    if !storage_dirs.contains(&default) {
        storage_dirs.push(default);
    }
    storage_dirs
        .into_iter()
        .map(|dir| dir.join("onnxruntime"))
        .collect()
}

/// Inputs to one authorization decision. Production builds it from process
/// state in [`authorize_onnx_runtime_load`]; tests build it directly.
pub(crate) struct OrtPinPolicy<'a> {
    /// `ORT_DYLIB_PATH` as captured at spawn.
    pub spawn_value: Option<&'a OsStr>,
    /// Refuse bare-name loads (supervised module).
    pub require_pinned: bool,
    /// Folders holding AFT's managed runtime (`<storage>/onnxruntime`).
    pub managed_roots: &'a [PathBuf],
    /// Accepted library hashes, lowercase hex.
    pub pinned_sha256: &'a [&'a str],
}

/// A library the pin admitted, with the open file it was hashed through.
///
/// A pinned library must be loaded through [`AuthorizedOrtLibrary::load_path`]
/// while this value is alive: the path names the open file, so the loader maps
/// the bytes that were hashed even if the file's own path has been replaced
/// since. Dropping the value closes the file; do that only after the load.
#[derive(Debug)]
pub struct AuthorizedOrtLibrary {
    authorization: OrtLoadAuthorization,
    held: Option<HeldLibrary>,
}

/// The open file a pinned library was hashed through, and what it looked like
/// while being hashed.
#[derive(Debug)]
struct HeldLibrary {
    file: File,
    path: PathBuf,
    sha256: String,
    #[cfg(unix)]
    identity: FileIdentity,
}

/// The metadata that changes when a file is replaced or written to. The change
/// time (`ctime`) is set by the kernel on every write and cannot be set back
/// by a user process, unlike the modification time.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
    size: u64,
    ctime: i64,
    ctime_nsec: i64,
    mtime: i64,
    mtime_nsec: i64,
}

#[cfg(unix)]
impl FileIdentity {
    fn of(file: &File) -> std::io::Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        Ok(Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            ctime: meta.ctime(),
            ctime_nsec: meta.ctime_nsec(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        })
    }
}

impl AuthorizedOrtLibrary {
    /// Why the load is allowed.
    pub fn authorization(&self) -> &OrtLoadAuthorization {
        &self.authorization
    }

    /// The path to give the loader (`dlopen`, `LoadLibrary`, `ort::init_from`),
    /// or `None` for a load by bare name from the loader search path.
    ///
    /// For a pinned library this names the open file the hash was computed
    /// through (`/dev/fd/<n>` on macOS, `/proc/self/fd/<n>` on Linux), never
    /// the path it was found at. On Windows it is the original path: the file
    /// is held open without write or delete sharing, so that path cannot name
    /// different bytes until this value is dropped.
    pub fn load_path(&self) -> Option<PathBuf> {
        // Only a pinned library is held, and it is always held.
        if let Some(held) = &self.held {
            return Some(held_file_load_path(&held.file, &held.path));
        }
        match &self.authorization {
            OrtLoadAuthorization::LoaderSearchUnpinned => None,
            OrtLoadAuthorization::OperatorUnpinned { path, .. }
            | OrtLoadAuthorization::Pinned { path, .. } => Some(path.clone()),
        }
    }

    /// Check, after the load, that the held file still is what was hashed:
    /// same device, inode, size and change time, and the same sha256 read
    /// again through the open file. A same-user process can rewrite the file in
    /// place between the hash and the loader mapping it; this reports that
    /// rewrite so the library is never used, though its initializers have
    /// already run. Always `Ok` for loads that were not pinned.
    pub fn confirm_unchanged(&self) -> Result<(), String> {
        let Some(held) = self.held.as_ref() else {
            return Ok(());
        };
        let path = &held.path;
        let changed = |detail: String| {
            // Worded like the hash-mismatch refusal so it does not match
            // `semantic_index::is_onnx_runtime_unavailable`: a missing runtime
            // is answered with `doctor --fix`, which cannot help here.
            format!(
                "onnx runtime library changed while it was being checked at {}: {detail}. \
                 It is not used, so semantic search is unavailable. Remove the file and \
                 reinstall with `npx @cortexkit/aft doctor --clear`.",
                path.display()
            )
        };
        #[cfg(unix)]
        {
            let now = FileIdentity::of(&held.file)
                .map_err(|error| changed(format!("cannot read its metadata: {error}")))?;
            if now != held.identity {
                return Err(changed("it was replaced or written to".to_string()));
            }
        }
        let mut file = &held.file;
        let sha256 = file
            .seek(SeekFrom::Start(0))
            .and_then(|_| sha256_reader(&mut file))
            .map_err(|error| changed(format!("cannot read it again: {error}")))?;
        if sha256 != held.sha256 {
            return Err(changed(format!("its sha256 is now {sha256}")));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn held_file_load_path(file: &File, _path: &Path) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/dev/fd/{}", file.as_raw_fd()))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn held_file_load_path(file: &File, _path: &Path) -> PathBuf {
    use std::os::fd::AsRawFd;
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(windows)]
fn held_file_load_path(_file: &File, path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Open a library to hash it. On Windows the file is shared for reading only,
/// so nothing can write, replace, rename or delete it (or rename a folder
/// above it) while it stays open.
fn open_library(path: &Path) -> std::io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(path)
    }
    #[cfg(not(windows))]
    {
        File::open(path)
    }
}

/// Decide whether the library at `candidate` (or the bare-name load when
/// `None`) may be loaded. Reads and hashes the file; never loads it.
#[cfg(test)]
pub(crate) fn authorize_with(
    policy: &OrtPinPolicy<'_>,
    candidate: Option<&OsStr>,
) -> Result<OrtLoadAuthorization, String> {
    open_authorized_with(policy, candidate).map(|library| library.authorization)
}

/// [`authorize_with`], keeping a pinned library's file open so it can be
/// loaded through the same open file it was hashed through.
pub(crate) fn open_authorized_with(
    policy: &OrtPinPolicy<'_>,
    candidate: Option<&OsStr>,
) -> Result<AuthorizedOrtLibrary, String> {
    let Some(candidate) = candidate.filter(|value| !value.is_empty()) else {
        if policy.require_pinned {
            // Keeps the missing-runtime prefix: the semantic build treats that
            // prefix as "wait for the managed download and retry", and the
            // download is exactly what makes a pinned runtime available here.
            return Err(format!(
                "{ONNX_RUNTIME_MISSING_PREFIX} No managed ONNX Runtime was found and \
                 ORT_DYLIB_PATH was not set when this module was spawned. The supervised \
                 module loads only a pinned runtime or one named by ORT_DYLIB_PATH at \
                 spawn, never a bare library name from the loader search path. Run \
                 `npx @cortexkit/aft doctor --fix` to install it."
            ));
        }
        return Ok(AuthorizedOrtLibrary {
            authorization: OrtLoadAuthorization::LoaderSearchUnpinned,
            held: None,
        });
    };

    let path = Path::new(candidate);
    let unreadable = |error: std::io::Error| {
        format!(
            "{ONNX_RUNTIME_MISSING_PREFIX} cannot read '{}' to check its sha256 before \
             loading it: {error}. Run `npx @cortexkit/aft doctor --fix` to install it.",
            path.display()
        )
    };
    let mut file = open_library(path).map_err(unreadable)?;
    #[cfg(unix)]
    let identity = FileIdentity::of(&file).map_err(unreadable)?;
    let sha256 = sha256_reader(&mut file).map_err(unreadable)?;
    // Rewind so a loader that reads through the descriptor (opening
    // `/dev/fd/<n>` on macOS shares this file's offset) starts at the top.
    file.seek(SeekFrom::Start(0)).map_err(unreadable)?;

    if policy
        .pinned_sha256
        .iter()
        .any(|pinned| pinned.eq_ignore_ascii_case(&sha256))
    {
        return Ok(AuthorizedOrtLibrary {
            authorization: OrtLoadAuthorization::Pinned {
                path: path.to_path_buf(),
                sha256: sha256.clone(),
            },
            // The metadata is taken before hashing, so a write while the hash
            // was being read also shows up in `confirm_unchanged`.
            held: Some(HeldLibrary {
                file,
                path: path.to_path_buf(),
                sha256,
                #[cfg(unix)]
                identity,
            }),
        });
    }

    let named_at_spawn = policy.spawn_value == Some(candidate);
    if named_at_spawn && !is_inside_any(path, policy.managed_roots) {
        return Ok(AuthorizedOrtLibrary {
            authorization: OrtLoadAuthorization::OperatorUnpinned {
                path: path.to_path_buf(),
                sha256,
            },
            held: None,
        });
    }

    // Worded so `semantic_index::is_onnx_runtime_unavailable` does not match it
    // (that check looks for "dlopen", "not found" and similar): a match would
    // report a missing runtime and suggest `doctor --fix`, which cannot help
    // while a tampered file still sits in the folder.
    Err(format!(
        "onnx runtime library hash mismatch at {}: sha256 {sha256} is not a pinned ONNX \
         Runtime build, so semantic search is unavailable. Remove the file and reinstall \
         with `npx @cortexkit/aft doctor --clear`, or name a runtime you trust with \
         ORT_DYLIB_PATH in the environment AFT is started with.",
        path.display()
    ))
}

/// True when `path` lies inside one of `roots`, comparing both the spelling
/// given and the symlink-resolved forms, so neither a symlink into the managed
/// folder nor a symlinked storage directory escapes the check.
fn is_inside_any(path: &Path, roots: &[PathBuf]) -> bool {
    let resolved_path = std::fs::canonicalize(path).ok();
    roots.iter().any(|root| {
        if path.starts_with(root) {
            return true;
        }
        let Some(resolved_path) = resolved_path.as_deref() else {
            return false;
        };
        resolved_path.starts_with(root)
            || std::fs::canonicalize(root)
                .map(|resolved_root| resolved_path.starts_with(resolved_root))
                .unwrap_or(false)
    })
}

/// Lowercase hex sha256 of a file's bytes, following symlinks.
#[cfg(test)]
pub(crate) fn sha256_file(path: &Path) -> std::io::Result<String> {
    sha256_reader(&mut File::open(path)?)
}

/// Lowercase hex sha256 of everything `reader` yields from its current
/// position.
fn sha256_reader(reader: &mut impl Read) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn pinned_hashes() -> Vec<&'static str> {
    PINNED_ORT_LIBRARIES
        .iter()
        .map(|library| library.sha256)
        .collect()
}

/// Authorize the next ONNX Runtime load against this process's state and keep
/// the checked file open for it. Every path that loads the library goes
/// through this (via [`load_authorized_with`]) and loads only on `Ok`.
pub fn open_onnx_runtime_for_load(
    candidate: Option<&OsStr>,
) -> Result<AuthorizedOrtLibrary, String> {
    let roots = managed_runtime_roots();
    let pinned = pinned_hashes();
    let policy = OrtPinPolicy {
        spawn_value: capture_spawn_ort_dylib_path(),
        require_pinned: pinned_runtime_required(),
        managed_roots: &roots,
        pinned_sha256: &pinned,
    };
    let library = open_authorized_with(&policy, candidate)?;
    log_unpinned_once(library.authorization());
    Ok(library)
}

fn log_unpinned_once(authorization: &OrtLoadAuthorization) {
    static OPERATOR: Once = Once::new();
    static LOADER_SEARCH: Once = Once::new();
    match authorization {
        OrtLoadAuthorization::Pinned { .. } => {}
        OrtLoadAuthorization::OperatorUnpinned { path, sha256 } => OPERATOR.call_once(|| {
            crate::slog_warn!(
                "ONNX Runtime at {} (sha256 {sha256}) is not a pinned build; accepted unpinned \
                 because ORT_DYLIB_PATH named it when this process was spawned",
                path.display()
            );
        }),
        OrtLoadAuthorization::LoaderSearchUnpinned => LOADER_SEARCH.call_once(|| {
            crate::slog_warn!(
                "no ONNX Runtime path configured; loading it by name from the loader search \
                 path, unpinned"
            );
        }),
    }
}

/// Authorize `candidate`, then hand `load` the path to load it through (see
/// [`AuthorizedOrtLibrary::load_path`]; `None` means load by bare name), with
/// the checked file held open until `load` returns. A refused library never
/// reaches the loader, and a pinned library that changed during the load is
/// reported as an error instead of being used.
///
/// `after_check` runs between the hash and the load. Production passes a
/// no-op; tests use it to replace or rewrite the file in exactly the window
/// this function exists to close.
pub(crate) fn load_authorized_with<T>(
    open: impl FnOnce(Option<&OsStr>) -> Result<AuthorizedOrtLibrary, String>,
    candidate: Option<&OsStr>,
    after_check: impl FnOnce(),
    load: impl FnOnce(Option<&Path>) -> Result<T, String>,
) -> Result<T, String> {
    let library = open(candidate)?;
    after_check();
    let loaded = load(library.load_path().as_deref())?;
    library.confirm_unchanged()?;
    Ok(loaded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn write_library(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write fake library");
        path
    }

    fn hash_of(path: &Path) -> String {
        sha256_file(path).expect("hash fake library")
    }

    fn policy<'a>(
        spawn_value: Option<&'a OsStr>,
        require_pinned: bool,
        managed_roots: &'a [PathBuf],
        pinned_sha256: &'a [&'a str],
    ) -> OrtPinPolicy<'a> {
        OrtPinPolicy {
            spawn_value,
            require_pinned,
            managed_roots,
            pinned_sha256,
        }
    }

    /// Stands in for the loader: reads whatever `load_path` names, as `dlopen`
    /// would map it. `None` (a bare-name load) is not expected here.
    fn read_as_loader(load_path: Option<&Path>) -> Result<Vec<u8>, String> {
        let load_path = load_path.ok_or("a pinned library must be loaded by path")?;
        std::fs::read(load_path).map_err(|error| error.to_string())
    }

    #[test]
    fn a_library_swapped_between_the_check_and_the_load_is_never_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"genuine build");
        let replacement = write_library(dir.path(), "replacement.dylib", b"swapped build");
        let hash = hash_of(&path);
        let pinned = [hash.as_str()];
        let roots = [dir.path().to_path_buf()];
        let policy = policy(None, true, &roots, &pinned);

        let swapped = Cell::new(None);
        let mapped = RefCell::new(None);
        let result = load_authorized_with(
            |candidate| open_authorized_with(&policy, candidate),
            Some(path.as_os_str()),
            || swapped.set(Some(std::fs::rename(&replacement, &path).is_ok())),
            |load_path| {
                *mapped.borrow_mut() = Some(read_as_loader(load_path)?);
                Ok(())
            },
        );
        // What the loader got is the checked file, on every platform.
        assert_eq!(*mapped.borrow(), Some(b"genuine build".to_vec()));
        if cfg!(unix) {
            // The rename succeeded, and the path now names the replacement.
            assert_eq!(swapped.get(), Some(true), "the swap ran");
            assert_eq!(std::fs::read(&path).unwrap(), b"swapped build");
            // The load is still reported as failed: the rename unlinked the
            // checked file, which moves its change time, and the check after
            // the load treats that as a change. A library replaced mid-load
            // means something is writing to the folder.
            let error = result.expect_err("a swap during the load is reported");
            assert!(
                error.starts_with("onnx runtime library changed while it was being checked"),
                "{error}"
            );
        } else {
            // The held file cannot be replaced at all.
            assert_eq!(swapped.get(), Some(false), "the swap was refused");
            assert_eq!(result, Ok(()));
        }
    }

    #[test]
    fn a_library_rewritten_in_place_after_the_check_is_reported_and_not_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"genuine build");
        let hash = hash_of(&path);
        let pinned = [hash.as_str()];
        let roots = [dir.path().to_path_buf()];
        let policy = policy(None, true, &roots, &pinned);

        let rewritten = Cell::new(None);
        let result = load_authorized_with(
            |candidate| open_authorized_with(&policy, candidate),
            Some(path.as_os_str()),
            // Same length, so only the content (and the change time) differs.
            || rewritten.set(Some(std::fs::write(&path, b"hostile build").is_ok())),
            read_as_loader,
        );
        assert_eq!(
            rewritten.get(),
            Some(cfg!(unix)),
            "the rewrite ran as expected"
        );
        if cfg!(unix) {
            // No user-space lock stops a same-user writer on Unix; the rewrite
            // is caught after the load, and the load is reported as failed.
            let error = result.expect_err("a library rewritten after its check must not be used");
            assert!(
                error.starts_with(&format!(
                    "onnx runtime library changed while it was being checked at {}",
                    path.display()
                )),
                "{error}"
            );
            assert!(
                !crate::semantic_index::is_onnx_runtime_unavailable(&error),
                "a rewritten library is not a missing runtime: {error}"
            );
        } else {
            // Windows refuses the write while the file is held.
            assert_eq!(result, Ok(b"genuine build".to_vec()));
        }
    }

    #[test]
    fn a_pinned_library_is_loaded_through_the_checked_file_not_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"genuine build");
        let hash = hash_of(&path);
        let pinned = [hash.as_str()];
        let roots = [dir.path().to_path_buf()];
        let library =
            open_authorized_with(&policy(None, true, &roots, &pinned), Some(path.as_os_str()))
                .expect("a pinned library is admitted");
        let load_path = library
            .load_path()
            .expect("a pinned library has a load path");
        #[cfg(target_os = "macos")]
        assert!(load_path.starts_with("/dev/fd"), "{}", load_path.display());
        #[cfg(all(unix, not(target_os = "macos")))]
        assert!(
            load_path.starts_with("/proc/self/fd"),
            "{}",
            load_path.display()
        );
        #[cfg(windows)]
        assert_eq!(load_path, path);
        assert_eq!(library.confirm_unchanged(), Ok(()));
    }

    #[test]
    fn an_operator_library_is_loaded_by_its_own_path() {
        let operator_dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let path = write_library(operator_dir.path(), "libonnxruntime.dylib", b"brew build");
        let roots = [storage.path().join("onnxruntime")];
        let pinned: [&str; 0] = [];
        let library = open_authorized_with(
            &policy(Some(path.as_os_str()), true, &roots, &pinned),
            Some(path.as_os_str()),
        )
        .expect("the operator's spawn-time choice is admitted");
        assert_eq!(library.load_path(), Some(path.clone()));
        // Its bytes are accepted whatever they are, so there is nothing to
        // re-check after the load.
        std::fs::write(&path, b"other build").unwrap();
        assert_eq!(library.confirm_unchanged(), Ok(()));
    }

    #[test]
    fn a_bare_name_load_has_no_path() {
        let roots: [PathBuf; 0] = [];
        let pinned: [&str; 0] = [];
        let library = open_authorized_with(&policy(None, false, &roots, &pinned), None)
            .expect("standalone bare-name loads are allowed");
        assert_eq!(library.load_path(), None);
    }

    /// A real ONNX Runtime the pin accepts, if this machine has one:
    /// `AFT_TEST_ORT_LIBRARY_DIR`, else the runtime AFT manages for this user.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn real_pinned_runtime() -> Option<PathBuf> {
        #[cfg(target_os = "macos")]
        const NAME: &str = "libonnxruntime.dylib";
        #[cfg(target_os = "linux")]
        const NAME: &str = "libonnxruntime.so";
        let dir = std::env::var_os("AFT_TEST_ORT_LIBRARY_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                let home = PathBuf::from(std::env::var_os("HOME")?);
                Some(home.join(".local/share/cortexkit/aft/onnxruntime/1.24.4"))
            })?;
        let library = dir.join(NAME);
        let hash = sha256_file(&library).ok()?;
        pinned_hashes().contains(&hash.as_str()).then_some(library)
    }

    /// The first two entries of ONNX Runtime's `OrtApiBase`.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[repr(C)]
    struct OrtApiBase {
        _get_api: unsafe extern "C" fn(u32) -> *const std::ffi::c_void,
        get_version_string: unsafe extern "C" fn() -> *const std::ffi::c_char,
    }

    /// The real loader, the real library: a file swapped in after the check is
    /// not what `dlopen` maps. Loading by path here would fail (the
    /// replacement is not a library at all); loading through the descriptor
    /// maps the checked ONNX Runtime and reports its version.
    #[test]
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn real_dlopen_maps_the_checked_runtime_after_a_swap() {
        let Some(real) = real_pinned_runtime() else {
            eprintln!(
                "skipping: no pinned ONNX Runtime on this machine (AFT_TEST_ORT_LIBRARY_DIR)"
            );
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(real.file_name().unwrap());
        std::fs::copy(&real, &path).unwrap();
        let replacement = write_library(dir.path(), "replacement", b"not a library");
        let pinned = pinned_hashes();
        let roots = [dir.path().to_path_buf()];
        let policy = policy(None, true, &roots, &pinned);

        let mapped_version = RefCell::new(None);
        let result = load_authorized_with(
            |candidate| open_authorized_with(&policy, candidate),
            Some(path.as_os_str()),
            || std::fs::rename(&replacement, &path).unwrap(),
            |load_path| {
                let load_path = load_path.ok_or("a pinned library is loaded by path")?;
                let c_path = std::ffi::CString::new(load_path.to_str().unwrap()).unwrap();
                let symbol_name = std::ffi::CString::new("OrtGetApiBase").unwrap();
                // SAFETY: loads a pinned Microsoft build and calls its
                // documented entry point; the handle is closed before return.
                unsafe {
                    let handle = libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW);
                    if handle.is_null() {
                        let error = std::ffi::CStr::from_ptr(libc::dlerror());
                        return Err(error.to_string_lossy().into_owned());
                    }
                    let symbol = libc::dlsym(handle, symbol_name.as_ptr());
                    assert!(!symbol.is_null(), "OrtGetApiBase is exported");
                    let get_api_base: unsafe extern "C" fn() -> *const OrtApiBase =
                        std::mem::transmute(symbol);
                    let version =
                        std::ffi::CStr::from_ptr(((*get_api_base()).get_version_string)())
                            .to_string_lossy()
                            .into_owned();
                    libc::dlclose(handle);
                    *mapped_version.borrow_mut() = Some(version);
                    Ok(())
                }
            },
        );
        assert_eq!(*mapped_version.borrow(), Some("1.24.4".to_string()));
        // The version came from the checked library, but the rename itself
        // is still reported as a failed load, as in the swap test above.
        assert!(result.is_err(), "{result:?}");
    }

    #[test]
    fn sha256_file_matches_a_known_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "lib", b"abc");
        assert_eq!(
            hash_of(&path),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn pinned_library_is_authorized_and_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"genuine build");
        let hash = hash_of(&path);
        let pinned = [hash.as_str()];
        let roots = [dir.path().to_path_buf()];
        // Inside the managed folder and not named by ORT_DYLIB_PATH at spawn,
        // so only a matching pinned hash can admit it.
        let policy = policy(None, true, &roots, &pinned);

        let loads = Cell::new(0);
        let result = load_authorized_with(
            |candidate| open_authorized_with(&policy, candidate),
            Some(path.as_os_str()),
            || {},
            |loaded| {
                // The loader gets a path naming the checked file (not
                // necessarily its original path; see `load_path`).
                assert_eq!(read_as_loader(loaded), Ok(b"genuine build".to_vec()));
                loads.set(loads.get() + 1);
                Ok(())
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(loads.get(), 1);
        assert_eq!(
            authorize_with(&policy, Some(path.as_os_str())),
            Ok(OrtLoadAuthorization::Pinned {
                path: path.clone(),
                sha256: hash
            })
        );
    }

    #[test]
    fn mismatched_library_is_refused_and_never_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"tampered build");
        let pinned = ["872533f130f1839a5bc01788ddb4f75c83a189763441ba1178788ed965449289"];
        let roots = [dir.path().to_path_buf()];
        let policy = policy(None, false, &roots, &pinned);

        let loads = Cell::new(0);
        let error = load_authorized_with(
            |candidate| open_authorized_with(&policy, candidate),
            Some(path.as_os_str()),
            || {},
            |_| {
                loads.set(loads.get() + 1);
                Ok(())
            },
        )
        .expect_err("a library off the pinned list must be refused");
        assert_eq!(
            loads.get(),
            0,
            "a refused library must never reach the loader"
        );
        assert!(
            error.starts_with(&format!(
                "onnx runtime library hash mismatch at {}",
                path.display()
            )),
            "{error}"
        );
        assert!(
            !crate::semantic_index::is_onnx_runtime_unavailable(&error),
            "a tampered library is not a missing runtime: {error}"
        );
    }

    #[test]
    fn spawn_time_operator_path_outside_the_managed_folder_is_accepted_unpinned() {
        let operator_dir = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let path = write_library(operator_dir.path(), "libonnxruntime.dylib", b"brew build");
        let roots = [storage.path().join("onnxruntime")];
        let pinned = ["872533f130f1839a5bc01788ddb4f75c83a189763441ba1178788ed965449289"];
        for require_pinned in [false, true] {
            let policy = policy(Some(path.as_os_str()), require_pinned, &roots, &pinned);
            assert_eq!(
                authorize_with(&policy, Some(path.as_os_str())),
                Ok(OrtLoadAuthorization::OperatorUnpinned {
                    path: path.clone(),
                    sha256: hash_of(&path)
                }),
                "require_pinned={require_pinned}"
            );
        }
    }

    #[test]
    fn spawn_time_path_inside_the_managed_folder_must_still_match_the_pin() {
        let storage = tempfile::tempdir().unwrap();
        let version_dir = storage.path().join("onnxruntime").join("1.24.4");
        std::fs::create_dir_all(&version_dir).unwrap();
        let path = write_library(&version_dir, "libonnxruntime.dylib", b"swapped build");
        let roots = [storage.path().join("onnxruntime")];
        let pinned = ["872533f130f1839a5bc01788ddb4f75c83a189763441ba1178788ed965449289"];
        // The bridge passes its managed path at spawn; that is AFT's own
        // download, so a bad hash is refused whether or not the process is
        // the supervised module (require_pinned true or false).
        for require_pinned in [false, true] {
            let policy = policy(Some(path.as_os_str()), require_pinned, &roots, &pinned);
            let error = authorize_with(&policy, Some(path.as_os_str()))
                .expect_err("a managed library with a bad hash must be refused");
            assert!(
                error.starts_with("onnx runtime library hash mismatch at"),
                "require_pinned={require_pinned}: {error}"
            );
        }
    }

    #[test]
    fn managed_folder_reached_through_a_symlink_is_still_managed() {
        #[cfg(unix)]
        {
            let storage = tempfile::tempdir().unwrap();
            let outside = tempfile::tempdir().unwrap();
            let version_dir = storage.path().join("onnxruntime").join("1.24.4");
            std::fs::create_dir_all(&version_dir).unwrap();
            let real = write_library(&version_dir, "libonnxruntime.dylib", b"swapped build");
            let link = outside.path().join("libonnxruntime.dylib");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            let roots = [storage.path().join("onnxruntime")];
            let pinned: [&str; 0] = [];
            let policy = policy(Some(link.as_os_str()), false, &roots, &pinned);
            assert!(authorize_with(&policy, Some(link.as_os_str())).is_err());
        }
    }

    #[test]
    fn a_value_aft_exported_itself_is_not_an_operator_override() {
        // The process starts with no ORT_DYLIB_PATH; AFT later resolves its
        // managed download and exports it. The spawn capture must still read
        // "unset", so the exported path is held to the pin.
        let env: RefCell<Option<OsString>> = RefCell::new(None);
        let spawn = SpawnEnvValue::new();
        assert_eq!(spawn.capture_with(|| env.borrow().clone()), None);

        let dir = tempfile::tempdir().unwrap();
        let path = write_library(dir.path(), "libonnxruntime.dylib", b"swapped build");
        *env.borrow_mut() = Some(path.as_os_str().to_os_string());

        let spawn_value = spawn.capture_with(|| env.borrow().clone());
        assert_eq!(spawn_value, None, "the first capture must win");
        let roots: [PathBuf; 0] = [];
        let pinned = ["872533f130f1839a5bc01788ddb4f75c83a189763441ba1178788ed965449289"];
        let policy = policy(spawn_value, false, &roots, &pinned);
        let error = authorize_with(&policy, Some(path.as_os_str()))
            .expect_err("AFT's own export is not the operator's choice");
        assert!(error.starts_with("onnx runtime library hash mismatch at"));
    }

    #[test]
    fn spawn_capture_keeps_a_value_present_at_spawn() {
        let spawn = SpawnEnvValue::new();
        let value = OsString::from("/opt/onnxruntime/libonnxruntime.so");
        assert_eq!(
            spawn.capture_with(|| Some(value.clone())),
            Some(value.as_os_str())
        );
        assert_eq!(
            spawn.capture_with(|| None),
            Some(value.as_os_str()),
            "a later unset must not erase the spawn value"
        );
        let empty = SpawnEnvValue::new();
        assert_eq!(empty.capture_with(|| Some(OsString::new())), None);
    }

    #[test]
    fn bare_name_load_is_refused_under_the_supervised_module() {
        let roots: [PathBuf; 0] = [];
        let pinned: [&str; 0] = [];
        let error = authorize_with(&policy(None, true, &roots, &pinned), None)
            .expect_err("the supervised module must not load by bare name");
        assert!(
            error.starts_with(ONNX_RUNTIME_MISSING_PREFIX),
            "the wait-for-download flow keys on this prefix: {error}"
        );
    }

    #[test]
    fn bare_name_load_is_allowed_standalone() {
        let roots: [PathBuf; 0] = [];
        let pinned: [&str; 0] = [];
        assert_eq!(
            authorize_with(&policy(None, false, &roots, &pinned), None),
            Ok(OrtLoadAuthorization::LoaderSearchUnpinned)
        );
        assert_eq!(
            authorize_with(&policy(None, false, &roots, &pinned), Some(OsStr::new(""))),
            Ok(OrtLoadAuthorization::LoaderSearchUnpinned),
            "an empty ORT_DYLIB_PATH is unset"
        );
    }

    #[test]
    fn unreadable_library_reports_a_missing_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent").join("libonnxruntime.dylib");
        let roots: [PathBuf; 0] = [];
        let pinned: [&str; 0] = [];
        let error = authorize_with(
            &policy(Some(path.as_os_str()), false, &roots, &pinned),
            Some(path.as_os_str()),
        )
        .expect_err("a file that cannot be hashed cannot be loaded");
        assert!(error.starts_with(ONNX_RUNTIME_MISSING_PREFIX), "{error}");
        assert!(error.contains("doctor --fix"), "{error}");
    }

    #[test]
    fn installer_pins_the_same_library_hashes() {
        // The bridge installer (onnx-runtime.ts, `librarySha256`) checks a
        // download against its own copy of PINNED_ORT_LIBRARIES before
        // publishing it. If the two lists drift apart, the installer publishes
        // builds AFT then refuses, or refuses builds AFT would load.
        let installer = Path::new(crate::test_support::manifest_dir())
            .join("../../packages/aft-bridge/src/onnx-runtime.ts");
        let Ok(source) = std::fs::read_to_string(&installer) else {
            eprintln!("skipping: {} not present", installer.display());
            return;
        };
        let installer_pins: Vec<&str> = source
            .lines()
            .filter_map(|line| line.trim().strip_prefix("librarySha256: \""))
            .filter_map(|rest| rest.split('"').next())
            .collect();
        let mut rust_pins: Vec<&str> = PINNED_ORT_LIBRARIES
            .iter()
            .map(|library| library.sha256)
            .collect();
        let mut installer_sorted = installer_pins.clone();
        rust_pins.sort_unstable();
        installer_sorted.sort_unstable();
        assert_eq!(installer_sorted, rust_pins);
    }

    #[test]
    fn pinned_list_covers_every_platform_the_installer_downloads() {
        for asset in [
            "onnxruntime-osx-arm64-1.24.4.tgz",
            "onnxruntime-linux-x64-1.24.4.tgz",
            "onnxruntime-linux-aarch64-1.24.4.tgz",
            "onnxruntime-win-x64-1.24.4.zip",
            "onnxruntime-win-arm64-1.24.4.zip",
        ] {
            let entry = PINNED_ORT_LIBRARIES
                .iter()
                .find(|library| library.asset == asset)
                .unwrap_or_else(|| panic!("no pin for {asset}"));
            assert_eq!(entry.sha256.len(), 64, "{asset}");
            assert!(
                entry
                    .sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "{asset}: pins are lowercase hex"
            );
        }
    }
}
