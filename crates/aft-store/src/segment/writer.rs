//! Writing a segment file from start to finish.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::error::{ErrorKind, SegmentError};
use super::format::{
    Alignment, BLOCK_LEN, HEADER_LEN, Header, MAX_SECTIONS, MAX_SEGMENT_LEN, ROW_LEN, Requirement,
    Row, SectionId,
};
use crate::ids::{Digest128, FamilyId, SegmentId, WriterIncarnation};

/// Identity fields recorded in the header of a new segment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SegmentMeta {
    pub family: FamilyId,
    pub segment: SegmentId,
    pub incarnation: WriterIncarnation,
    pub created_unix_ms: u64,
}

/// How a section is stored and how readers treat it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SectionSpec {
    pub id: SectionId,
    /// The plane's version of this section kind's layout.
    pub version: u16,
    pub alignment: Alignment,
    pub requirement: Requirement,
}

/// What a finished segment contains.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SegmentSummary {
    pub path: PathBuf,
    pub len: u64,
    pub sections: usize,
}

/// Writes one new segment file: sections one after another, then the
/// checksums, the section table and the header.
///
/// The file is created new and owner-only, and is not valid until
/// [`finish`](Self::finish) returns: until then its header is zeros, so a
/// reader that finds a torn file refuses it. Making a finished segment
/// visible to readers is the caller's step.
#[derive(Debug)]
pub struct SegmentWriter {
    path: PathBuf,
    out: BufWriter<File>,
    meta: SegmentMeta,
    position: u64,
    rows: Vec<PendingRow>,
    seen: HashSet<SectionId>,
    block_digests: Vec<Digest128>,
    open: Option<OpenSection>,
}

#[derive(Debug)]
struct PendingRow {
    spec: SectionSpec,
    offset: u64,
    len: u64,
    item_count: u64,
    first_block: usize,
    digest: Digest128,
}

#[derive(Debug)]
struct OpenSection {
    spec: SectionSpec,
    offset: u64,
    len: u64,
    whole: blake3::Hasher,
    block: blake3::Hasher,
    block_fill: usize,
    first_block: usize,
}

impl SegmentWriter {
    /// Creates `path`, which must not exist.
    ///
    /// # Errors
    ///
    /// When the file exists or cannot be created.
    pub fn create(path: &Path, meta: SegmentMeta) -> Result<Self, SegmentError> {
        let file = crate::fs::create_new_private(path).map_err(|error| io(path, error))?;
        let mut writer = Self {
            path: path.to_path_buf(),
            out: BufWriter::with_capacity(BLOCK_LEN, file),
            meta,
            position: 0,
            rows: Vec::new(),
            seen: HashSet::new(),
            block_digests: Vec::new(),
            open: None,
        };
        // The real header is written last, once its offsets and digests are
        // known; zeros until then mark the file as unfinished.
        writer.put(&[0; HEADER_LEN])?;
        Ok(writer)
    }

    /// Starts a section. Its bytes follow through [`write`](Self::write) until
    /// [`end_section`](Self::end_section).
    ///
    /// # Errors
    ///
    /// When another section is still open, the section id was already
    /// written, or the table would hold too many sections.
    pub fn begin_section(&mut self, spec: SectionSpec) -> Result<(), SegmentError> {
        if let Some(open) = &self.open {
            return Err(self.error(ErrorKind::UnfinishedSection(open.spec.id)));
        }
        if !self.seen.insert(spec.id) {
            return Err(self.error(ErrorKind::DuplicateSection(spec.id)));
        }
        if self.rows.len() >= MAX_SECTIONS as usize {
            return Err(self.error(ErrorKind::Malformed("too many sections")));
        }
        self.pad_to(spec.alignment.bytes())?;
        self.open = Some(OpenSection {
            spec,
            offset: self.position,
            len: 0,
            whole: blake3::Hasher::new(),
            block: blake3::Hasher::new(),
            block_fill: 0,
            first_block: self.block_digests.len(),
        });
        Ok(())
    }

    /// Appends bytes to the open section.
    ///
    /// # Errors
    ///
    /// When no section is open, the segment would exceed
    /// [`MAX_SEGMENT_LEN`](super::MAX_SEGMENT_LEN), or the write fails.
    pub fn write(&mut self, mut bytes: &[u8]) -> Result<(), SegmentError> {
        let Some(mut open) = self.open.take() else {
            return Err(self.error(ErrorKind::Malformed("bytes written outside a section")));
        };
        open.whole.update(bytes);
        open.len += bytes.len() as u64;
        while !bytes.is_empty() {
            let take = bytes.len().min(BLOCK_LEN - open.block_fill);
            open.block.update(&bytes[..take]);
            open.block_fill += take;
            if open.block_fill == BLOCK_LEN {
                self.block_digests
                    .push(Digest128::from_hash(&open.block.finalize()));
                open.block.reset();
                open.block_fill = 0;
            }
            let (now, rest) = bytes.split_at(take);
            self.put(now)?;
            bytes = rest;
        }
        self.open = Some(open);
        Ok(())
    }

