//! Runs have a checksummed header and independently verified data blocks.
//! Record framing can cross block boundaries; bytes are never exposed before
//! the block containing them passes its checksum.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};

use crate::Digest128;
use crate::cache::FileToken;
use crate::segment::open_files::OpenFiles;

use super::manifest::Workspace;
use super::memory::{Buffer, Memory};
use super::{Error, RecordRef, corrupt};

const HEADER: usize = 64;
const MAGIC: &[u8; 8] = b"AFTSPIL1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RunMeta {
    pub(super) id: u64,
    pub(super) level: u64,
    pub(super) block_bytes: u64,
    pub(super) payload: u64,
    pub(super) records: u64,
    pub(super) max_record: u64,
}

impl RunMeta {
    pub(super) fn path(&self, directory: &Path) -> PathBuf {
        directory.join(format!("run-{:020}.bin", self.id))
    }

    pub(super) fn file_bytes(&self) -> Result<u64, Error> {
        if self.block_bytes == 0 {
            return Err(Error::Config("zero run block size"));
        }
        self.payload
            .checked_add(
                self.payload
                    .div_ceil(self.block_bytes)
                    .checked_mul(16)
                    .ok_or(Error::Overflow)?,
            )
            .and_then(|n| n.checked_add(HEADER as u64))
            .ok_or(Error::Overflow)
    }

    pub(super) fn encode(self) -> [u8; 48] {
        let mut bytes = [0; 48];
        for (chunk, field) in bytes.chunks_exact_mut(8).zip([
            self.id,
            self.level,
            self.block_bytes,
            self.payload,
            self.records,
            self.max_record,
        ]) {
            chunk.copy_from_slice(&field.to_le_bytes());
        }
        bytes
    }

    pub(super) fn decode(bytes: &[u8; 48]) -> Self {
        Self {
            id: u64_at(bytes, 0),
            level: u64_at(bytes, 8),
            block_bytes: u64_at(bytes, 16),
            payload: u64_at(bytes, 24),
            records: u64_at(bytes, 32),
            max_record: u64_at(bytes, 40),
        }
    }

    fn header(self) -> [u8; HEADER] {
        let mut header = [0; HEADER];
        header[..8].copy_from_slice(MAGIC);
        header[8..16].copy_from_slice(&self.block_bytes.to_le_bytes());
        header[16..24].copy_from_slice(&self.payload.to_le_bytes());
        header[24..32].copy_from_slice(&self.records.to_le_bytes());
        header[32..40].copy_from_slice(&self.max_record.to_le_bytes());
        header[40..48].copy_from_slice(&self.id.to_le_bytes());
        let digest = Digest128::of(&header[..48]);
        header[48..].copy_from_slice(digest.as_bytes());
        header
    }
}

fn u64_at(bytes: &[u8], start: usize) -> u64 {
    u64::from_le_bytes(bytes[start..start + 8].try_into().expect("fixed field"))
}

fn block_digest(index: u64, bytes: &[u8]) -> Digest128 {
    let mut hash = blake3::Hasher::new();
    hash.update(&index.to_le_bytes());
    hash.update(bytes);
    Digest128::from_hash(&hash.finalize())
}

fn read_exact(file: &File, bytes: &mut [u8], offset: u64, path: &Path) -> Result<(), Error> {
    crate::fs::read_exact_at(file, bytes, offset).map_err(|e| {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            corrupt(path, "truncated run")
        } else {
            Error::Io(e)
        }
    })
}

#[derive(Debug)]
pub(super) struct Writer {
    file: Option<File>,
    partial: PathBuf,
    final_path: PathBuf,
    meta: RunMeta,
    buffer: Buffer<u8>,
    block: u64,
    memory: Memory,
    committed: bool,
    written: u64,
}

impl Writer {
    pub(super) fn new(
        workspace: &mut Workspace,
        block_bytes: usize,
        memory: &Memory,
    ) -> Result<Self, Error> {
        let buffer = Buffer::new(block_bytes, memory)?;
        let id = workspace.next_id;
        workspace.next_id = id.checked_add(1).ok_or(Error::Overflow)?;
        let meta = RunMeta {
            id,
            level: 0,
            block_bytes: block_bytes as u64,
            payload: 0,
            records: 0,
            max_record: 0,
        };
        let final_path = meta.path(&workspace.path);
        let partial = final_path.with_extension("part");
        let file = crate::fs::create_new_private(&partial)?;
        let mut this = Self {
            file: Some(file),
            partial,
            final_path,
            meta,
            buffer,
            block: 0,
            memory: memory.clone(),
            committed: false,
            written: 0,
        };
        this.put_file(&[0; HEADER])?;
        Ok(this)
    }

