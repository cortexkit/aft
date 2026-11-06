//! Opening, verifying and reading a segment file.

use std::collections::HashSet;
use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::error::{ErrorKind, Region, SegmentError};
use super::format::{
    Alignment, BLOCK_LEN, HEADER_LEN, Header, MAX_SECTIONS, MAX_SEGMENT_LEN, ROW_LEN, Requirement,
    Row, SectionId, SectionKind,
};
use crate::cache::{BlockCache, BlockKey, FileToken};
use crate::ids::{Digest128, FamilyId, SegmentId, WriterIncarnation};

/// A section kind and version the caller can read.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct KnownSection {
    pub kind: SectionKind,
    pub version: u16,
}

/// What a reader can see about a section without reading it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SectionInfo {
    pub id: SectionId,
    pub version: u16,
    pub requirement: Requirement,
    pub alignment: Alignment,
    /// Length of the section's bytes.
    pub len: u64,
    /// The plane's count of what the section holds, as written.
    pub item_count: u64,
}

/// A section of one open [`Segment`]. Using it with another segment is a bug
/// and panics.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SectionHandle {
    file: FileToken,
    index: u32,
}

/// An open, verified segment. Its bytes are read through the shared
/// [`BlockCache`]; dropping the segment releases its cached blocks.
#[derive(Debug)]
pub struct Segment {
    path: PathBuf,
    file: File,
    token: FileToken,
    cache: Arc<BlockCache>,
    header: Header,
    sections: Vec<OpenSection>,
}

#[derive(Debug)]
struct OpenSection {
    info: SectionInfo,
    offset: u64,
    digest: Digest128,
    block_digests: Box<[Digest128]>,
}

impl Segment {
    /// Opens `path` and checks everything except section bytes: the header,
    /// the section table, the checksum table, that every section lies inside
    /// the file without overlapping another, and that the segment belongs to
    /// `family`. Sections not in `known` are skipped when they are optional;
    /// an unknown required section makes the segment unreadable.
    ///
    /// # Errors
    ///
    /// When the file cannot be read or fails any of those checks.
    pub fn open(
        path: &Path,
        family: FamilyId,
        known: &[KnownSection],
        cache: Arc<BlockCache>,
    ) -> Result<Self, SegmentError> {
        let fail = |kind| SegmentError::new(path, kind);
        let file = crate::fs::open_read(path).map_err(|error| fail(error.into()))?;
        let file_len = file.metadata().map_err(|error| fail(error.into()))?.len();
        if file_len < HEADER_LEN as u64 {
            return Err(fail(ErrorKind::NotASegment));
        }
        if file_len > MAX_SEGMENT_LEN {
            return Err(fail(ErrorKind::Malformed(
                "file is larger than any segment",
            )));
        }
        let mut header_bytes = [0; HEADER_LEN];
        crate::fs::read_exact_at(&file, &mut header_bytes, 0)
            .map_err(|error| fail(error.into()))?;
        let header = Header::decode(&header_bytes).map_err(fail)?;
        if header.family != family {
            return Err(fail(ErrorKind::FamilyMismatch {
                found: header.family,
                expected: family,
            }));
        }
        if header.section_count > MAX_SECTIONS {
            return Err(fail(ErrorKind::Malformed("section count is out of range")));
        }
        let table_len = u64::from(header.section_count) * ROW_LEN as u64;
        let regions_fit = header.checksums_offset >= HEADER_LEN as u64
            && header.checksums_offset <= header.table_offset
            && (header.table_offset - header.checksums_offset)
                .is_multiple_of(Digest128::LEN as u64)
            && header.table_offset.checked_add(table_len) == Some(file_len);
        if !regions_fit {
            return Err(fail(ErrorKind::Malformed(
                "the file length does not match its checksum and section tables",
            )));
        }

        let table = read_region(&file, header.table_offset, table_len).map_err(fail)?;
        if Digest128::of(&table) != header.table_digest {
            return Err(fail(ErrorKind::Corrupt(Region::SectionTable)));
        }
        let checksums_len = header.table_offset - header.checksums_offset;
        let checksums = read_region(&file, header.checksums_offset, checksums_len).map_err(fail)?;
        if Digest128::of(&checksums) != header.checksums_digest {
            return Err(fail(ErrorKind::Corrupt(Region::ChecksumTable)));
        }

        let mut rows = Vec::with_capacity(table.len() / ROW_LEN);
        for chunk in table.as_chunks::<ROW_LEN>().0 {
            rows.push(Row::decode(chunk).map_err(fail)?);
        }
        check_layout(&rows, &header).map_err(fail)?;

        let known: HashSet<KnownSection> = known.iter().copied().collect();
        let token = FileToken::next();
        let mut sections = Vec::new();
        for row in rows {
            let understood = known.contains(&KnownSection {
                kind: row.id.kind,
                version: row.version,
            });
            if !understood {
                if row.requirement == Requirement::Required {
                    return Err(fail(ErrorKind::UnknownRequiredSection {
                        id: row.id,
                        version: row.version,
                    }));
                }
                continue;
            }
            let first = usize::try_from(
                (row.checksums_offset - header.checksums_offset) / Digest128::LEN as u64,
            )
            .map_err(|_| fail(ErrorKind::Malformed("checksum offset is out of range")))?;
            let count = usize::try_from(row.block_count())
                .map_err(|_| fail(ErrorKind::Malformed("block count is out of range")))?;
            let block_digests = checksums[first * Digest128::LEN..(first + count) * Digest128::LEN]
                .as_chunks::<{ Digest128::LEN }>()
                .0
                .iter()
                .map(|chunk| Digest128::from_bytes(*chunk))
                .collect();
            sections.push(OpenSection {
                info: SectionInfo {
                    id: row.id,
                    version: row.version,
                    requirement: row.requirement,
                    alignment: row.alignment,
                    len: row.len,
                    item_count: row.item_count,
                },
                offset: row.offset,
                digest: row.digest,
                block_digests,
            });
        }
        Ok(Self {
            path: path.to_path_buf(),
            file,
            token,
            cache,
            header,
            sections,
        })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn family(&self) -> FamilyId {
        self.header.family
    }

    #[must_use]
    pub fn id(&self) -> SegmentId {
        self.header.segment
    }

    #[must_use]
    pub fn incarnation(&self) -> WriterIncarnation {
        self.header.incarnation
    }

    #[must_use]
    pub fn created_unix_ms(&self) -> u64 {
        self.header.created_unix_ms
    }

    /// The handle of a section this reader understood, if the segment has it.
    #[must_use]
    pub fn section(&self, id: SectionId) -> Option<SectionHandle> {
        let index = self
            .sections
            .iter()
            .position(|section| section.info.id == id)?;
        Some(SectionHandle {
            file: self.token,
            index: u32::try_from(index).expect("section count fits in u32"),
        })
    }

    /// The sections this reader understood, in file order.
    pub fn sections(&self) -> impl Iterator<Item = (SectionHandle, SectionInfo)> + '_ {
        self.sections.iter().enumerate().map(|(index, section)| {
            (
                SectionHandle {
                    file: self.token,
                    index: u32::try_from(index).expect("section count fits in u32"),
                },
                section.info,
            )
        })
    }

