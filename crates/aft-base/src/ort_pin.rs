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
//! because library validation would refuse a library that does not carry AFT's
//! Team ID. With library validation off, the loader no longer checks who
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

/// Opening words of every missing-runtime message this crate produces.
///
/// `is_onnx_runtime_unavailable` treats this prefix as proof on its own, and
/// readers outside the daemon (the OpenCode sidebar) key on the same prefix to
/// tell "the runtime is not installed" apart from an ordinary backend failure.
/// Changing it changes that contract.
pub const ONNX_RUNTIME_MISSING_PREFIX: &str = "ONNX Runtime not found.";

/// Environment variable naming the ONNX Runtime library to load.
pub const ORT_DYLIB_PATH_ENV: &str = "ORT_DYLIB_PATH";

/// One ONNX Runtime library build AFT accepts without an override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinnedOrtLibrary {
    /// ONNX Runtime release version.
    pub version: &'static str,
    /// Microsoft's package or legacy release asset the library comes from.
    pub asset: &'static str,
    /// Path of the library inside that asset.
    pub library: &'static str,
    /// Lowercase hex sha256 of the library file's bytes (symlinks followed).
    pub sha256: &'static str,
}

/// The ONNX Runtime libraries AFT accepts without an operator override.
///
/// The installer in `packages/aft-bridge/src/onnx-runtime.ts` downloads and
/// checks the main libraries under `runtimes/<rid>/native/` in Microsoft's
/// `Microsoft.ML.OnnxRuntime` NuGet package from `api.nuget.org`. The other
/// four NuGet libraries match the previous GitHub release builds byte for
/// byte. On macOS the NuGet library is signed by Microsoft; the pin for the
/// previous macOS build keeps existing managed caches usable.
/// Bumping the installer's `ORT_VERSION` means adding the new builds here.
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
        asset: "microsoft.ml.onnxruntime.1.24.4.nupkg",
        library: "runtimes/osx-arm64/native/libonnxruntime.dylib",
        sha256: "0db33e160224e480c17932799d1dfaea59853b8e722b57bf61932c87e03586f0",
    },
    PinnedOrtLibrary {
        version: "1.24.4",
        asset: "microsoft.ml.onnxruntime.1.24.4.nupkg",
        library: "runtimes/linux-x64/native/libonnxruntime.so",
        sha256: "d132535d051344ff5c64c9c200004150559049a81ed330eb4422c1962fb6b7e4",
    },
    PinnedOrtLibrary {
        version: "1.24.4",
        asset: "microsoft.ml.onnxruntime.1.24.4.nupkg",
        library: "runtimes/linux-arm64/native/libonnxruntime.so",
        sha256: "52b2a0e75e79468404284fec38ad5ee1a7a996232274f5e1b84f3e793fb07554",
    },
    PinnedOrtLibrary {
        version: "1.24.4",
        asset: "microsoft.ml.onnxruntime.1.24.4.nupkg",
        library: "runtimes/win-x64/native/onnxruntime.dll",
        sha256: "b95efb2113b603bbbf3f191061c5516a871ed546893c820e4f3b7b6c358dbf2a",
    },
    PinnedOrtLibrary {
        version: "1.24.4",
        asset: "microsoft.ml.onnxruntime.1.24.4.nupkg",
        library: "runtimes/win-arm64/native/onnxruntime.dll",
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
#[cfg(any(test, feature = "test-support"))]
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
#[cfg(any(test, feature = "test-support"))]
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
