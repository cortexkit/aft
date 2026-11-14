//! Budgeted stable external sorting of variable-size byte records.
//!
//! Keys use lexicographic byte order (memcmp, with shorter prefixes first).
//! Encode unsigned integers big-endian if numeric order is wanted. Equal keys
//! retain push order, including across intermediate merges and checkpoints.
//!
//! The budget covers allocated capacities of record bytes, sort indexes, run
//! metadata, merge heads/heap/readers, and read/write buffers. Fixed-size stack
//! state, allocator bookkeeping, paths, and the shared segment file table are
//! not included. Records returned by `Iterator::next` become caller-owned and
//! cease to count; use [`SortedRecords::next_record`] to avoid those copies.
//! There is no cache of run contents, and no allocation proportional to output
//! size. Oversized records and merges that cannot fit are typed errors.
//!
//! Each spill is durable before an atomic, checksummed manifest publishes it.
//! [`Sorter::checkpoint`] explicitly retains a workspace for [`Sorter::resume`].
//! Ordinary drop, exhaustion, and errors remove the entire workspace. A killed
//! process leaves its last manifest reusable; unmanifested files are discarded
//! on resume. One caller must exclusively own a workspace while using it.

mod manifest;
mod memory;
mod run;
#[cfg(test)]
mod tests;

use std::collections::TryReserveError;
use std::io;
use std::mem::size_of;
use std::path::{Path, PathBuf};

use manifest::Workspace;
use memory::{Buffer, Memory};
use run::{Merge, RunMeta, Writer};

/// Caller policy checked before disk writes, including intermediate merges.
#[derive(Clone)]
pub struct WriteGuard(std::sync::Arc<dyn Fn() -> io::Result<()> + Send + Sync>);

impl std::fmt::Debug for WriteGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriteGuard")
    }
}

impl WriteGuard {
    pub fn new(check: impl Fn() -> io::Result<()> + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(check))
    }

    pub fn check(&self) -> io::Result<()> {
        (self.0)()
    }
}

/// Resource limits for one sort. Buffers are not shared between sorts.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub memory_budget: usize,
    /// Bytes per checksummed I/O block. Also the read buffer size of each run.
    pub buffer_bytes: usize,
    /// Maximum inputs to a merge. Larger run sets use intermediate passes.
    pub fan_in: usize,
}

impl Config {
    pub fn new(memory_budget: usize) -> Self {
        Self {
            memory_budget,
            buffer_bytes: 64 * 1024,
            fan_in: 16,
        }
    }

    fn validate(self) -> Result<Self, Error> {
        if self.buffer_bytes == 0
            || self.buffer_bytes > u32::MAX as usize
            || self.fan_in < 2
            || self.fan_in > crate::segment::MAX_OPEN_SEGMENT_FILES - 2
        {
            return Err(Error::Config(
                "nonzero block size and fan-in in 2..=1022 required",
            ));
        }
        Ok(self)
    }
}

