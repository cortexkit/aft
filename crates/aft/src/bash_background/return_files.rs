//! Files a `runon` job copies back from the build server (`return_paths`).
//!
//! The runner streams each requested file in `return_chunk` records and then
//! reports every path in one `return_manifest`. [`ReturnReceiver`] stages the
//! bytes of each path in a private file inside the project and moves it to the
//! requested path only after the manifest verified its size and SHA-256
//! (checked by the contract crate's `ReturnAssembler`). A path that failed,
//! did not verify, or would resolve outside the project writes nothing and
//! leaves any file already there untouched.
//!
//! [`ReturnedPath`] is the per-path outcome saved with the task, and
//! [`render_report`] prints it with the command's result.

use serde::{Deserialize, Serialize};

/// Most paths one bash call may ask the runner to copy back.
pub(crate) const MAX_RETURN_PATHS: usize = 64;

/// What happened to one requested path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReturnedPath {
    /// The path as the caller wrote it, relative to the command's workdir.
    pub path: String,
    /// The same path relative to the checkout, with `/` separators, as the
    /// runner names the files it reports as changed. `None` when the workdir
    /// is not in a checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkout_path: Option<String>,
    pub outcome: ReturnOutcome,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub(crate) enum ReturnOutcome {
    /// The verified file now sits at the requested local path.
    Returned { bytes: u64 },
    /// Nothing was written for this path.
    NotReturned { reason: NotReturned },
}

/// Why a requested path was not written locally.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum NotReturned {
    /// The runner's manifest named this failure cause (`missing`,
    /// `not_regular`, `escape`, `over_cap`, `read_error`, `expired`, or a
    /// cause this version does not know).
    Runner { cause: String },
    /// The bytes the runner sent did not pass the stream checks, so every
    /// path of the job was discarded. `code` is the checker's error code.
    Unverified { code: String },
    /// The job ended without the runner's per-path report.
    NoReport,
    /// The local destination, or a symlink on the way to it, leads outside
    /// the project.
    OutsideProject,
    /// Staging or placing the verified file failed locally.
    LocalWrite { error: String },
    /// A symlink or something other than a regular file is at the local
    /// path. It is never followed or replaced.
    NotRegular,
}

// Only the Unix remote-run path prints the report.
#[cfg_attr(not(unix), allow(dead_code))]
impl NotReturned {
    fn describe(&self, cap_bytes: u64) -> String {
        match self {
            Self::Runner { cause } => match cause.as_str() {
                "missing" => "no such file on the server".into(),
                "not_regular" => "not a regular file on the server".into(),
                "escape" => "it resolves outside the checkout on the server".into(),
                "over_cap" => format!(
                    "it would take the total past the {} limit",
                    format_cap(cap_bytes)
                ),
                "read_error" => {
                    "the server could not read it, or it changed while being read".into()
                }
                "expired" => "the server no longer keeps this job's files".into(),
                other => format!("the server reported {}", other.escape_default()),
            },
            Self::Unverified { code } => {
                format!("the copy failed verification ({code}); nothing was written")
            }
            Self::NoReport => "the server sent no report for it".into(),
            Self::OutsideProject => "the local path resolves outside the project".into(),
            Self::LocalWrite { error } => format!("could not write it here: {error}"),
            Self::NotRegular => {
                "a symlink or other non-regular file is already at this path; AFT left it unchanged"
                    .into()
            }
        }
    }
}

// Only the Unix remote-run path prints the report.
#[cfg_attr(not(unix), allow(dead_code))]
fn format_cap(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if bytes % MIB == 0 {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{bytes}-byte")
    }
}

/// The block printed after a remote command's output: each requested path
/// once, returned with its size or not returned with the reason.
// Only the Unix remote-run path prints the report.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn render_report(paths: &[ReturnedPath]) -> String {
    let cap = crate::exec_remote::return_max_bytes();
    let mut block = String::from("Files requested back from the server (return_paths):");
    for entry in paths {
        let path = entry.path.escape_default();
        match &entry.outcome {
            ReturnOutcome::Returned { bytes } => block.push_str(&format!(
                "\n  {path}: returned ({bytes} byte{})",
                if *bytes == 1 { "" } else { "s" }
            )),
            ReturnOutcome::NotReturned { reason } => block.push_str(&format!(
                "\n  {path}: not returned ({})",
                reason.describe(cap)
            )),
        }
    }
    block
}