    #[must_use]
    pub fn info(&self, handle: SectionHandle) -> SectionInfo {
        self.open_section(handle).info
    }

    /// Fills `buf` with the section's bytes starting at `offset`, verifying
    /// every block the range touches.
    ///
    /// # Errors
    ///
    /// When the range runs past the section's end, a read fails, or a block
    /// fails its checksum.
    ///
    /// # Panics
    ///
    /// When `handle` belongs to another segment.
    pub fn read(
        &self,
        handle: SectionHandle,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<(), SegmentError> {
        let section = self.open_section(handle);
        let end = offset
            .checked_add(buf.len() as u64)
            .filter(|end| *end <= section.info.len)
            .ok_or_else(|| {
                self.error(ErrorKind::OutOfRange {
                    id: section.info.id,
                    end: offset.saturating_add(buf.len() as u64),
                    len: section.info.len,
                })
            })?;
        let block_len = BLOCK_LEN as u64;
        let mut position = offset;
        let mut filled = 0;
        while position < end {
            let block_index = position / block_len;
            let block = self.block(handle, section, block_index)?;
            let within = usize::try_from(position - block_index * block_len)
                .expect("an offset within a block fits in usize");
            let take = (block.len() - within).min(buf.len() - filled);
            buf[filled..filled + take].copy_from_slice(&block[within..within + take]);
            filled += take;
            position += take as u64;
        }
        Ok(())
    }

    /// The section's bytes in `range`, as a new buffer.
    ///
    /// # Errors
    ///
    /// As [`read`](Self::read).
    ///
    /// A range read from stored offsets is untrusted, so a backwards range,
    /// or one past the end of its section, is refused before anything is
    /// allocated.
    ///
    /// # Panics
    ///
    /// When `handle` belongs to another segment.
    pub fn read_range(
        &self,
        handle: SectionHandle,
        range: Range<u64>,
    ) -> Result<Vec<u8>, SegmentError> {
        if range.start > range.end {
            return Err(self.error(ErrorKind::Malformed("a stored range runs backwards")));
        }
        let info = self.info(handle);
        if range.end > info.len {
            return Err(self.error(ErrorKind::OutOfRange {
                id: info.id,
                end: range.end,
                len: info.len,
            }));
        }
        let len = usize::try_from(range.end - range.start)
            .map_err(|_| self.error(ErrorKind::Malformed("range is larger than memory")))?;
        let mut buf = vec![0; len];
        self.read(handle, range.start, &mut buf)?;
        Ok(buf)
    }

    /// Reads every section in full, bypassing the cache, and checks every
    /// block digest and every section digest. Used before merging a segment
    /// and by health checks.
    ///
    /// # Errors
    ///
    /// On the first failed read or checksum.
    pub fn verify(&self) -> Result<(), SegmentError> {
        let mut buf = vec![0; BLOCK_LEN];
        for section in &self.sections {
            let mut whole = blake3::Hasher::new();
            for (block_index, expected) in section.block_digests.iter().enumerate() {
                let bytes = self.read_block_uncached(section, block_index as u64, &mut buf)?;
                if Digest128::of(bytes) != *expected {
                    return Err(self.error(ErrorKind::Corrupt(Region::Block {
                        section: section.info.id,
                        block: block_index as u64,
                    })));
                }
                whole.update(bytes);
            }
            if Digest128::from_hash(&whole.finalize()) != section.digest {
                return Err(self.error(ErrorKind::Corrupt(Region::Section(section.info.id))));
            }
        }
        Ok(())
    }

    fn open_section(&self, handle: SectionHandle) -> &OpenSection {
        assert_eq!(
            handle.file,
            self.token,
            "section handle used with a segment it does not belong to ({})",
            self.path.display()
        );
        &self.sections[handle.index as usize]
    }

    fn block(
        &self,
        handle: SectionHandle,
        section: &OpenSection,
        block_index: u64,
    ) -> Result<Arc<[u8]>, SegmentError> {
        let key = BlockKey {
            file: self.token,
            section: handle.index,
            block: block_index,
        };
        self.cache.get_or_load(key, || {
            let mut buf = vec![0; BLOCK_LEN];
            let bytes = self.read_block_uncached(section, block_index, &mut buf)?;
            let index = usize::try_from(block_index).expect("block index fits in usize");
            if Digest128::of(bytes) != section.block_digests[index] {
                return Err(self.error(ErrorKind::Corrupt(Region::Block {
                    section: section.info.id,
                    block: block_index,
                })));
            }
            Ok(Arc::from(bytes))
        })
    }

    fn read_block_uncached<'b>(
        &self,
        section: &OpenSection,
        block_index: u64,
        buf: &'b mut [u8],
    ) -> Result<&'b [u8], SegmentError> {
        let block_len = BLOCK_LEN as u64;
        let start = block_index * block_len;
        let len = usize::try_from(block_len.min(section.info.len - start))
            .expect("a block length fits in usize");
        let bytes = &mut buf[..len];
        crate::fs::read_exact_at(&self.file, bytes, section.offset + start)
            .map_err(|error| self.error(error.into()))?;
        Ok(bytes)
    }

    fn error(&self, kind: ErrorKind) -> SegmentError {
        SegmentError::new(&self.path, kind)
    }
}