/// Capacity counters, not merely the number of bytes currently filled.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub budget_bytes: usize,
    pub buffered_bytes: usize,
    pub peak_buffered_bytes: usize,
    /// All files in the workspace, including manifests and partially written runs.
    pub disk_bytes: u64,
    pub peak_disk_bytes: u64,
    /// Includes intermediate passes and manifests, not just the final output.
    pub bytes_written: u64,
    pub runs_written: u64,
    pub merge_passes: u64,
    pub peak_merge_readers: usize,
    /// High-water sum of allocated I/O buffer capacities across merge inputs.
    pub peak_merge_io_bytes: usize,
    /// High-water sum of allocated capacities for the current input records
    /// held for merging (one record per run).
    pub peak_merge_head_bytes: usize,
    pub allocations: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("spill I/O: {0}")]
    Io(#[from] io::Error),
    #[error("corrupt or torn spill file {path}: {reason}")]
    Corrupt { path: PathBuf, reason: &'static str },
    #[error("spill needs {needed} buffered bytes but budget is {budget}")]
    Budget { needed: usize, budget: usize },
    #[error("invalid spill configuration: {0}")]
    Config(&'static str),
    #[error("spill integer overflow")]
    Overflow,
    #[error("spill allocation failed: {0}")]
    Allocation(TryReserveError),
    #[error("spill sorter already failed")]
    Failed,
}

fn corrupt(path: &Path, reason: &'static str) -> Error {
    Error::Corrupt {
        path: path.to_owned(),
        reason,
    }
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    start: usize,
    key_len: usize,
    end: usize,
    sequence: u64,
}

#[derive(Debug)]
struct Tail {
    entries: Buffer<Entry>,
    bytes: Buffer<u8>,
}

impl Tail {
    fn new(memory: &Memory, config: Config) -> Result<Self, Error> {
        let available = memory.available().saturating_sub(config.buffer_bytes);
        let count = available / 3 / size_of::<Entry>();
        if count == 0 {
            return Err(Error::Budget {
                needed: config.memory_budget.saturating_add(1),
                budget: config.memory_budget,
            });
        }
        let entries = Buffer::new(count, memory)?;
        let bytes = Buffer::new(available - count * size_of::<Entry>(), memory)?;
        Ok(Self { entries, bytes })
    }

    fn sort(&mut self) {
        let bytes = &self.bytes;
        self.entries.sort_unstable_by(|a, b| {
            bytes[a.start..a.start + a.key_len]
                .cmp(&bytes[b.start..b.start + b.key_len])
                .then(a.sequence.cmp(&b.sequence))
        });
    }
}

/// Stable byte-key sorter. The private child directory is created inside `root`.
/// The caller must supply a private root; on Windows files inherit the root
/// directory's access-control list.
#[derive(Debug)]
pub struct Sorter {
    workspace: Option<Workspace>,
    config: Config,
    memory: Memory,
    runs: Buffer<RunMeta>,
    tail: Option<Tail>,
    sequence: u64,
    fixed_key_bytes: usize,
    source_range: u64,
    tail_ranges: Option<(u64, u64)>,
}

impl Sorter {
    pub fn new(root: &Path, config: Config) -> Result<Self, Error> {
        Self::create(root, config, 0, true)
    }

    /// Sort fixed-width keys without values or per-record disk framing. Equal
    /// keys are identical records; checksummed blocks still detect torn writes.
    pub fn fixed_keys(root: &Path, config: Config, bytes: usize) -> Result<Self, Error> {
        if bytes == 0 {
            return Err(Error::Config("fixed key width must be nonzero"));
        }
        Self::create(root, config, bytes, true)
    }

    /// Disposable fixed runs: checksummed but without manifests, fsync or
    /// durable renames. A build restarts rather than resuming these runs.
    pub fn ephemeral_fixed_keys(root: &Path, config: Config, bytes: usize) -> Result<Self, Error> {
        if bytes == 0 {
            return Err(Error::Config("fixed key width must be nonzero"));
        }
        Self::create(root, config, bytes, false)
    }

    fn create(
        root: &Path,
        config: Config,
        fixed_key_bytes: usize,
        durable: bool,
    ) -> Result<Self, Error> {
        let config = config.validate()?;
        let memory = Memory::new(config.memory_budget);
        // A u64 sequence has at most 64 levels in any radix >= 2.
        let runs = Buffer::new(
            config.fan_in.checked_mul(64).ok_or(Error::Overflow)?,
            &memory,
        )?;
        let mut workspace = Workspace::create(root, &memory, durable)?;
        workspace.fixed_key_bytes = fixed_key_bytes as u64;
        Ok(Self {
            workspace: Some(workspace),
            config,
            memory,
            runs,
            tail: None,
            sequence: 0,
            fixed_key_bytes,
            source_range: 0,
            tail_ranges: None,
        })
    }

    /// Reclaim exclusive ownership of a retained or crash-left workspace.
    /// Invalid manifests/runs are errors; no older snapshot is silently used.
    pub fn resume(directory: &Path, config: Config) -> Result<Self, Error> {
        let config = config.validate()?;
        let memory = Memory::new(config.memory_budget);
        let runs = Buffer::new(
            config.fan_in.checked_mul(64).ok_or(Error::Overflow)?,
            &memory,
        )?;
        let workspace = Workspace::open(directory, &memory)?;
        let mut this = Self {
            workspace: Some(workspace),
            config,
            memory,
            runs,
            tail: None,
            sequence: 0,
            fixed_key_bytes: 0,
            source_range: 0,
            tail_ranges: None,
        };
        let result = this
            .workspace
            .as_mut()
            .expect("workspace")
            .load(&mut this.runs, &mut this.sequence);
        this.checked(result)?;
        this.fixed_key_bytes =
            usize::try_from(this.workspace.as_ref().expect("workspace").fixed_key_bytes)
                .map_err(|_| Error::Overflow)?;
        Ok(this)
    }

    pub fn stats(&self) -> Stats {
        self.memory.stats()
    }

    pub fn set_write_guard(&mut self, guard: WriteGuard) -> Result<(), Error> {
        guard.check()?;
        self.workspace.as_mut().ok_or(Error::Failed)?.guard = Some(guard);
        Ok(())
    }

    pub fn directory(&self) -> Option<&Path> {
        self.workspace.as_ref().map(|w| w.path.as_path())
    }

    /// Lists the currently published run names without reading their contents.
    /// Resume a checkpoint first to list/reuse its last committed run set. Each
    /// returned path is caller-owned and excluded from the buffer budget.
    pub fn finished_runs(&self) -> impl Iterator<Item = FinishedRun> + '_ {
        self.runs.iter().map(|meta| FinishedRun {
            first_source_range: meta.first_source_range,
            last_source_range: meta.last_source_range,
            path: meta.path(&self.workspace.as_ref().expect("workspace").path),
            records: meta.records,
            max_record_bytes: meta.max_record,
        })
    }

    /// Copies one key/value into the budgeted arena. Any error invalidates the
    /// sorter and removes its workspace; previously pushed records are not kept.
    pub fn push(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        let result = self.push_inner(key, value);
        self.checked(result)
    }

    /// Label subsequent input records with their fragment range. Labels affect
    /// run metadata only, never key order or spill scheduling.
    pub fn set_source_range(&mut self, range: u64) {
        self.source_range = range;
    }

    fn push_inner(&mut self, key: &[u8], value: &[u8]) -> Result<(), Error> {
        if self.workspace.is_none() {
            return Err(Error::Failed);
        }
        if self.fixed_key_bytes != 0 && (key.len() != self.fixed_key_bytes || !value.is_empty()) {
            return Err(Error::Config(
                "fixed records require exactly one fixed-width key",
            ));
        }
        let len = key.len().checked_add(value.len()).ok_or(Error::Overflow)?;
        if self.tail.is_none() {
            self.tail = Some(Tail::new(&self.memory, self.config)?);
        }
        let tail = self.tail.as_ref().expect("tail");
        if len > tail.bytes.capacity() {
            return Err(Error::Budget {
                needed: self
                    .config
                    .memory_budget
                    .saturating_add(len - tail.bytes.capacity()),
                budget: self.config.memory_budget,
            });
        }
        if tail.entries.len() == tail.entries.capacity()
            || len > tail.bytes.capacity() - tail.bytes.len()
        {
            self.spill()?;
            self.tail = Some(Tail::new(&self.memory, self.config)?);
        }
        let next = self.sequence.checked_add(1).ok_or(Error::Overflow)?;
        let tail = self.tail.as_mut().expect("tail");
        let start = tail.bytes.len();
        tail.bytes.extend(key);
        tail.bytes.extend(value);
        tail.entries.push(Entry {
            start,
            key_len: key.len(),
            end: tail.bytes.len(),
            sequence: self.sequence,
        });
        self.sequence = next;
        self.tail_ranges = Some(
            self.tail_ranges
                .map_or((self.source_range, self.source_range), |(first, last)| {
                    (first.min(self.source_range), last.max(self.source_range))
                }),
        );
        Ok(())
    }

    fn checked<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if result.is_err() {
            self.tail = None;
            self.runs.clear();
            self.workspace = None;
        }
        result
    }

    fn spill(&mut self) -> Result<(), Error> {
        let Some(mut tail) = self.tail.take() else {
            return Ok(());
        };
        if tail.entries.is_empty() {
            return Ok(());
        }
        tail.sort();
        let workspace = self.workspace.as_mut().ok_or(Error::Failed)?;
        let mut writer = Writer::new(
            workspace,
            self.config.buffer_bytes,
            &self.memory,
            self.fixed_key_bytes,
        )?;
        for entry in tail.entries.iter() {
            writer.record(
                &tail.bytes[entry.start..entry.start + entry.key_len],
                &tail.bytes[entry.start + entry.key_len..entry.end],
                entry.sequence,
            )?;
        }
        let meta = writer.finish(
            0,
            self.tail_ranges
                .take()
                .expect("nonempty tail has source ranges"),
        )?;
        drop(tail);
        self.runs.push(meta);
        self.publish()?;
        // Leveled compaction bounds the manifest by 64 * fan_in entries and
        // avoids repeatedly rewriting a growing run for every small spill.
        loop {
            let level = self.runs.iter().find_map(|r| {
                (self
                    .runs
                    .iter()
                    .filter(|other| other.level == r.level)
                    .count()
                    >= self.config.fan_in)
                    .then_some(r.level)
            });
            let Some(level) = level else {
                break;
            };
            let mut inputs = Buffer::new(self.config.fan_in, &self.memory)?;
            let mut index = 0;
            while index < self.runs.len() && inputs.len() < self.config.fan_in {
                if self.runs[index].level == level {
                    inputs.push(self.runs.remove(index));
                } else {
                    index += 1;
                }
            }
            self.merge_inputs(inputs, level.checked_add(1).ok_or(Error::Overflow)?)?;
        }
        Ok(())
    }

    fn merge_inputs(&mut self, mut inputs: Buffer<RunMeta>, level: u64) -> Result<(), Error> {
        // Reduce fan-in further when heads would consume too much of the budget.
        while inputs.len() > 1 {
            let mut chosen = Buffer::new(inputs.len(), &self.memory)?;
            let mut cost = self.config.buffer_bytes;
            while let Some(meta) = inputs.last().copied() {
                let more = Merge::reader_cost(meta, self.config.buffer_bytes)?;
                if cost.checked_add(more).ok_or(Error::Overflow)? > self.memory.available() {
                    break;
                }
                cost += more;
                chosen.push(inputs.pop().expect("input"));
            }
            if chosen.len() < 2 {
                return Err(Error::Budget {
                    needed: self.config.memory_budget.saturating_add(1),
                    budget: self.config.memory_budget,
                });
            }
            let workspace = self.workspace.as_mut().ok_or(Error::Failed)?;
            let mut merge = Merge::new(
                &workspace.path,
                &chosen,
                self.config.buffer_bytes,
                &self.memory,
            )?;
            let mut writer = Writer::new(
                workspace,
                self.config.buffer_bytes,
                &self.memory,
                self.fixed_key_bytes,
            )?;
            while let Some(record) = merge.next_record()? {
                writer.record(record.key, record.value, record.sequence)?;
            }
            let ranges = (
                chosen
                    .iter()
                    .map(|meta| meta.first_source_range)
                    .min()
                    .expect("merge inputs"),
                chosen
                    .iter()
                    .map(|meta| meta.last_source_range)
                    .max()
                    .expect("merge inputs"),
            );
            let meta = writer.finish(level, ranges)?;
            drop(merge);
            self.memory.update(|s| s.merge_passes += 1);
            // Publish replacements before removing inputs. A crash sees either
            // complete set, never a manifest naming already removed files.
            for input in inputs.iter() {
                self.runs.push(*input);
            }
            self.runs.push(meta);
            self.publish()?;
            for old in chosen.iter() {
                workspace_remove(&mut self.workspace, old)?;
            }
            for _ in 0..inputs.len() + 1 {
                self.runs.pop();
            }
            inputs.push(meta);
        }
        if let Some(meta) = inputs.pop() {
            self.runs.push(meta);
        }
        Ok(())
    }

    fn publish(&mut self) -> Result<(), Error> {
        self.workspace
            .as_mut()
            .ok_or(Error::Failed)?
            .publish(&self.runs, self.sequence)
    }

    /// Retains durable named runs and their manifest instead of cleaning up on
    /// drop. The returned directory can be passed to `resume` in another process.
    pub fn checkpoint(mut self) -> Result<Checkpoint, Error> {
        if !self
            .workspace
            .as_ref()
            .is_some_and(|workspace| workspace.durable)
        {
            return Err(Error::Config("ephemeral sorts cannot checkpoint"));
        }
        let result = self.spill().and_then(|()| self.publish());
        self.checked(result)?;
        let mut workspace = self.workspace.take().ok_or(Error::Failed)?;
        workspace.retain = true;
        Ok(Checkpoint {
            directory: workspace.path.clone(),
            finished_runs: self.runs.len(),
            stats: self.stats(),
        })
    }

    /// Sorts the tail and returns a bounded, fallible streaming merge. A tail
    /// with no prior spill stays in memory if an owned output record also fits.
    /// Otherwise it becomes a durable input run so that sort and merge
    /// allocations do not overlap.
    pub fn finish(mut self) -> Result<SortedRecords, Error> {
        if self.workspace.is_none() {
            return Err(Error::Failed);
        }
        if self.runs.is_empty() {
            let max_record = self.tail.as_ref().map_or(0, |tail| {
                tail.entries
                    .iter()
                    .map(|entry| entry.end - entry.start)
                    .max()
                    .unwrap_or(0)
            });
            if max_record <= self.memory.available() {
                if let Some(tail) = &mut self.tail {
                    tail.sort();
                }
                return Ok(SortedRecords {
                    workspace: self.workspace.take(),
                    memory: self.memory.clone(),
                    source: Some(Source::Tail(self.tail.take(), 0)),
                });
            }
            // Write the in-memory tail to a run when an owned output copy would
            // not fit alongside it. Then check that merge records and output fit.
        }
        let result = self.spill();
        self.checked(result)?;
        loop {
            let cost = Merge::cost(&self.runs, self.config.buffer_bytes, true)?;
            if self.runs.len() <= self.config.fan_in && cost <= self.memory.available() {
                break;
            }
            if self.runs.len() < 2 {
                return Err(Error::Budget {
                    needed: self.memory.stats().buffered_bytes.saturating_add(cost),
                    budget: self.config.memory_budget,
                });
            }
            let count = self.config.fan_in.min(self.runs.len());
            let mut inputs = Buffer::new(count, &self.memory)?;
            for _ in 0..count {
                inputs.push(self.runs.pop().expect("run"));
            }
            let level = inputs.iter().map(|r| r.level).max().unwrap_or(0) + 1;
            self.merge_inputs(inputs, level)?;
        }
        let merge = Merge::new(
            &self.workspace.as_ref().expect("workspace").path,
            &self.runs,
            self.config.buffer_bytes,
            &self.memory,
        )?;
        Ok(SortedRecords {
            workspace: self.workspace.take(),
            memory: self.memory.clone(),
            source: Some(Source::Merge(merge)),
        })
    }
}