/// The note added when a command that asked for `return_paths` ran on this
/// machine instead of the build server.
// Only the Unix remote-run path prints the report.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const LOCAL_RUN_NOTE: &str =
    "return_paths: the command ran on this machine, so its files are already where it wrote them; nothing was copied back";

/// Check the shape of the requested paths before anything else happens.
/// Each is a file path relative to the command's workdir, without `..`,
/// listed once, and there are at most [`MAX_RETURN_PATHS`]. Whether each
/// resolves inside the project is checked separately against the filesystem.
pub(crate) fn check_requested_paths(paths: &[String]) -> Result<(), String> {
    use std::path::{Component, Path};
    if paths.len() > MAX_RETURN_PATHS {
        return Err(format!(
            "return_paths lists {} paths; at most {MAX_RETURN_PATHS} are allowed",
            paths.len()
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for path in paths {
        let shown = path.escape_default();
        if path.trim().is_empty() {
            return Err("return_paths has an empty path".into());
        }
        if path.contains('\0') {
            return Err(format!("return_paths path {shown} contains a NUL byte"));
        }
        let parsed = Path::new(path);
        if parsed.is_absolute() || path.starts_with(['/', '\\']) || parsed.has_root() {
            return Err(format!(
                "return_paths path {shown} must be relative to workdir"
            ));
        }
        if parsed
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::Prefix(_)))
        {
            return Err(format!("return_paths path {shown} must not use .."));
        }
        if path.ends_with(['/', '\\'])
            || !parsed
                .components()
                .any(|component| matches!(component, Component::Normal(_)))
        {
            return Err(format!(
                "return_paths path {shown} names a directory; list the files to copy back"
            ));
        }
        if !seen.insert(path.as_str()) {
            return Err(format!("return_paths lists {shown} twice"));
        }
    }
    Ok(())
}

/// Where a returned path's file goes, as resolved before any of it is written.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Destination {
    /// The resolved file to replace or create.
    pub target: std::path::PathBuf,
    /// Parent directories that do not exist yet, shallowest first. Each lies
    /// below an existing directory already checked to be inside the project.
    pub missing_dirs: Vec<std::path::PathBuf>,
}

/// The parent of a returned path's file: the deepest directory that exists,
/// resolved through any symlinks and refused unless it is inside `root`
/// (which must itself be resolved), the names of the directories below it
/// that do not exist yet, and the file's own name.
pub(crate) struct ResolvedParent {
    pub existing: std::path::PathBuf,
    pub missing: Vec<std::ffi::OsString>,
    pub file_name: std::ffi::OsString,
}

pub(crate) fn resolve_parent(
    root: &std::path::Path,
    requested: &std::path::Path,
) -> Result<ResolvedParent, NotReturned> {
    use std::fs;
    let file_name = requested
        .file_name()
        .ok_or_else(|| NotReturned::LocalWrite {
            error: "the path has no file name".into(),
        })?
        .to_owned();
    let parent = requested.parent().ok_or(NotReturned::OutsideProject)?;
    let mut existing = parent;
    let mut missing = Vec::new();
    while fs::symlink_metadata(existing).is_err() {
        missing.push(
            existing
                .file_name()
                .ok_or(NotReturned::OutsideProject)?
                .to_owned(),
        );
        existing = existing.parent().ok_or(NotReturned::OutsideProject)?;
    }
    let resolved = fs::canonicalize(existing).map_err(|_| NotReturned::OutsideProject)?;
    if !resolved.starts_with(root) {
        return Err(NotReturned::OutsideProject);
    }
    missing.reverse();
    Ok(ResolvedParent {
        existing: resolved,
        missing,
        file_name,
    })
}

