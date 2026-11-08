//! Variable-length unit bytes, each stored compressed when that saves space.
//!
//! A plane keeps such bytes in two sections: the data, and an index with one
//! fixed-width entry per unit, by ordinal: data offset (u64), stored length
//! (u32), length when unpacked (u32), codec (u8). A reader finds a unit's
//! entry by its ordinal alone and reads only that unit's bytes.

use std::io::{Read, Write};

use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;

use crate::le::{u32_at, u64_at};
use crate::segment::{ErrorKind, SectionHandle, Segment, SegmentError, SegmentWriter};

/// The width of one index entry.
pub(crate) const INDEX_ENTRY_LEN: usize = 17;

/// The largest unit a reader unpacks. A stored length above it is malformed,
/// so a damaged index cannot make a reader allocate for it.
const MAX_UNPACKED: u32 = 256 << 20;

/// Bytes are stored compressed only when that saves at least this share.
const MIN_SAVING_PERCENT: usize = 10;

const RAW: u8 = 0;
const ZLIB: u8 = 1;

/// Collects units in ordinal order.
#[derive(Debug, Default)]
pub(crate) struct PackedBuilder {
    data: Vec<u8>,
    index: Vec<u8>,
}

impl PackedBuilder {
    pub(crate) fn with_capacity(units: usize) -> Self {
        Self {
            data: Vec::new(),
            index: Vec::with_capacity(units * INDEX_ENTRY_LEN),
        }
    }

    /// Adds the next unit's bytes.
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        let (codec, stored) = pack(bytes);
        self.index
            .extend_from_slice(&(self.data.len() as u64).to_le_bytes());
        self.index.extend_from_slice(
            &u32::try_from(stored.len())
                .expect("a unit is under 4 GiB")
                .to_le_bytes(),
        );
        self.index.extend_from_slice(
            &u32::try_from(bytes.len())
                .expect("a unit is under 4 GiB")
                .to_le_bytes(),
        );
        self.index.push(codec);
        self.data.extend_from_slice(&stored);
    }

    /// The data and index sections' bytes.
    pub(crate) fn finish(self) -> (Vec<u8>, Vec<u8>) {
        (self.data, self.index)
    }
}

/// One unit as its data section stores it: possibly compressed, with what
/// its index entry says about it.
#[derive(Debug)]
pub(crate) struct StoredUnit {
    codec: u8,
    unpacked: u32,
    bytes: Vec<u8>,
}

impl StoredUnit {
    /// A unit with no bytes, as [`PackedBuilder::push`] stores an empty one.
    pub(crate) const EMPTY: Self = Self {
        codec: RAW,
        unpacked: 0,
        bytes: Vec::new(),
    };
}

/// Copies units from other segments' data sections into the data section
/// open in a writer as they arrive, keeping only the index: a fold moves
/// stored units into a new segment without unpacking and packing them
/// again, and without holding them all.
#[derive(Debug, Default)]
pub(crate) struct PackedCopy {
    len: u64,
    index: Vec<u8>,
}

impl PackedCopy {
    pub(crate) fn with_capacity(units: usize) -> Self {
        Self {
            len: 0,
            index: Vec::with_capacity(units * INDEX_ENTRY_LEN),
        }
    }

    /// Writes the next unit into `writer`'s open section.
    ///
    /// # Errors
    ///
    /// When the write fails.
    pub(crate) fn push(
        &mut self,
        writer: &mut SegmentWriter,
        unit: &StoredUnit,
    ) -> Result<(), SegmentError> {
        self.index.extend_from_slice(&self.len.to_le_bytes());
        self.index.extend_from_slice(
            &u32::try_from(unit.bytes.len())
                .expect("a unit is under 4 GiB")
                .to_le_bytes(),
        );
        self.index.extend_from_slice(&unit.unpacked.to_le_bytes());
        self.index.push(unit.codec);
        writer.write(&unit.bytes)?;
        self.len += unit.bytes.len() as u64;
        Ok(())
    }

    /// The index section's bytes.
    pub(crate) fn finish(self) -> Vec<u8> {
        self.index
    }
}

fn pack(bytes: &[u8]) -> (u8, Vec<u8>) {
    if bytes.is_empty() {
        return (RAW, Vec::new());
    }
    let mut encoder = ZlibEncoder::new(Vec::with_capacity(bytes.len() / 3), Compression::fast());
    encoder
        .write_all(bytes)
        .expect("writing into a Vec cannot fail");
    let packed = encoder.finish().expect("writing into a Vec cannot fail");
    if packed.len() * 100 <= bytes.len() * (100 - MIN_SAVING_PERCENT) {
        (ZLIB, packed)
    } else {
        (RAW, bytes.to_vec())
    }
}

/// The unit at `ordinal`, unpacked.
///
/// # Errors
///
/// When a read fails, a block fails its checksum, or the stored bytes do not
/// unpack to their recorded length.
pub(crate) fn read(
    segment: &Segment,
    data: SectionHandle,
    index: SectionHandle,
    ordinal: u32,
) -> Result<Vec<u8>, SegmentError> {
    let malformed = |reason| SegmentError::new(segment.path(), ErrorKind::Malformed(reason));
    let StoredUnit {
        codec,
        unpacked,
        bytes,
    } = read_stored(segment, data, index, ordinal)?;
    match codec {
        RAW => Ok(bytes),
        _ => {
            // Grown as bytes arrive rather than sized from the stored length.
            let mut out = Vec::with_capacity((unpacked as usize).min(1 << 20));
            ZlibDecoder::new(bytes.as_slice())
                .take(u64::from(unpacked) + 1)
                .read_to_end(&mut out)
                .map_err(|_| malformed("a packed unit does not unpack"))?;
            if out.len() == unpacked as usize {
                Ok(out)
            } else {
                Err(malformed("a packed unit unpacks to the wrong length"))
            }
        }
    }
}

/// The unit at `ordinal` as stored, not unpacked. Its index entry is
/// checked as [`read`] checks it.
///
/// # Errors
///
/// When a read fails, a block fails its checksum, or the index entry is
/// malformed.
pub(crate) fn read_stored(
    segment: &Segment,
    data: SectionHandle,
    index: SectionHandle,
    ordinal: u32,
) -> Result<StoredUnit, SegmentError> {
    let malformed = |reason| SegmentError::new(segment.path(), ErrorKind::Malformed(reason));
    let mut entry = [0; INDEX_ENTRY_LEN];
    segment.read(
        index,
        u64::from(ordinal) * INDEX_ENTRY_LEN as u64,
        &mut entry,
    )?;
    let offset = u64_at(&entry, 0);
    let stored = u32_at(&entry, 8);
    let unpacked = u32_at(&entry, 12);
    let codec = entry[16];
    match codec {
        RAW if stored == unpacked => {}
        ZLIB if unpacked <= MAX_UNPACKED => {}
        _ => return Err(malformed("unknown packed unit codec")),
    }
    let end = offset
        .checked_add(u64::from(stored))
        .ok_or_else(|| malformed("a packed unit's range overflows"))?;
    let bytes = segment.read_range(data, offset..end)?;
    Ok(StoredUnit {
        codec,
        unpacked,
        bytes,
    })
}