    fn put_file(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.file.as_mut().expect("writer file").write_all(bytes)?;
        self.written += bytes.len() as u64;
        disk_add(&self.memory, bytes.len() as u64);
        Ok(())
    }

    fn flush_block(&mut self) -> Result<(), Error> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let digest = block_digest(self.block, &self.buffer);
        self.put_file(digest.as_bytes())?;
        self.file
            .as_mut()
            .expect("writer file")
            .write_all(&self.buffer)?;
        let len = self.buffer.len() as u64;
        self.written += len;
        disk_add(&self.memory, len);
        self.buffer.clear();
        self.block += 1;
        Ok(())
    }

    fn write(&mut self, mut bytes: &[u8]) -> Result<(), Error> {
        self.meta.payload = self
            .meta
            .payload
            .checked_add(bytes.len() as u64)
            .ok_or(Error::Overflow)?;
        while !bytes.is_empty() {
            let take = bytes.len().min(self.buffer.capacity() - self.buffer.len());
            self.buffer.extend(&bytes[..take]);
            bytes = &bytes[take..];
            if self.buffer.len() == self.buffer.capacity() {
                self.flush_block()?;
            }
        }
        Ok(())
    }

    pub(super) fn record(&mut self, key: &[u8], value: &[u8], sequence: u64) -> Result<(), Error> {
        let mut frame = [0; 24];
        frame[..8].copy_from_slice(&(key.len() as u64).to_le_bytes());
        frame[8..16].copy_from_slice(&(value.len() as u64).to_le_bytes());
        frame[16..].copy_from_slice(&sequence.to_le_bytes());
        self.write(&frame)?;
        self.write(key)?;
        self.write(value)?;
        self.meta.records = self.meta.records.checked_add(1).ok_or(Error::Overflow)?;
        self.meta.max_record = self.meta.max_record.max((key.len() + value.len()) as u64);
        Ok(())
    }

    pub(super) fn finish(mut self, level: u64) -> Result<RunMeta, Error> {
        self.flush_block()?;
        self.meta.level = level;
        let file = self.file.as_mut().expect("writer file");
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&self.meta.header())?;
        self.memory.update(|s| s.bytes_written += HEADER as u64);
        crate::sync::sync_all(file, &self.partial)?;
        self.file = None;
        crate::fs::rename_new_durable(&self.partial, &self.final_path)?;
        self.committed = true;
        self.memory.update(|s| s.runs_written += 1);
        Ok(self.meta)
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.file = None;
        if !self.committed {
            let _ = std::fs::remove_file(&self.partial);
            let _ = std::fs::remove_file(&self.final_path);
            self.memory
                .update(|s| s.disk_bytes = s.disk_bytes.saturating_sub(self.written));
        }
    }
}

pub(super) fn disk_add(memory: &Memory, bytes: u64) {
    memory.update(|s| {
        s.disk_bytes += bytes;
        s.bytes_written += bytes;
        s.peak_disk_bytes = s.peak_disk_bytes.max(s.disk_bytes);
    });
}

#[derive(Debug)]
struct Reader {
    path: PathBuf,
    token: FileToken,
    meta: RunMeta,
    buffer: Buffer<u8>,
    head: Buffer<u8>,
    key_len: usize,
    sequence: u64,
    position: u64,
    loaded_block: Option<u64>,
    remaining: u64,
}

impl Reader {
    fn new(
        directory: &Path,
        meta: RunMeta,
        buffer_bytes: usize,
        memory: &Memory,
    ) -> Result<Self, Error> {
        let path = meta.path(directory);
        // Check configured bounds before trusting lengths from disk. The
        // manifest is checksummed too, but it is not an allocation authority.
        if meta.block_bytes == 0
            || meta.block_bytes > buffer_bytes as u64
            || meta.records == 0
            || meta
                .records
                .checked_mul(24)
                .is_none_or(|n| n > meta.payload)
        {
            return Err(corrupt(&path, "invalid run lengths"));
        }
        let buffer = Buffer::new(buffer_bytes, memory)?;
        let head = Buffer::new(
            usize::try_from(meta.max_record).map_err(|_| Error::Overflow)?,
            memory,
        )?;
        let this = Self {
            path,
            token: FileToken::next(),
            meta,
            buffer,
            head,
            key_len: 0,
            sequence: 0,
            position: 0,
            loaded_block: None,
            remaining: meta.records,
        };
        let file = this.open()?;
        OpenFiles::process().insert(this.token, file);
        Ok(this)
    }