/// The local file a returned path writes: `requested` resolved through any
/// symlinks, refused unless every existing part of it stays inside `root`
/// (which must itself be resolved). Nothing is created here. This is the
/// check made before the job is submitted; placing the file later refuses a
/// symlink at the path itself instead of following it.
pub(crate) fn resolve_destination(
    root: &std::path::Path,
    requested: &std::path::Path,
) -> Result<Destination, NotReturned> {
    use std::{fs, io};
    let local = |error: io::Error| NotReturned::LocalWrite {
        error: error.to_string(),
    };
    let parent = resolve_parent(root, requested)?;
    let mut directory = parent.existing;
    let mut missing_dirs = Vec::with_capacity(parent.missing.len());
    for component in parent.missing {
        directory.push(component);
        missing_dirs.push(directory.clone());
    }
    let target = directory.join(parent.file_name);
    if !missing_dirs.is_empty() {
        return Ok(Destination {
            target,
            missing_dirs,
        });
    }
    match fs::symlink_metadata(&target) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Destination {
            target,
            missing_dirs,
        }),
        Err(error) => Err(local(error)),
        Ok(metadata) => {
            // Like `write`, a symlink is followed to its target, which must
            // itself be inside the project.
            let resolved = if metadata.file_type().is_symlink() {
                fs::canonicalize(&target).map_err(|_| NotReturned::OutsideProject)?
            } else {
                target
            };
            if !resolved.starts_with(root) {
                return Err(NotReturned::OutsideProject);
            }
            if fs::metadata(&resolved).map_err(local)?.is_dir() {
                return Err(NotReturned::LocalWrite {
                    error: "a directory is at this path".into(),
                });
            }
            Ok(Destination {
                target: resolved,
                missing_dirs,
            })
        }
    }
}

/// Staging files and directories a task created for its returned files,
/// saved in the task record before each is created. After a restart they
/// are removed from these recorded places; nothing is found by scanning.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StagingRecord {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirs: Vec<std::path::PathBuf>,
}

impl StagingRecord {
    pub(crate) fn is_empty(&self) -> bool {
        self.files.is_empty() && self.dirs.is_empty()
    }
}

/// The name of a path's staging file in the task's private control folder:
/// specific to the task and the path's position in `return_paths`.
// Only the Unix remote-run path stages returned files.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn staging_name(task_id: &str, index: usize) -> String {
    format!(".aft-return-{task_id}-{index}.part")
}

/// Remove what an earlier AFT process staged for this task and did not
/// finish: each recorded staging file, then each recorded directory that is
/// still empty, deepest first. A recorded file whose name is not one of this
/// task's staging names is left alone.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) fn remove_recorded_staging(record: &StagingRecord, task_id: &str) {
    let prefix = format!(".aft-return-{task_id}-");
    for file in &record.files {
        let ours = file
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(&prefix) && name.ends_with(".part"));
        if ours {
            let _ = std::fs::remove_file(file);
        }
    }
    for dir in record.dirs.iter().rev() {
        remove_empty_dir(dir);
    }
}

/// Remove a directory AFT created, only when its path still resolves to
/// itself, so a path already redirected through a symlink is left alone. The
/// removal itself goes by path, so this narrows rather than closes a race;
/// at worst it removes an empty directory. A failed removal, including for a
/// directory that is not empty, is ignored.
#[cfg_attr(not(unix), allow(dead_code))]
fn remove_empty_dir(dir: &std::path::Path) {
    if std::fs::canonicalize(dir).is_ok_and(|resolved| resolved == dir) {
        // Fails, and keeps the directory, when anything is in it.
        let _ = std::fs::remove_dir(dir);
    }
}

#[cfg(unix)]
pub(crate) use receiver::ReturnReceiver;
#[cfg(all(unix, test))]
pub(crate) use receiver::{with_before_publish_for_test, with_copy_for_test, STOPPED_FOR_TEST};

