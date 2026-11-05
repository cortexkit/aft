//! The small files of a family: manifests, horizons, checkout states and
//! lease records.
//!
//! Each is one checksummed record, written once to a new name and never
//! changed. A file whose bytes do not check (torn by a crash, or damaged) is
//! not valid, and readers fall back to the newest valid file of its kind.
//!
//! Layout, all little-endian:
//!
//! | Bytes | Field |
//! |---|---|
//! | 8 | magic `AFTREC\0\0` |
//! | 2 | kind |
//! | 2 | kind's format version |
//! | 4 | flags, zero |
//! | 16 | family id |
//! | 8 | number: the generation, horizon, state or lease number in the name |
//! | 8 | payload length |
//! | n | payload |
//! | 16 | BLAKE3-128 of everything before it |

use std::io;
use std::path::Path;

use crate::ids::{Digest128, FamilyId};
use crate::le::{array, u16_at, u32_at, u64_at};

const MAGIC: [u8; 8] = *b"AFTREC\0\0";
const HEADER_LEN: usize = 48;

/// Records larger than this are refused before they are read whole, so a
/// damaged length field cannot ask for an absurd allocation. The largest
/// record is a full checkout state, well under this.
const MAX_PAYLOAD: u64 = 256 << 20;

/// What a record file holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Manifest,
    Horizon,
    State,
    Lease,
}

impl Kind {
    const fn code(self) -> u16 {
        match self {
            Self::Manifest => 1,
            Self::Horizon => 2,
            Self::State => 3,
            Self::Lease => 4,
        }
    }

    /// The format version this build writes and reads for the kind.
    const fn version(self) -> u16 {
        1
    }
}