    fn open(&self) -> Result<File, Error> {
        let file = crate::fs::open_read(&self.path)?;
        let mut header = [0; HEADER];
        read_exact(&file, &mut header, 0, &self.path)?;
        if header != self.meta.header() || file.metadata()?.len() != self.meta.file_bytes()? {
            return Err(corrupt(&self.path, "run header or length mismatch"));
        }
        Ok(file)
    }

    fn load_block(&mut self) -> Result<(), Error> {
        let index = self.position / self.meta.block_bytes;
        if self.loaded_block == Some(index) {
            return Ok(());
        }
        let offset = HEADER as u64 + index * (self.meta.block_bytes + 16);
        let len = usize::try_from(
            (self.meta.payload - index * self.meta.block_bytes).min(self.meta.block_bytes),
        )
        .map_err(|_| Error::Overflow)?;
        self.buffer.clear();
        // Fill the already allocated buffer, without a second read allocation.
        self.buffer.resize_zeroed(len);
        let file = OpenFiles::process().get(self.token, || self.open())?;
        let mut expected = [0; 16];
        read_exact(&file, &mut expected, offset, &self.path)?;
        read_exact(&file, &mut self.buffer, offset + 16, &self.path)?;
        if block_digest(index, &self.buffer).as_bytes() != &expected {
            return Err(corrupt(&self.path, "block checksum mismatch"));
        }
        self.loaded_block = Some(index);
        Ok(())
    }

    fn read_frame(&mut self, out: &mut [u8]) -> Result<(), Error> {
        if out.len() as u64 > self.meta.payload - self.position {
            return Err(corrupt(&self.path, "truncated record frame"));
        }
        let mut filled = 0;
        while filled < out.len() {
            self.load_block()?;
            let within = (self.position % self.meta.block_bytes) as usize;
            let count = (out.len() - filled).min(self.buffer.len() - within);
            out[filled..filled + count].copy_from_slice(&self.buffer[within..within + count]);
            filled += count;
            self.position += count as u64;
        }
        Ok(())
    }

    fn advance(&mut self) -> Result<bool, Error> {
        if self.remaining == 0 {
            return Ok(false);
        }
        let mut frame = [0; 24];
        self.read_frame(&mut frame)?;
        let key_len = u64_at(&frame, 0);
        let value_len = u64_at(&frame, 8);
        let len = key_len
            .checked_add(value_len)
            .filter(|n| *n <= self.meta.max_record)
            .ok_or_else(|| corrupt(&self.path, "record exceeds run head bound"))?;
        if len > self.meta.payload - self.position {
            return Err(corrupt(&self.path, "truncated record"));
        }
        self.key_len = usize::try_from(key_len).map_err(|_| Error::Overflow)?;
        self.sequence = u64_at(&frame, 16);
        self.head.clear();
        // Copy into a fixed-capacity head; a record may span many blocks.
        let mut left = usize::try_from(len).map_err(|_| Error::Overflow)?;
        while left > 0 {
            self.load_block()?;
            let within = (self.position % self.meta.block_bytes) as usize;
            let count = left.min(self.buffer.len() - within);
            self.head.extend(&self.buffer[within..within + count]);
            left -= count;
            self.position += count as u64;
        }
        self.remaining -= 1;
        if self.remaining == 0 && self.position != self.meta.payload {
            return Err(corrupt(&self.path, "unframed trailing data"));
        }
        Ok(true)
    }

    fn current(&self) -> RecordRef<'_> {
        RecordRef {
            key: &self.head[..self.key_len],
            value: &self.head[self.key_len..],
            sequence: self.sequence,
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        OpenFiles::process().forget(self.token);
    }
}

#[derive(Debug)]
pub(super) struct Merge {
    readers: Buffer<Reader>,
    heap: Buffer<usize>,
    pending: Option<usize>,
}