#[cfg(unix)]
mod receiver {
    use super::{
        remove_empty_dir, resolve_parent, staging_name, NotReturned, ReturnOutcome, ReturnedPath,
        StagingRecord,
    };
    use crate::bash_background::persistence::PinnedDir;
    use crate::exec_remote::types::{
        ReturnAssembler, ReturnChunk, ReturnManifest, ReturnManifestOutcome,
    };
    use sha2::Digest;
    use std::ffi::{CString, OsStr, OsString};
    use std::fs;
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Component, Path, PathBuf};
    use std::sync::Arc;

    /// Saves staging files and directories in the task record before the
    /// receiver creates them, so a restart can remove them.
    pub(crate) type Recorder<'a> = dyn FnMut(&StagingRecord) -> io::Result<()> + 'a;

    /// One path's bytes as received so far, in a file in the task's private
    /// control folder, which no sandboxed command can write.
    struct Staged {
        name: OsString,
        file: fs::File,
    }

    /// Receives one job's returned files and places those that verify.
    pub(crate) struct ReturnReceiver {
        paths: Vec<String>,
        cap_bytes: u64,
        task_id: String,
        /// The command's working directory; requested paths are relative to it.
        cwd: PathBuf,
        /// The resolved project root. Every destination must resolve inside it.
        root: PathBuf,
        /// The resolved checkout the runner's workspace reports are relative
        /// to, when the workdir is in one.
        checkout_root: Option<PathBuf>,
        /// The task's private control folder, where received bytes are staged.
        private: Arc<PinnedDir>,
        /// `None` once a stream check failed; every path is then discarded.
        assembler: Option<ReturnAssembler>,
        stream_error: Option<String>,
        staged: Vec<Option<Staged>>,
        /// A local staging failure for one path. That path is not placed even
        /// if the manifest verifies it.
        write_errors: Vec<Option<NotReturned>>,
        /// Directories this receiver created in the project, shallowest
        /// first. Those still empty when the job's files are settled are
        /// removed again.
        created_dirs: Vec<PathBuf>,
        received_chunk: bool,
        /// The last accepted chunk's path index, offset and SHA-256. If saving
        /// the stream cursor fails after a chunk was taken, the stream
        /// delivers that chunk again; an exact repeat is not new data.
        last_chunk: Option<(u32, u64, [u8; 32])>,
    }

    impl ReturnReceiver {
        pub(crate) fn new(
            paths: Vec<String>,
            cap_bytes: u64,
            cwd: &Path,
            project_root: &Path,
            checkout_root: Option<&Path>,
            private: Arc<PinnedDir>,
            task_id: &str,
        ) -> io::Result<Self> {
            Ok(Self {
                staged: (0..paths.len()).map(|_| None).collect(),
                write_errors: vec![None; paths.len()],
                assembler: Some(ReturnAssembler::new(&paths, cap_bytes)),
                paths,
                cap_bytes,
                task_id: task_id.to_string(),
                cwd: cwd.to_path_buf(),
                root: fs::canonicalize(project_root)?,
                checkout_root: checkout_root.map(Path::to_path_buf),
                private,
                stream_error: None,
                created_dirs: Vec::new(),
                received_chunk: false,
                last_chunk: None,
            })
        }

        /// Whether any chunk has arrived since the receiver started or was
        /// last reset.
        pub(crate) fn received_chunk(&self) -> bool {
            self.received_chunk
        }

        /// Start over for a new job, such as a resubmission after the runner
        /// refused the first one, discarding anything already staged.
        pub(crate) fn reset(&mut self) {
            self.settle();
            self.assembler = Some(ReturnAssembler::new(&self.paths, self.cap_bytes));
            self.stream_error = None;
            self.write_errors = vec![None; self.paths.len()];
            self.received_chunk = false;
            self.last_chunk = None;
        }

        /// Check one chunk and append its bytes to the path's staging file.
        pub(crate) fn chunk(&mut self, chunk: &ReturnChunk, record: &mut Recorder<'_>) {
            self.received_chunk = true;
            let Some(assembler) = self.assembler.as_mut() else {
                return;
            };
            // Only the same offset with the same bytes is a repeat. Anything
            // else at an earlier offset goes to the assembler, which refuses
            // it and voids the job.
            let digest: [u8; 32] = sha2::Sha256::digest(chunk.data()).into();
            if self.last_chunk == Some((chunk.path_index, chunk.offset, digest)) {
                return;
            }
            let data = match assembler.accept_chunk(chunk) {
                Ok(data) => data,
                Err(error) => {
                    // Any stream error voids every path of the job.
                    self.stream_error = Some(error.code().to_string());
                    self.assembler = None;
                    self.discard_all();
                    return;
                }
            };
            self.last_chunk = Some((chunk.path_index, chunk.offset, digest));
            // The assembler accepted the index, so it is in range.
            let index = chunk.path_index as usize;
            if self.write_errors[index].is_some() {
                return;
            }
            if let Err(reason) = self.write(index, chunk.offset, data, record) {
                self.write_errors[index] = Some(reason);
                self.discard(index);
            }
        }

        fn write(
            &mut self,
            index: usize,
            offset: u64,
            data: &[u8],
            record: &mut Recorder<'_>,
        ) -> Result<(), NotReturned> {
            self.stage(index, record)?;
            let staged = self.staged[index].as_mut().unwrap();
            staged
                .file
                .seek(SeekFrom::Start(offset))
                .and_then(|_| staged.file.write_all(data))
                .map_err(local)
        }

        /// Create the path's private staging file, once.
        fn stage(&mut self, index: usize, record: &mut Recorder<'_>) -> Result<(), NotReturned> {
            if self.staged[index].is_some() {
                return Ok(());
            }
            let name = OsString::from(staging_name(&self.task_id, index));
            record(&StagingRecord {
                files: vec![self.private.path().join(&name)],
                dirs: Vec::new(),
            })
            .map_err(local)?;
            let file = self.private.open_new_file(&name).map_err(local)?;
            self.staged[index] = Some(Staged { name, file });
            Ok(())
        }

        /// Verify the manifest and place every verified file. Returns one
        /// entry per requested path, in request order.
        pub(crate) fn manifest(
            &mut self,
            manifest: &ReturnManifest,
            record: &mut Recorder<'_>,
        ) -> Vec<ReturnedPath> {
            let verified = match self.assembler.take() {
                None => Err(self
                    .stream_error
                    .clone()
                    .unwrap_or_else(|| "stream_refused".into())),
                Some(assembler) => assembler
                    .finish(manifest)
                    .map_err(|error| error.code().to_string()),
            };
            let verified = match verified {
                Ok(verified) => verified,
                Err(code) => {
                    self.settle();
                    return self.all_not_returned(NotReturned::Unverified { code });
                }
            };
            let mut report = Vec::with_capacity(self.paths.len());
            for (index, entry) in verified.into_iter().enumerate() {
                let outcome = match entry.outcome {
                    ReturnManifestOutcome::Returned { size, .. } => {
                        match self.write_errors[index].take() {
                            Some(reason) => ReturnOutcome::NotReturned { reason },
                            None => match self.place(index, size, record) {
                                Ok(()) => ReturnOutcome::Returned { bytes: size },
                                Err(reason) => ReturnOutcome::NotReturned { reason },
                            },
                        }
                    }
                    ReturnManifestOutcome::Failed { cause, .. } => ReturnOutcome::NotReturned {
                        reason: NotReturned::Runner {
                            cause: String::from(cause),
                        },
                    },
                    // A future outcome shape is not proof the bytes are good.
                    _ => ReturnOutcome::NotReturned {
                        reason: NotReturned::Unverified {
                            code: "unknown_outcome".into(),
                        },
                    },
                };
                if !matches!(outcome, ReturnOutcome::Returned { .. }) {
                    self.discard(index);
                }
                report.push(self.entry(index, outcome));
            }
            self.settle();
            report
        }

        /// The job ended without a manifest: nothing is placed.
        pub(crate) fn without_manifest(&mut self) -> Vec<ReturnedPath> {
            self.assembler = None;
            self.settle();
            self.all_not_returned(NotReturned::NoReport)
        }

        fn all_not_returned(&self, reason: NotReturned) -> Vec<ReturnedPath> {
            (0..self.paths.len())
                .map(|index| {
                    self.entry(
                        index,
                        ReturnOutcome::NotReturned {
                            reason: reason.clone(),
                        },
                    )
                })
                .collect()
        }

        fn entry(&self, index: usize, outcome: ReturnOutcome) -> ReturnedPath {
            ReturnedPath {
                path: self.paths[index].clone(),
                checkout_path: self.checkout_path(index),
                outcome,
            }
        }

        /// The requested path relative to the checkout, with `/` separators:
        /// the form the runner uses for the files it reports as changed.
        fn checkout_path(&self, index: usize) -> Option<String> {
            let checkout = self.checkout_root.as_deref()?;
            let cwd = fs::canonicalize(&self.cwd).unwrap_or_else(|_| self.cwd.clone());
            let relative = cwd.strip_prefix(checkout).ok()?;
            let parts: Vec<String> = relative
                .components()
                .chain(Path::new(&self.paths[index]).components())
                .filter_map(|component| match component {
                    Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect();
            Some(parts.join("/"))
        }

        /// Place a verified file at its destination.
        ///
        /// Every step works on open handles, never on a name a sandboxed
        /// command in the project could swap between a check and its use:
        /// - the destination's directory is opened component by component
        ///   from the project root without following any symlink;
        /// - a symlink or non-regular file already at the destination is
        ///   refused, never followed;
        /// - the mode is set through the staged file's own handle;
        /// - on the same filesystem the staged file is renamed from the
        ///   private control folder, which no sandboxed command can write,
        ///   into the destination directory: the rename replaces the
        ///   destination's directory entry, never anything it points to;
        /// - on another filesystem the bytes are copied into a recorded temp
        ///   file created exclusively in the destination directory, which is
        ///   then renamed over the destination within that directory, so the
        ///   destination is replaced in one step or not at all;
        /// - afterwards the destination must be the file written (same
        ///   device and inode), or the path is reported as not returned.
        fn place(
            &mut self,
            index: usize,
            size: u64,
            record: &mut Recorder<'_>,
        ) -> Result<(), NotReturned> {
            // A zero-byte file has no chunks, so it has no staging file yet.
            self.stage(index, record)?;
            let staged = self.staged[index].take().unwrap();
            let result = self.place_staged(index, size, &staged, record);
            // Renamed away on success; otherwise removed here.
            let _ = unlink_at(self.private.as_raw_fd(), &staged.name);
            result
        }

        fn place_staged(
            &mut self,
            index: usize,
            size: u64,
            staged: &Staged,
            record: &mut Recorder<'_>,
        ) -> Result<(), NotReturned> {
            let staged_stat = fstat(staged.file.as_raw_fd()).map_err(local)?;
            if staged_stat.st_size as u64 != size {
                return Err(NotReturned::LocalWrite {
                    error: format!("staged {} bytes, expected {size}", staged_stat.st_size),
                });
            }
            staged.file.sync_all().map_err(local)?;
            // The private entry must still be the file the bytes were written to.
            if !stat_at(self.private.as_raw_fd(), &staged.name)
                .map_err(local)?
                .is_some_and(|entry| same_file(&entry, &staged_stat))
            {
                return Err(NotReturned::LocalWrite {
                    error: "the private staging file changed before placement".into(),
                });
            }
            let (directory, directory_path, name) = self.open_destination_dir(index, record)?;
            let existing = stat_at(directory.as_raw_fd(), &name).map_err(local)?;
            if existing
                .as_ref()
                .is_some_and(|entry| entry.st_mode & libc::S_IFMT != libc::S_IFREG)
            {
                return Err(NotReturned::NotRegular);
            }
            // Keep an existing file's permissions; a new file gets the usual
            // owner-writable, world-readable mode instead of the staging 0600.
            let mode = existing
                .as_ref()
                .map_or(0o644, |entry| u32::from(entry.st_mode) & 0o7777);
            let directory_stat = fstat(directory.as_raw_fd()).map_err(local)?;
            let placed = if directory_stat.st_dev == staged_stat.st_dev && !copy_for_test() {
                staged
                    .file
                    .set_permissions(fs::Permissions::from_mode(mode))
                    .map_err(local)?;
                rename_at(
                    self.private.as_raw_fd(),
                    &staged.name,
                    directory.as_raw_fd(),
                    &name,
                )
                .map_err(local)?;
                staged_stat
            } else {
                self.copy_and_publish(
                    index,
                    staged,
                    &directory,
                    &directory_path,
                    &name,
                    mode,
                    record,
                )?
            };
            match stat_at(directory.as_raw_fd(), &name).map_err(local)? {
                Some(entry) if same_file(&entry, &placed) => Ok(()),
                _ => Err(NotReturned::LocalWrite {
                    error: "the file at this path changed while it was placed".into(),
                }),
            }
        }

        /// Copy the staged bytes to the destination on another filesystem and
        /// publish them in one step: write a recorded temp file, created
        /// exclusively through the destination directory's handle without
        /// following a symlink, set its mode and sync it through its own
        /// handle, then rename it over `name` within that directory.
        ///
        /// The temp name sits in a directory sandboxed commands can write, so
        /// one could swap it before the rename. That can at worst put a link
        /// or another file at the destination, a path the same command could
        /// already write directly; nothing outside the project is opened or
        /// changed, and the identity check after the rename then reports the
        /// path as not returned.
        #[allow(clippy::too_many_arguments)]
        fn copy_and_publish(
            &self,
            index: usize,
            staged: &Staged,
            directory: &OwnedFd,
            directory_path: &Path,
            name: &OsStr,
            mode: u32,
            record: &mut Recorder<'_>,
        ) -> Result<libc::stat, NotReturned> {
            let temp = OsString::from(staging_name(&self.task_id, index));
            // Saved before the temp file exists, so a restart can remove it.
            record(&StagingRecord {
                files: vec![directory_path.join(&temp)],
                dirs: Vec::new(),
            })
            .map_err(local)?;
            let mut out = create_exclusive_at(directory.as_raw_fd(), &temp).map_err(local)?;
            let written = fstat(out.as_raw_fd()).map_err(local)?;
            let published = (|| {
                let mut source = staged.file.try_clone()?;
                source.seek(SeekFrom::Start(0))?;
                let mut buffer = Vec::new();
                source.read_to_end(&mut buffer)?;
                out.write_all(&buffer)?;
                out.set_permissions(fs::Permissions::from_mode(mode))?;
                out.sync_all()?;
                before_publish_for_test(&directory_path.join(&temp))?;
                rename_at(directory.as_raw_fd(), &temp, directory.as_raw_fd(), name)
            })();
            if let Err(error) = published {
                if !stopped_for_test(&error) {
                    // Remove only the temp file created here, never an entry
                    // put in its place.
                    if stat_at(directory.as_raw_fd(), &temp)
                        .ok()
                        .flatten()
                        .is_some_and(|entry| same_file(&entry, &written))
                    {
                        let _ = unlink_at(directory.as_raw_fd(), &temp);
                    }
                }
                return Err(local(error));
            }
            Ok(written)
        }

        /// Open the destination's directory from the project root, one
        /// component at a time and following no symlink, creating missing
        /// directories after recording them. Returns it, its path and the
        /// file name.
        fn open_destination_dir(
            &mut self,
            index: usize,
            record: &mut Recorder<'_>,
        ) -> Result<(OwnedFd, PathBuf, OsString), NotReturned> {
            let parent = resolve_parent(&self.root, &self.cwd.join(&self.paths[index]))?;
            let relative = parent
                .existing
                .strip_prefix(&self.root)
                .map_err(|_| NotReturned::OutsideProject)?;
            let mut directory = open_directory(&self.root).map_err(local)?;
            for component in relative.components() {
                let Component::Normal(part) = component else {
                    return Err(NotReturned::OutsideProject);
                };
                directory =
                    open_directory_at(directory.as_raw_fd(), part).map_err(not_a_directory)?;
            }
            let mut path = parent.existing.clone();
            let missing: Vec<PathBuf> = parent
                .missing
                .iter()
                .map(|part| {
                    path.push(part);
                    path.clone()
                })
                .collect();
            if !missing.is_empty() {
                record(&StagingRecord {
                    files: Vec::new(),
                    dirs: missing.clone(),
                })
                .map_err(local)?;
            }
            for (part, full) in parent.missing.iter().zip(missing) {
                match make_directory_at(directory.as_raw_fd(), part) {
                    Ok(()) => self.created_dirs.push(full),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(local(error)),
                }
                directory =
                    open_directory_at(directory.as_raw_fd(), part).map_err(not_a_directory)?;
            }
            Ok((directory, path, parent.file_name))
        }

        fn discard(&mut self, index: usize) {
            if let Some(staged) = self.staged[index].take() {
                let _ = unlink_at(self.private.as_raw_fd(), &staged.name);
            }
        }

        fn discard_all(&mut self) {
            for index in 0..self.staged.len() {
                self.discard(index);
            }
        }

        /// Discard what is still staged and remove the directories this
        /// receiver created that nothing was placed in, deepest first.
        fn settle(&mut self) {
            self.discard_all();
            for dir in self.created_dirs.drain(..).rev() {
                remove_empty_dir(&dir);
            }
        }
    }

    impl Drop for ReturnReceiver {
        fn drop(&mut self) {
            self.settle();
        }
    }

    fn local(error: io::Error) -> NotReturned {
        NotReturned::LocalWrite {
            error: error.to_string(),
        }
    }

    /// A path component that is now a symlink or not a directory means the
    /// directory changed after it was checked to be inside the project.
    fn not_a_directory(error: io::Error) -> NotReturned {
        match error.raw_os_error() {
            Some(libc::ELOOP | libc::ENOTDIR) => NotReturned::OutsideProject,
            _ => local(error),
        }
    }

    fn same_file(a: &libc::stat, b: &libc::stat) -> bool {
        a.st_dev == b.st_dev && a.st_ino == b.st_ino
    }

    #[cfg(test)]
    thread_local! {
        static COPY_FOR_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Take the other-filesystem path even on one filesystem, for tests.
    #[cfg(test)]
    pub(crate) fn with_copy_for_test<T>(run: impl FnOnce() -> T) -> T {
        COPY_FOR_TEST.with(|copy| copy.set(true));
        let result = run();
        COPY_FOR_TEST.with(|copy| copy.set(false));
        result
    }

    #[cfg(test)]
    type BeforePublish = Box<dyn FnMut(&Path) -> io::Result<()>>;

    #[cfg(test)]
    thread_local! {
        static BEFORE_PUBLISH: std::cell::RefCell<Option<BeforePublish>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The error a test hook returns to simulate AFT stopping just before the
    /// other-filesystem rename: the temp file is left as a crash leaves it.
    #[cfg(test)]
    pub(crate) const STOPPED_FOR_TEST: &str = "AFT stopped before publishing (test)";

    /// Run `hook` with the temp file's path just before the rename that
    /// publishes it, for tests.
    #[cfg(test)]
    pub(crate) fn with_before_publish_for_test<T>(
        hook: impl FnMut(&Path) -> io::Result<()> + 'static,
        run: impl FnOnce() -> T,
    ) -> T {
        BEFORE_PUBLISH.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        let result = run();
        BEFORE_PUBLISH.with(|slot| *slot.borrow_mut() = None);
        result
    }

    fn before_publish_for_test(_temp: &Path) -> io::Result<()> {
        #[cfg(test)]
        if let Some(hook) = BEFORE_PUBLISH.with(|slot| slot.borrow_mut().take()) {
            let mut hook = hook;
            let result = hook(_temp);
            BEFORE_PUBLISH.with(|slot| *slot.borrow_mut() = Some(hook));
            return result;
        }
        Ok(())
    }

    fn stopped_for_test(_error: &io::Error) -> bool {
        #[cfg(test)]
        return _error.to_string() == STOPPED_FOR_TEST;
        #[cfg(not(test))]
        false
    }

    fn copy_for_test() -> bool {
        #[cfg(test)]
        return COPY_FOR_TEST.with(std::cell::Cell::get);
        #[cfg(not(test))]
        false
    }

    fn c_name(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
    }

    fn open_directory(path: &Path) -> io::Result<OwnedFd> {
        let path = c_name(path.as_os_str())?;
        // SAFETY: `path` is a NUL-terminated string that outlives the call.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just opened and is owned by nothing else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn open_directory_at(parent: RawFd, name: &OsStr) -> io::Result<OwnedFd> {
        let name = c_name(name)?;
        // SAFETY: `parent` is an open directory and `name` outlives the call.
        let fd = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just opened and is owned by nothing else.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn make_directory_at(parent: RawFd, name: &OsStr) -> io::Result<()> {
        let name = c_name(name)?;
        // SAFETY: `parent` is an open directory and `name` outlives the call.
        if unsafe { libc::mkdirat(parent, name.as_ptr(), 0o755) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn create_exclusive_at(directory: RawFd, name: &OsStr) -> io::Result<fs::File> {
        let name = c_name(name)?;
        // SAFETY: `directory` is an open directory and `name` outlives the call.
        let fd = unsafe {
            libc::openat(
                directory,
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just opened and is owned by nothing else.
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }

    /// The entry `name` in `directory`, not following a symlink; `None` when
    /// there is no such entry.
    fn stat_at(directory: RawFd, name: &OsStr) -> io::Result<Option<libc::stat>> {
        let name = c_name(name)?;
        let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the pointers are valid for the call; `entry` is written by it.
        let result = unsafe {
            libc::fstatat(
                directory,
                name.as_ptr(),
                entry.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result != 0 {
            let error = io::Error::last_os_error();
            return if error.kind() == io::ErrorKind::NotFound {
                Ok(None)
            } else {
                Err(error)
            };
        }
        // SAFETY: `fstatat` succeeded, so it initialised `entry`.
        Ok(Some(unsafe { entry.assume_init() }))
    }

    fn fstat(fd: RawFd) -> io::Result<libc::stat> {
        let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `fd` is open and `entry` is written by the call.
        if unsafe { libc::fstat(fd, entry.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fstat` succeeded, so it initialised `entry`.
        Ok(unsafe { entry.assume_init() })
    }

    fn rename_at(from_dir: RawFd, from: &OsStr, to_dir: RawFd, to: &OsStr) -> io::Result<()> {
        let from = c_name(from)?;
        let to = c_name(to)?;
        // SAFETY: both directories are open and both names outlive the call.
        if unsafe { libc::renameat(from_dir, from.as_ptr(), to_dir, to.as_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn unlink_at(directory: RawFd, name: &OsStr) -> io::Result<()> {
        let name = c_name(name)?;
        // SAFETY: `directory` is open and `name` outlives the call.
        if unsafe { libc::unlinkat(directory, name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "return_files_tests.rs"]
mod tests;