fn workspace_remove(workspace: &mut Option<Workspace>, meta: &RunMeta) -> Result<(), Error> {
    workspace.as_mut().ok_or(Error::Failed)?.remove_run(meta)
}

/// A retained workspace is not deleted when this descriptor is dropped.
#[derive(Debug)]
pub struct Checkpoint {
    pub directory: PathBuf,
    pub finished_runs: usize,
    pub stats: Stats,
}

/// One durable, immutable run named by the current manifest.
#[derive(Debug)]
pub struct FinishedRun {
    pub first_source_range: u64,
    pub last_source_range: u64,
    pub path: PathBuf,
    pub records: u64,
    pub max_record_bytes: u64,
}

/// Borrowed output, valid until the next call to the iterator.
#[derive(Clone, Copy, Debug)]
pub struct RecordRef<'a> {
    pub key: &'a [u8],
    pub value: &'a [u8],
    sequence: u64,
}

/// An owned output record; its allocation belongs to the caller.
#[derive(Debug, PartialEq, Eq)]
pub struct Record {
    bytes: Vec<u8>,
    key_len: usize,
}
impl Record {
    pub fn key(&self) -> &[u8] {
        &self.bytes[..self.key_len]
    }
    pub fn value(&self) -> &[u8] {
        &self.bytes[self.key_len..]
    }
}