impl Merge {
    pub(super) fn reader_cost(meta: RunMeta, block_bytes: usize) -> Result<usize, Error> {
        size_of::<Reader>()
            .checked_add(size_of::<usize>())
            .and_then(|n| n.checked_add(block_bytes))
            .and_then(|n| n.checked_add(usize::try_from(meta.max_record).ok()?))
            .ok_or(Error::Overflow)
    }

    pub(super) fn cost(
        inputs: &[RunMeta],
        block_bytes: usize,
        output: bool,
    ) -> Result<usize, Error> {
        let mut cost = 0usize;
        for meta in inputs {
            cost = cost
                .checked_add(Self::reader_cost(*meta, block_bytes)?)
                .ok_or(Error::Overflow)?;
        }
        if output {
            cost = cost
                .checked_add(
                    usize::try_from(inputs.iter().map(|r| r.max_record).max().unwrap_or(0))
                        .map_err(|_| Error::Overflow)?,
                )
                .ok_or(Error::Overflow)?;
        }
        Ok(cost)
    }

    pub(super) fn new(
        directory: &Path,
        inputs: &[RunMeta],
        block_bytes: usize,
        memory: &Memory,
    ) -> Result<Self, Error> {
        let cost = Self::cost(inputs, block_bytes, false)?;
        if cost > memory.available() {
            return Err(Error::Budget {
                needed: memory.stats().buffered_bytes.saturating_add(cost),
                budget: memory.stats().budget_bytes,
            });
        }
        let mut this = Self {
            readers: Buffer::new(inputs.len(), memory)?,
            heap: Buffer::new(inputs.len(), memory)?,
            pending: None,
        };
        for meta in inputs {
            let mut reader = Reader::new(directory, *meta, block_bytes, memory)?;
            let valid = reader.advance()?;
            let index = this.readers.len();
            this.readers.push(reader);
            if valid {
                this.insert(index);
            }
        }
        let io_bytes = this
            .readers
            .iter()
            .map(|reader| reader.buffer.capacity())
            .sum();
        let head_bytes = this
            .readers
            .iter()
            .map(|reader| reader.head.capacity())
            .sum();
        memory.update(|s| {
            s.peak_merge_readers = s.peak_merge_readers.max(this.readers.len());
            s.peak_merge_io_bytes = s.peak_merge_io_bytes.max(io_bytes);
            s.peak_merge_head_bytes = s.peak_merge_head_bytes.max(head_bytes);
        });
        Ok(this)
    }

    fn less(&self, a: usize, b: usize) -> bool {
        let a = self.readers[a].current();
        let b = self.readers[b].current();
        a.key.cmp(b.key).then(a.sequence.cmp(&b.sequence)).is_lt()
    }

    fn insert(&mut self, index: usize) {
        self.heap.push(index);
        let mut slot = self.heap.len() - 1;
        while slot > 0 {
            let parent = (slot - 1) / 2;
            if !self.less(self.heap[slot], self.heap[parent]) {
                break;
            }
            self.heap.swap(slot, parent);
            slot = parent;
        }
    }

    fn pop(&mut self) -> Option<usize> {
        let result = *self.heap.first()?;
        let last = self.heap.pop().expect("heap");
        if !self.heap.is_empty() {
            self.heap[0] = last;
            let mut slot = 0;
            loop {
                let mut child = slot * 2 + 1;
                if child >= self.heap.len() {
                    break;
                }
                if child + 1 < self.heap.len() && self.less(self.heap[child + 1], self.heap[child])
                {
                    child += 1;
                }
                if !self.less(self.heap[child], self.heap[slot]) {
                    break;
                }
                self.heap.swap(child, slot);
                slot = child;
            }
        }
        Some(result)
    }

    pub(super) fn advance(&mut self) -> Result<bool, Error> {
        if let Some(previous) = self.pending.take()
            && self.readers[previous].advance()?
        {
            self.insert(previous);
        }
        self.pending = self.pop();
        Ok(self.pending.is_some())
    }

    pub(super) fn current(&self) -> RecordRef<'_> {
        self.readers[self.pending.expect("current record")].current()
    }

    pub(super) fn next_record(&mut self) -> Result<Option<RecordRef<'_>>, Error> {
        if self.advance()? {
            Ok(Some(self.current()))
        } else {
            Ok(None)
        }
    }
}