    /// Ends the open section. `item_count` is what the section holds in the
    /// plane's own unit, such as keys or postings; readers can see it without
    /// reading the section.
    ///
    /// # Errors
    ///
    /// When no section is open.
    pub fn end_section(&mut self, item_count: u64) -> Result<(), SegmentError> {
        let Some(open) = self.open.take() else {
            return Err(self.error(ErrorKind::Malformed("no section is open")));
        };
        if open.block_fill > 0 {
            self.block_digests
                .push(Digest128::from_hash(&open.block.finalize()));
        }
        self.rows.push(PendingRow {
            spec: open.spec,
            offset: open.offset,
            len: open.len,
            item_count,
            first_block: open.first_block,
            digest: Digest128::from_hash(&open.whole.finalize()),
        });
        Ok(())
    }

    /// Writes a whole section in one call.
    ///
    /// # Errors
    ///
    /// As [`begin_section`](Self::begin_section) and [`write`](Self::write).
    pub fn write_section(
        &mut self,
        spec: SectionSpec,
        bytes: &[u8],
        item_count: u64,
    ) -> Result<(), SegmentError> {
        self.begin_section(spec)?;
        self.write(bytes)?;
        self.end_section(item_count)
    }

    /// Writes the checksums, the section table and the header, and flushes the
    /// file to stable storage.
    ///
    /// # Errors
    ///
    /// When a section is still open, the segment is too large, or a write or
    /// the flush fails.
    pub fn finish(mut self) -> Result<SegmentSummary, SegmentError> {
        if let Some(open) = &self.open {
            return Err(self.error(ErrorKind::UnfinishedSection(open.spec.id)));
        }
        self.pad_to(Alignment::Word.bytes())?;
        let checksums_offset = self.position;
        let mut checksums = Vec::with_capacity(self.block_digests.len() * Digest128::LEN);
        for digest in &self.block_digests {
            checksums.extend_from_slice(digest.as_bytes());
        }
        self.put(&checksums)?;
        let table_offset = self.position;
        let mut table = Vec::with_capacity(self.rows.len() * ROW_LEN);
        for pending in &self.rows {
            let row = Row {
                id: pending.spec.id,
                version: pending.spec.version,
                requirement: pending.spec.requirement,
                alignment: pending.spec.alignment,
                offset: pending.offset,
                len: pending.len,
                item_count: pending.item_count,
                checksums_offset: checksums_offset + (pending.first_block * Digest128::LEN) as u64,
                digest: pending.digest,
            };
            table.extend_from_slice(&row.encode());
        }
        self.put(&table)?;
        let header = Header {
            family: self.meta.family,
            segment: self.meta.segment,
            incarnation: self.meta.incarnation,
            created_unix_ms: self.meta.created_unix_ms,
            section_count: u32::try_from(self.rows.len())
                .map_err(|_| self.error(ErrorKind::Malformed("too many sections")))?,
            checksums_offset,
            table_offset,
            table_digest: Digest128::of(&table),
            checksums_digest: Digest128::of(&checksums),
        };
        let path = self.path.clone();
        let len = self.position;
        let sections = self.rows.len();
        let mut file = self
            .out
            .into_inner()
            .map_err(|error| io(&path, error.into_error()))?;
        file.seek(SeekFrom::Start(0))
            .and_then(|_| file.write_all(&header.encode()))
            .and_then(|()| crate::sync::sync_all(&file, &path))
            .map_err(|error| io(&path, error))?;
        Ok(SegmentSummary {
            path,
            len,
            sections,
        })
    }

    fn pad_to(&mut self, alignment: u64) -> Result<(), SegmentError> {
        let padding = self.position.next_multiple_of(alignment) - self.position;
        const ZEROS: [u8; 64] = [0; 64];
        self.put(&ZEROS[..usize::try_from(padding).expect("padding is below 64 bytes")])
    }

    fn put(&mut self, bytes: &[u8]) -> Result<(), SegmentError> {
        let end = self.position + bytes.len() as u64;
        if end > MAX_SEGMENT_LEN {
            return Err(self.error(ErrorKind::TooLarge {
                limit: MAX_SEGMENT_LEN,
            }));
        }
        self.out
            .write_all(bytes)
            .map_err(|error| io(&self.path, error))?;
        self.position = end;
        Ok(())
    }

    fn error(&self, kind: ErrorKind) -> SegmentError {
        SegmentError::new(&self.path, kind)
    }
}

fn io(path: &Path, error: std::io::Error) -> SegmentError {
    SegmentError::new(path, ErrorKind::Io(error))
}