#[derive(Debug)]
enum Source {
    Tail(Option<Tail>, usize),
    Merge(Merge),
}

#[derive(Debug)]
pub struct SortedRecords {
    workspace: Option<Workspace>,
    memory: Memory,
    source: Option<Source>,
}

impl SortedRecords {
    pub fn stats(&self) -> Stats {
        self.memory.stats()
    }

    /// A lending iterator: no allocation or copy of the returned record. On an
    /// error the iterator returns no further records and deletes its workspace
    /// before returning the error.
    pub fn next_record(&mut self) -> Result<Option<RecordRef<'_>>, Error> {
        let advance = match self.source.as_mut() {
            Some(Source::Merge(merge)) => merge.advance(),
            Some(Source::Tail(tail, index)) => {
                if tail.as_ref().is_some_and(|t| *index < t.entries.len()) {
                    *index += 1;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            None => Ok(false),
        };
        match advance {
            Ok(true) => {}
            other => {
                self.source = None;
                self.workspace = None;
                return other.map(|_| None);
            }
        }
        Ok(match self.source.as_ref().expect("source") {
            Source::Merge(merge) => Some(merge.current()),
            Source::Tail(tail, index) => {
                let tail = tail.as_ref().expect("tail");
                let e = tail.entries[*index - 1];
                Some(RecordRef {
                    key: &tail.bytes[e.start..e.start + e.key_len],
                    value: &tail.bytes[e.start + e.key_len..e.end],
                    sequence: e.sequence,
                })
            }
        })
    }
}

impl Iterator for SortedRecords {
    type Item = Result<Record, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        let memory = self.memory.clone();
        let result = (|| {
            let Some(record) = self.next_record()? else {
                return Ok(None);
            };
            let mut bytes = Buffer::new(record.key.len() + record.value.len(), &memory)?;
            bytes.extend(record.key);
            bytes.extend(record.value);
            Ok(Some(Record {
                bytes: bytes.into_untracked(),
                key_len: record.key.len(),
            }))
        })();
        match result {
            Ok(record) => record.map(Ok),
            Err(error) => {
                self.source = None;
                self.workspace = None;
                Some(Err(error))
            }
        }
    }
}

impl Drop for SortedRecords {
    fn drop(&mut self) {
        // Close readers before deleting files, including on Windows.
        self.source = None;
        self.workspace = None;
    }
}

/// Deletes only marked spill children for which the caller confirms no process
/// will use the directory again. The predicate must exclude live builders and
/// retained checkpoints: age or a PID alone is not a safe ownership test.
pub fn sweep_abandoned(root: &Path, abandoned: impl FnMut(&Path) -> bool) -> Result<usize, Error> {
    manifest::sweep(root, abandoned)
}
