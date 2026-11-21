//! Lossless front coding of a fragment's string table, before record compression.
//!
//! Name-scoped dependency keys repeat long paths. Sharing their leading bytes
//! explicitly works even when the preceding path is outside zlib's window.
//! String indices and all fields after the table keep their original encoding.

use aft_store::content::DecodeError;

const MAGIC: &[u8] = b"AFTF\x02";
// Match the store's maximum unpacked record size. A damaged shared-length field
// must not turn a small stored record into an unbounded allocation.
const MAX_EXPANDED: usize = 256 << 20;

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn number(&mut self) -> Result<usize, DecodeError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self
                .bytes
                .get(self.at)
                .ok_or(DecodeError("short fragment table"))?;
            self.at += 1;
            if shift == 63 && byte > 1 {
                return Err(DecodeError("fragment table integer overflow"));
            }
            value |= u64::from(byte & 127) << shift;
            if byte < 128 {
                return usize::try_from(value)
                    .map_err(|_| DecodeError("fragment table size overflow"));
            }
        }
        Err(DecodeError("long fragment table integer"))
    }

    fn blob(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.number()?;
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or(DecodeError("short fragment table string"))?;
        let value = &self.bytes[self.at..end];
        self.at = end;
        Ok(value)
    }
}

fn number(out: &mut Vec<u8>, mut value: usize) {
    while value >= 128 {
        out.push((value as u8 & 127) | 128);
        value >>= 7;
    }
    out.push(value as u8);
}

pub(super) fn encode(bytes: &[u8]) -> Vec<u8> {
    let mut input = Reader { bytes, at: 0 };
    let count = input.number().expect("the encoder wrote a table count");
    let mut out = Vec::with_capacity(bytes.len());
    out.extend_from_slice(MAGIC);
    number(&mut out, count);
    let mut previous: &[u8] = &[];
    for _ in 0..count {
        let value = input.blob().expect("the encoder wrote a table string");
        let shared = previous
            .iter()
            .zip(value)
            .take_while(|(a, b)| a == b)
            .count();
        number(&mut out, shared);
        number(&mut out, value.len() - shared);
        out.extend_from_slice(&value[shared..]);
        previous = value;
    }
    out.extend_from_slice(&bytes[input.at..]);
    out
}

pub(super) fn decode(bytes: &[u8]) -> Result<Vec<u8>, DecodeError> {
    if !bytes.starts_with(MAGIC) {
        return Err(DecodeError("unknown fragment string-table format"));
    }
    let mut input = Reader {
        bytes,
        at: MAGIC.len(),
    };
    let count = input.number()?;
    if count > (bytes.len() - input.at) / 2 {
        return Err(DecodeError("fragment table count exceeds bytes left"));
    }
    let mut out = Vec::new();
    number(&mut out, count);
    let mut previous = 0..0;
    for _ in 0..count {
        let shared = input.number()?;
        let suffix = input.blob()?;
        if shared > previous.len() {
            return Err(DecodeError("fragment table prefix exceeds previous string"));
        }
        let length = shared
            .checked_add(suffix.len())
            .ok_or(DecodeError("fragment table length overflow"))?;
        if out
            .len()
            .checked_add(length)
            .and_then(|len| len.checked_add(10))
            .is_none_or(|len| len > MAX_EXPANDED)
        {
            return Err(DecodeError("expanded fragment exceeds record limit"));
        }
        number(&mut out, length);
        let start = out.len();
        out.extend_from_within(previous.start..previous.start + shared);
        out.extend_from_slice(suffix);
        previous = start..out.len();
    }
    let fields = &bytes[input.at..];
    if out
        .len()
        .checked_add(fields.len())
        .is_none_or(|len| len > MAX_EXPANDED)
    {
        return Err(DecodeError("expanded fragment exceeds record limit"));
    }
    out.extend_from_slice(fields);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aft_store::content::codec::Encoder;

    #[test]
    fn fragment_front_coding_preserves_indices_fields_and_split_unicode() {
        let mut original = Encoder::default();
        original.str("é");
        original.str("ê");
        original.str("é");
        original.blob(&[255, 0, 128]);
        let original = original.finish();
        assert_eq!(decode(&encode(&original)).unwrap(), original);
    }

    #[test]
    fn fragment_front_coding_rejects_invalid_prefix_count_length_and_version() {
        for damaged in [
            vec![1, 1, 0],       // The first string has no predecessor to share.
            vec![2, 0, 0],       // The count cannot fit in the bytes left.
            vec![1, 0, 3, b'x'], // The suffix ends early.
            vec![255; 12],       // An overflowing count.
        ] {
            let mut bytes = MAGIC.to_vec();
            bytes.extend(damaged);
            assert!(decode(&bytes).is_err());
        }
        assert!(decode(b"AFTF\x01\0").is_err());
        assert!(decode(&[0, 1]).is_err());
    }
}
