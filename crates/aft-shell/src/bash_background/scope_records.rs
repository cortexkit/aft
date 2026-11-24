use super::*;

#[cfg(target_os = "linux")]
pub(crate) const TASK_SCOPE_RECORD_MAX_BYTES: u64 = 1_048_576;

#[cfg(target_os = "linux")]
#[derive(Clone)]
pub(crate) struct TaskRecordRef {
    pub task_id: String,
    pub terminal: bool,
    pub session_dir: PathBuf,
    session: Arc<PinnedDir>,
    ancestors: Vec<Arc<PinnedDir>>,
}

#[cfg(target_os = "linux")]
pub(crate) fn reread_storage_task_record(record: &TaskRecordRef) -> io::Result<PersistedTask> {
    if record.session.path() != record.session_dir {
        return Err(io::Error::other("task record session changed"));
    }
    for directory in &record.ancestors {
        verify_scope_directory(directory)?;
    }
    let task = resolve_task_layout_from_session(Arc::clone(&record.session), &record.task_id)?;
    let metadata = read_task_at_limited(&task, Some(TASK_SCOPE_RECORD_MAX_BYTES))?;
    if metadata.task_id != record.task_id {
        return Err(io::Error::other("task record identity changed"));
    }
    Ok(metadata)
}

#[cfg(target_os = "linux")]
fn verify_scope_directory(directory: &PinnedDir) -> io::Result<()> {
    if !directory.same_identity(&PinnedDir::open(directory.path())?)? {
        return Err(io::Error::other("task record directory changed"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum StorageRecordLevel {
    Root,
    Harness,
    Sessions,
    Tasks,
}

#[cfg(target_os = "linux")]
struct StorageRecordFrame {
    directory: Arc<PinnedDir>,
    entries: DirectoryEntries,
    level: StorageRecordLevel,
}

#[cfg(target_os = "linux")]
pub(crate) struct StorageTaskRecordCursor {
    root: Arc<PinnedDir>,
    frames: Vec<StorageRecordFrame>,
    seen: std::collections::HashSet<(PathBuf, String)>,
    owners: std::collections::HashMap<String, PathBuf>,
    pub(crate) rejected_task_ids: std::collections::HashSet<String>,
}

#[cfg(target_os = "linux")]
pub(crate) struct StorageTaskRecordBatch {
    pub records: Vec<TaskRecordRef>,
    pub finished: bool,
    pub entries_scanned: usize,
    pub skipped_oversized: usize,
}

#[cfg(target_os = "linux")]
impl StorageTaskRecordCursor {
    pub(crate) fn new(storage_dir: &Path) -> io::Result<Self> {
        let root = Arc::new(PinnedDir::open(storage_dir)?);
        let entries = root.unix_entries()?;
        Ok(Self {
            root: Arc::clone(&root),
            frames: vec![StorageRecordFrame {
                directory: root,
                entries,
                level: StorageRecordLevel::Root,
            }],
            seen: Default::default(),
            owners: Default::default(),
            rejected_task_ids: Default::default(),
        })
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn discover_storage_task_records(
    storage_dir: &Path,
    cursor: &mut StorageTaskRecordCursor,
) -> io::Result<StorageTaskRecordBatch> {
    if storage_dir != cursor.root.path() {
        return Err(io::Error::other("task record cursor root changed"));
    }
    verify_scope_directory(&cursor.root)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(25);
    let mut batch = StorageTaskRecordBatch {
        records: Vec::new(),
        finished: false,
        entries_scanned: 0,
        skipped_oversized: 0,
    };
    // Only one entry can continue after the time limit. Each entry uses a constant number of filesystem operations.
    while batch.entries_scanned < super::super::gc_cursor::GC_ENTRY_BUDGET
        && batch.records.len() < super::super::gc_cursor::GC_ENTRY_BUDGET
        && std::time::Instant::now() < deadline
    {
        let Some(frame) = cursor.frames.last_mut() else {
            batch.finished = true;
            break;
        };
        verify_scope_directory(&frame.directory)?;
        let Some(entry) = frame.entries.next() else {
            cursor.frames.pop();
            continue;
        };
        batch.entries_scanned += 1;
        let (name, may_be_dir) = entry?;
        let Some(text) = name.to_str() else {
            continue;
        };
        if matches!(frame.level, StorageRecordLevel::Tasks) {
            let id = text.strip_suffix(".json").unwrap_or(text);
            if validate_task_id(id).is_err() {
                continue;
            }
            let session_dir = frame.directory.path().to_owned();
            if !cursor.seen.insert((session_dir.clone(), id.into())) {
                continue;
            }
            if cursor
                .owners
                .insert(id.into(), session_dir.clone())
                .is_some_and(|previous| previous != session_dir)
            {
                cursor.rejected_task_ids.insert(id.into());
            }
            let session = Arc::clone(&frame.directory);
            let ancestors = cursor
                .frames
                .iter()
                .map(|frame| Arc::clone(&frame.directory))
                .collect();
            let record = TaskRecordRef {
                task_id: id.into(),
                terminal: false,
                session_dir,
                session,
                ancestors,
            };
            match reread_storage_task_record(&record) {
                Ok(metadata) => batch.records.push(TaskRecordRef {
                    terminal: metadata.is_terminal(),
                    ..record
                }),
                Err(error) => {
                    if error.kind() == io::ErrorKind::FileTooLarge {
                        batch.skipped_oversized += 1;
                    }
                    cursor.rejected_task_ids.insert(id.into());
                }
            }
            continue;
        }
        let level = match frame.level {
            StorageRecordLevel::Root | StorageRecordLevel::Harness if text == "bash-tasks" => {
                Some(StorageRecordLevel::Sessions)
            }
            StorageRecordLevel::Root if is_harness_store_name(text) => {
                Some(StorageRecordLevel::Harness)
            }
            StorageRecordLevel::Sessions => Some(StorageRecordLevel::Tasks),
            _ => None,
        };
        if let Some(level) = level.filter(|_| may_be_dir) {
            let directory = match frame.directory.open_dir_at(&name) {
                Ok(directory) => Arc::new(directory),
                Err(error) if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) => {
                    continue
                }
                Err(error) => return Err(error),
            };
            let entries = directory.unix_entries()?;
            cursor.frames.push(StorageRecordFrame {
                directory,
                entries,
                level,
            });
        }
    }
    batch.finished |= cursor.frames.is_empty();
    Ok(batch)
}

pub fn read_task_at(task: &ResolvedTask) -> io::Result<PersistedTask> {
    read_task_at_limited(task, None)
}

fn read_task_at_limited(task: &ResolvedTask, limit: Option<u64>) -> io::Result<PersistedTask> {
    let name = match task.paths.layout {
        TaskLayout::Directory => OsString::from(METADATA_FILE),
        TaskLayout::Flat => OsString::from(format!("{}.json", task.paths.task_id)),
    };
    let mut file = open_metadata_through_replacement(&task.dirs.control, &name)?;
    let metadata = read_task_file_limited(&mut file, &task.dirs.control.path().join(&name), limit)?;
    if metadata.task_id != task.paths.task_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "background task metadata identity does not match its layout name",
        ));
    }
    Ok(metadata)
}

pub(super) fn read_task_file(file: &mut File, path: &Path) -> io::Result<PersistedTask> {
    read_task_file_limited(file, path, None)
}

fn read_task_file_limited(
    file: &mut File,
    path: &Path,
    limit: Option<u64>,
) -> io::Result<PersistedTask> {
    if let Some(limit) = limit {
        if file.metadata()?.len() > limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "task scope record is oversized",
            ));
        }
    }
    #[cfg(test)]
    work_counts::record_read();
    file.seek(SeekFrom::Start(0))?;
    let mut content = String::new();
    if let Some(limit) = limit {
        // A size check can race with appends. Bound the read too.
        file.take(limit.saturating_add(1))
            .read_to_string(&mut content)?;
        if content.len() as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                "task scope record is oversized",
            ));
        }
    } else {
        file.read_to_string(&mut content)?;
    }
    // Ordinary records need only the typed decode, not an allocated Value
    // tree followed by the same parse again. On decode failure, still check
    // the version before returning that error: future formats may have a
    // different shape and must be refused, not quarantined as corrupt tasks.
    #[cfg(test)]
    work_counts::record_parse();
    let decoded = decode_persisted_task(&content);
    let version = match &decoded {
        Ok(task) => Some(u64::from(task.schema_version)),
        Err(_) => {
            #[cfg(test)]
            work_counts::record_parse();
            serde_json::from_str::<serde_json::Value>(&content)
                .ok()
                .and_then(|value| value.get("schema_version")?.as_u64())
        }
    };
    crate::persisted_format::gate(
        crate::persisted_format::PersistedStore::BashTask,
        path,
        path,
        version,
    )
    .map_err(crate::persisted_format::UnsupportedPersistedFormat::into_io_error)?;
    let task = decoded.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !matches!(task.schema_version, 2 | 3 | 4 | 5 | 6 | SCHEMA_VERSION) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported background task schema_version {} (expected 2, 3, 4, 5, 6, or {SCHEMA_VERSION})",
                task.schema_version
            ),
        ));
    }
    validate_task_id(&task.task_id)?;
    Ok(task)
}

pub fn write_task(path: &Path, task: &PersistedTask) -> io::Result<()> {
    crate::production_storage::refuse_write(path)?;
    validate_task_id(&task.task_id)?;
    if let Some(parent) = path.parent() {
        crate::private_storage::create_dir_all(parent)?;
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let dir = PinnedDir::open(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "metadata path has no name"))?;
    write_task_in_dir(&dir, name, task, ReplaceSync::for_task(task))
}
