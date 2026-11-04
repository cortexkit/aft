//! The byte encoding content records use: LEB128 varints, and a string table
//! per record so every string is stored once however often it is used.

use std::collections::HashMap;

use super::DecodeError;

/// Writes one record's fields; strings go to the record's table.
#[derive(Debug, Default)]
pub struct Encoder {
    out: Vec<u8>,
    strings: Vec<Vec<u8>>,
    index: HashMap<Vec<u8>, u32>,
}

impl Encoder {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "each byte keeps the low seven bits on purpose"
    )]
    pub fn u64(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.out.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.out.push(value as u8);
    }

    pub fn u32(&mut self, value: u32) {
        self.u64(u64::from(value));
    }

    pub fn usize(&mut self, value: usize) {
        self.u64(value as u64);
    }

    pub fn bool(&mut self, value: bool) {
        self.out.push(u8::from(value));
    }

    pub fn byte(&mut self, value: u8) {
        self.out.push(value);
    }

    pub fn str(&mut self, value: &str) {
        self.bytes_ref(value.as_bytes());
    }

    /// A string by its index in the table.
    fn bytes_ref(&mut self, value: &[u8]) {
        let next = u32::try_from(self.strings.len()).expect("fewer than 2^32 strings");
        let index = *self.index.entry(value.to_vec()).or_insert_with(|| {
            self.strings.push(value.to_vec());
            next
        });
        self.u32(index);
    }

    /// An optional string: 0 for none, otherwise its index plus one.
    pub fn opt_str(&mut self, value: Option<&str>) {
        match value {
            None => self.u64(0),
            Some(value) => {
                let next = u32::try_from(self.strings.len()).expect("fewer than 2^32 strings");
                let index = *self
                    .index
                    .entry(value.as_bytes().to_vec())
                    .or_insert_with(|| {
                        self.strings.push(value.as_bytes().to_vec());
                        next
                    });
                self.u64(u64::from(index) + 1);
            }
        }
    }

    pub fn strs(&mut self, values: &[String]) {
        self.usize(values.len());
        for value in values {
            self.str(value);
        }
    }

    /// Raw bytes stored inline, not in the table.
    pub fn blob(&mut self, value: &[u8]) {
        self.usize(value.len());
        self.out.extend_from_slice(value);
    }

    /// The string table followed by the fields.
    pub fn finish(self) -> Vec<u8> {
        let mut table = Self::default();
        table.usize(self.strings.len());
        for string in &self.strings {
            table.blob(string);
        }
        let mut out = table.out;
        out.extend_from_slice(&self.out);
        out
    }
}

/// Reads what an [`Encoder`] wrote.
#[derive(Debug)]
pub struct Decoder<'a> {
    input: &'a [u8],
    position: usize,
    strings: Vec<&'a str>,
}

impl<'a> Decoder<'a> {
    pub fn new(input: &'a [u8]) -> Result<Self, DecodeError> {
        let mut decoder = Self {
            input,
            position: 0,
            strings: Vec::new(),
        };
        let count = decoder.usize()?;
        if count > input.len() {
            return Err(DecodeError("string table count"));
        }
        let mut strings = Vec::with_capacity(count);
        for _ in 0..count {
            let bytes = decoder.blob()?;
            strings
                .push(std::str::from_utf8(bytes).map_err(|_| DecodeError("string is not UTF-8"))?);
        }
        decoder.strings = strings;
        Ok(decoder)
    }

    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self
                .input
                .get(self.position)
                .ok_or(DecodeError("varint runs past the end"))?;
            self.position += 1;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(DecodeError("varint is too long"))
    }

    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        u32::try_from(self.u64()?).map_err(|_| DecodeError("value exceeds 32 bits"))
    }

    pub fn usize(&mut self) -> Result<usize, DecodeError> {
        usize::try_from(self.u64()?).map_err(|_| DecodeError("value exceeds usize"))
    }

    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError("bool is neither 0 nor 1")),
        }
    }

    pub fn byte(&mut self) -> Result<u8, DecodeError> {
        let byte = *self
            .input
            .get(self.position)
            .ok_or(DecodeError("record ends early"))?;
        self.position += 1;
        Ok(byte)
    }

    pub fn str(&mut self) -> Result<&'a str, DecodeError> {
        let index = self.usize()?;
        self.strings
            .get(index)
            .copied()
            .ok_or(DecodeError("string index out of range"))
    }

    pub fn string(&mut self) -> Result<String, DecodeError> {
        self.str().map(str::to_string)
    }

    pub fn opt_string(&mut self) -> Result<Option<String>, DecodeError> {
        match self.usize()? {
            0 => Ok(None),
            index => self
                .strings
                .get(index - 1)
                .map(|value| Some((*value).to_string()))
                .ok_or(DecodeError("string index out of range")),
        }
    }

    pub fn strings(&mut self) -> Result<Vec<String>, DecodeError> {
        let count = self.count()?;
        (0..count).map(|_| self.string()).collect()
    }

    pub fn blob(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.usize()?;
        let end = self
            .position
            .checked_add(len)
            .filter(|end| *end <= self.input.len())
            .ok_or(DecodeError("bytes run past the end"))?;
        let bytes = &self.input[self.position..end];
        self.position = end;
        Ok(bytes)
    }

    /// A count of items still to come, checked against the bytes left so a
    /// damaged count cannot ask for a huge allocation.
    pub fn count(&mut self) -> Result<usize, DecodeError> {
        let count = self.usize()?;
        if count > self.input.len() - self.position {
            return Err(DecodeError("count exceeds the bytes left"));
        }
        Ok(count)
    }

    pub fn finish(self) -> Result<(), DecodeError> {
        if self.position == self.input.len() {
            Ok(())
        } else {
            Err(DecodeError("bytes after the record"))
        }
    }
}