/// Why a record file was not accepted.
#[derive(Debug, thiserror::Error)]
pub(crate) enum RecordError {
    #[error("{0}")]
    Io(#[from] io::Error),
    /// Torn, truncated or damaged: the bytes do not check.
    #[error("the record's bytes do not check ({0})")]
    Invalid(&'static str),
    #[error("record format version {found} is not supported; this build reads version {supported}")]
    UnsupportedVersion { found: u16, supported: u16 },
    #[error("record belongs to family {found}, expected {expected}")]
    FamilyMismatch { found: FamilyId, expected: FamilyId },
}

/// Encodes a record.
pub(crate) fn encode(kind: Kind, family: FamilyId, number: u64, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_LEN + payload.len() + Digest128::LEN);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&kind.code().to_le_bytes());
    bytes.extend_from_slice(&kind.version().to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(family.digest().as_bytes());
    bytes.extend_from_slice(&number.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    bytes.extend_from_slice(payload);
    let checksum = Digest128::of(&bytes);
    bytes.extend_from_slice(checksum.as_bytes());
    bytes
}

/// Checks a record and returns its payload.
///
/// `number` is the number in the file's name: a record copied or renamed to
/// another name does not pass as that name's record.
pub(crate) fn decode(
    bytes: &[u8],
    kind: Kind,
    family: FamilyId,
    number: u64,
) -> Result<&[u8], RecordError> {
    if bytes.len() < HEADER_LEN + Digest128::LEN {
        return Err(RecordError::Invalid("shorter than a record"));
    }
    if array::<8>(bytes, 0) != MAGIC {
        return Err(RecordError::Invalid("no record magic"));
    }
    let payload_len = u64_at(bytes, 40);
    if payload_len > MAX_PAYLOAD
        || bytes.len() as u64 != HEADER_LEN as u64 + payload_len + Digest128::LEN as u64
    {
        return Err(RecordError::Invalid("length does not match the file"));
    }
    let body_len = bytes.len() - Digest128::LEN;
    let stored = Digest128::from_bytes(array(bytes, body_len));
    if Digest128::of(&bytes[..body_len]) != stored {
        return Err(RecordError::Invalid("checksum"));
    }
    // The bytes are intact from here: a mismatch below is a real record of
    // another kind, version or family, not damage.
    if u16_at(bytes, 8) != kind.code() || u32_at(bytes, 12) != 0 || u64_at(bytes, 32) != number {
        return Err(RecordError::Invalid(
            "kind, flags or number differ from the name",
        ));
    }
    let version = u16_at(bytes, 10);
    if version != kind.version() {
        return Err(RecordError::UnsupportedVersion {
            found: version,
            supported: kind.version(),
        });
    }
    let found = FamilyId::from_digest(Digest128::from_bytes(array(bytes, 16)));
    if found != family {
        return Err(RecordError::FamilyMismatch {
            found,
            expected: family,
        });
    }
    Ok(&bytes[HEADER_LEN..body_len])
}

/// Writes a record to `path`, which must not exist, durably.
pub(crate) fn write(
    path: &Path,
    kind: Kind,
    family: FamilyId,
    number: u64,
    payload: &[u8],
) -> io::Result<()> {
    crate::fs::write_new_durable(path, &encode(kind, family, number, payload))
}

/// Reads and checks the record at `path`.
pub(crate) fn read(
    path: &Path,
    kind: Kind,
    family: FamilyId,
    number: u64,
) -> Result<Vec<u8>, RecordError> {
    let limit = HEADER_LEN as u64 + MAX_PAYLOAD + Digest128::LEN as u64;
    let bytes = crate::fs::read_at_most(path, limit)
        .map_err(RecordError::Io)?
        .ok_or(RecordError::Invalid("larger than any record"))?;
    decode(&bytes, kind, family, number).map(<[u8]>::to_vec)
}

/// The refusal [`read`] gives the record at `path` when it names a format
/// version above the one this build reads for `kind`, as a newer build
/// writes it. Only the header is read unless it names such a version; then
/// the whole record is read, so damaged bytes are told apart from a newer
/// record exactly as [`read`] tells them. `None` for any other record, or
/// one that cannot be read: whoever reads it later reports why.
pub(crate) fn newer_version(
    path: &Path,
    kind: Kind,
    family: FamilyId,
    number: u64,
) -> Option<RecordError> {
    let file = crate::fs::open_read(path).ok()?;
    let mut header = [0; 12];
    crate::fs::read_exact_at(&file, &mut header, 0).ok()?;
    drop(file);
    if array::<8>(&header, 0) != MAGIC
        || u16_at(&header, 8) != kind.code()
        || u16_at(&header, 10) <= kind.version()
    {
        return None;
    }
    match read(path, kind, family, number) {
        Err(error @ RecordError::UnsupportedVersion { found, supported }) if found > supported => {
            Some(error)
        }
        _ => None,
    }
}

/// A cursor over a record's payload. Every read checks the remaining length
/// and reports a short payload as invalid instead of panicking.
#[derive(Debug)]
pub(crate) struct Payload<'a> {
    bytes: &'a [u8],
}

impl<'a> Payload<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], RecordError> {
        if self.bytes.len() < len {
            return Err(RecordError::Invalid("payload ends early"));
        }
        let (head, rest) = self.bytes.split_at(len);
        self.bytes = rest;
        Ok(head)
    }

    pub(crate) fn u32(&mut self) -> Result<u32, RecordError> {
        Ok(u32_at(self.take(4)?, 0))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, RecordError> {
        Ok(u64_at(self.take(8)?, 0))
    }

    pub(crate) fn digest(&mut self) -> Result<Digest128, RecordError> {
        Ok(Digest128::from_bytes(array(self.take(Digest128::LEN)?, 0)))
    }

    /// Fails unless every byte was read: trailing bytes mean the payload is
    /// not the layout this build expects.
    pub(crate) fn finish(self) -> Result<(), RecordError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(RecordError::Invalid("payload has trailing bytes"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::TestRng;

    fn family(byte: u8) -> FamilyId {
        FamilyId::from_digest(Digest128::from_bytes([byte; 16]))
    }

    #[test]
    fn round_trips_and_checks_name_kind_and_family() {
        let bytes = encode(Kind::Manifest, family(1), 7, b"payload");
        assert_eq!(
            decode(&bytes, Kind::Manifest, family(1), 7).unwrap(),
            b"payload"
        );
        assert!(matches!(
            decode(&bytes, Kind::Horizon, family(1), 7),
            Err(RecordError::Invalid(_))
        ));
        assert!(matches!(
            decode(&bytes, Kind::Manifest, family(1), 8),
            Err(RecordError::Invalid(_))
        ));
        assert!(matches!(
            decode(&bytes, Kind::Manifest, family(2), 7),
            Err(RecordError::FamilyMismatch { .. })
        ));
    }

    #[test]
    fn every_truncation_and_bit_flip_is_refused() {
        let bytes = encode(Kind::State, family(3), 2, &TestRng::new(5).bytes(300));
        for len in 0..bytes.len() {
            assert!(
                decode(&bytes[..len], Kind::State, family(3), 2).is_err(),
                "len {len}"
            );
        }
        for index in 0..bytes.len() {
            for bit in 0..8 {
                let mut flipped = bytes.clone();
                flipped[index] ^= 1 << bit;
                assert!(
                    decode(&flipped, Kind::State, family(3), 2).is_err(),
                    "byte {index} bit {bit}"
                );
            }
        }
    }

    #[test]
    fn payload_cursor_refuses_short_and_trailing_bytes() {
        let mut short = Payload::new(&[1, 2, 3]);
        assert!(short.u32().is_err());
        let mut long = Payload::new(&[0; 9]);
        long.u64().unwrap();
        assert!(long.finish().is_err());
    }
}