impl Drop for Segment {
    fn drop(&mut self) {
        self.cache.forget(self.token);
    }
}

fn read_region(file: &File, offset: u64, len: u64) -> Result<Vec<u8>, ErrorKind> {
    let len = usize::try_from(len).map_err(|_| ErrorKind::Malformed("region is too large"))?;
    let mut buf = vec![0; len];
    crate::fs::read_exact_at(file, &mut buf, offset)?;
    Ok(buf)
}

/// Checks that every section lies between the header and the checksum region,
/// starts on its alignment, owns a slice of the checksum region of the right
/// size, appears once, and overlaps no other section.
fn check_layout(rows: &[Row], header: &Header) -> Result<(), ErrorKind> {
    let mut seen = HashSet::new();
    let mut spans = Vec::with_capacity(rows.len());
    for row in rows {
        if !seen.insert(row.id) {
            return Err(ErrorKind::Malformed("a section appears twice"));
        }
        let fits = row.offset >= HEADER_LEN as u64
            && row.offset % row.alignment.bytes() == 0
            && row
                .offset
                .checked_add(row.len)
                .is_some_and(|end| end <= header.checksums_offset);
        if !fits {
            return Err(ErrorKind::Malformed(
                "a section lies outside the data region",
            ));
        }
        let checksums_end = row
            .block_count()
            .checked_mul(Digest128::LEN as u64)
            .and_then(|len| row.checksums_offset.checked_add(len));
        let checksums_fit = row.checksums_offset >= header.checksums_offset
            && (row.checksums_offset - header.checksums_offset)
                .is_multiple_of(Digest128::LEN as u64)
            && checksums_end.is_some_and(|end| end <= header.table_offset);
        if !checksums_fit {
            return Err(ErrorKind::Malformed(
                "a section's checksums lie outside the checksum table",
            ));
        }
        spans.push((row.offset, row.offset + row.len));
    }
    spans.sort_unstable();
    if spans.windows(2).any(|pair| pair[1].0 < pair[0].1) {
        return Err(ErrorKind::Malformed("sections overlap"));
    }
    Ok(())
}

crate::heap_bytes_none!(SectionInfo, SectionHandle);
crate::heap_bytes_of_fields!(OpenSection {
    info,
    offset,
    digest,
    block_digests,
});

/// What an open segment holds in memory: its path and its section table
/// with each section's block checksums. The file handle owns no heap, and
/// the block cache its reads go through is shared by every segment, so it
/// is counted once for the process, never here.
impl crate::heap::HeapBytes for Segment {
    fn heap_bytes(&self, seen: &mut crate::heap::Seen) -> usize {
        let Self {
            path,
            file: _,
            token: _,
            cache: _,
            header: _,
            sections,
        } = self;
        crate::heap::HeapBytes::heap_bytes(path, seen)
            + crate::heap::HeapBytes::heap_bytes(sections, seen)
    }
}
