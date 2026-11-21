//! The engram session door transports opaque bytes on the caller's own stream.
//! Cut management belongs to entorhinal; AFT never calls `cut.*`.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{compress256, digest::generic_array::GenericArray};

pub const PAGE_LIMIT: usize = 1_048_576;

#[derive(Debug, Clone)]
pub struct DoorError {
    pub code: String,
    pub detail: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    Retry,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamRefusal {
    pub code: &'static str,
    pub methods: &'static [&'static str],
    pub next: FailureClass,
}

const ALL: &[&str] = &[
    "session.admit",
    "session.cover",
    "session.read",
    "session.offsets",
];
const ADMIT: &[&str] = &["session.admit"];
const READ: &[&str] = &["session.read"];
const ADMIT_READ: &[&str] = &["session.admit", "session.read"];
const CLOUD: &[&str] = &["session.admit", "session.cover", "session.read"];

/// The stream-method rows of engram's SESSION_WRITER_CONTRACT at 6ca43ec0.
pub const STREAM_REFUSALS: &[StreamRefusal] = &[
    StreamRefusal {
        code: "invalid_request",
        methods: ALL,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "caller_forbidden",
        methods: ALL,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "stream_label_too_long",
        methods: ALL,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "unavailable",
        methods: ALL,
        next: FailureClass::Retry,
    },
    StreamRefusal {
        code: "record_too_large",
        methods: ADMIT_READ,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "response_too_large",
        methods: ALL,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "offset_mismatch",
        methods: ADMIT,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "lease_acquiring",
        methods: ADMIT,
        next: FailureClass::Retry,
    },
    StreamRefusal {
        code: "lease_unavailable",
        methods: CLOUD,
        next: FailureClass::Retry,
    },
    StreamRefusal {
        code: "lease_held_elsewhere",
        methods: ADMIT,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "lease_not_writable",
        methods: ADMIT,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "container_handed_off",
        methods: ADMIT,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "cut_pending_handoff",
        methods: ADMIT,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "offset_out_of_range",
        methods: READ,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "below_floor",
        methods: READ,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "cut_not_published",
        methods: READ,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "not_found",
        methods: READ,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "verify_failed",
        methods: READ,
        next: FailureClass::Stop,
    },
    StreamRefusal {
        code: "cut_item_missing",
        methods: READ,
        next: FailureClass::Stop,
    },
];

/// Unknown codes, including known codes on another method, are unavailable and
/// retryable rather than an invented terminal refusal.
pub fn classify(method: &str, code: &str) -> FailureClass {
    STREAM_REFUSALS
        .iter()
        .find(|row| row.code == code && row.methods.contains(&method))
        .map(|row| row.next)
        .unwrap_or(FailureClass::Retry)
}

fn normalize(method: &str, mut error: DoorError) -> DoorError {
    if !STREAM_REFUSALS
        .iter()
        .any(|row| row.code == error.code && row.methods.contains(&method))
    {
        error.code = "unavailable".into();
    }
    error
}

impl DoorError {
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            code: "unavailable".into(),
            detail: json!({"message":message.into()}),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Offsets {
    pub durable_offset: u64,
    pub cloud_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Midstate {
    pub chaining: String,
    pub tail: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadPage {
    pub bytes: String,
    pub from_offset: u64,
    pub end_offset: u64,
    pub stream_end: u64,
    pub digest: Option<String>,
    pub floor: Option<u64>,
    pub midstate: Option<Midstate>,
}

pub trait SessionDoor: Send + Sync {
    fn admit(&self, session: &str, offset: u64, bytes: &[u8]) -> Result<Offsets, DoorError>;
    fn cover(&self, session: &str) -> Result<Offsets, DoorError>;
    fn offsets(&self, session: &str) -> Result<Offsets, DoorError>;
    fn read(
        &self,
        session: &str,
        offset: u64,
        limit: usize,
        cut: Option<&str>,
    ) -> Result<ReadPage, DoorError>;
}

/// The transport supplies an already daemon-attested module route. There is no
/// principal or stream parameter that a caller could use to impersonate AFT.
pub trait JsonDoorTransport: Send + Sync {
    fn call(
        &self,
        method: &str,
        params: Value,
        timeout: std::time::Duration,
    ) -> Result<Value, DoorError>;
}

pub struct EngramDoor<T>(pub T);

impl<T: JsonDoorTransport> EngramDoor<T> {
    fn offsets_call(&self, method: &str, params: Value, long: bool) -> Result<Offsets, DoorError> {
        let timeout = std::time::Duration::from_secs(if long { 600 } else { 30 });
        serde_json::from_value(
            self.0
                .call(method, params, timeout)
                .map_err(|error| normalize(method, error))?,
        )
        .map_err(|error| DoorError::unavailable(error.to_string()))
    }
}

impl<T: JsonDoorTransport> SessionDoor for EngramDoor<T> {
    fn admit(&self, session: &str, offset: u64, bytes: &[u8]) -> Result<Offsets, DoorError> {
        if bytes.len() > PAGE_LIMIT {
            return Err(DoorError {
                code: "record_too_large".into(),
                detail: json!({"cap":PAGE_LIMIT}),
            });
        }
        self.offsets_call(
            "session.admit",
            json!({"sessionRef":session,"fromOffset":offset,"bytes":hex(bytes)}),
            false,
        )
    }
    fn cover(&self, session: &str) -> Result<Offsets, DoorError> {
        self.offsets_call("session.cover", json!({"sessionRef":session}), true)
    }
    fn offsets(&self, session: &str) -> Result<Offsets, DoorError> {
        self.offsets_call("session.offsets", json!({"sessionRef":session}), false)
    }
    fn read(
        &self,
        session: &str,
        offset: u64,
        limit: usize,
        cut: Option<&str>,
    ) -> Result<ReadPage, DoorError> {
        if limit > PAGE_LIMIT {
            return Err(DoorError {
                code: "record_too_large".into(),
                detail: json!({"cap":PAGE_LIMIT}),
            });
        }
        let mut params = json!({"sessionRef":session,"fromOffset":offset,"limit":limit});
        if let Some(cut) = cut {
            params["cutId"] = json!(cut);
        }
        serde_json::from_value(
            self.0
                .call("session.read", params, std::time::Duration::from_secs(600))
                .map_err(|error| normalize("session.read", error))?,
        )
        .map_err(|error| DoorError::unavailable(error.to_string()))
    }
}

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8] = b"0123456789abcdef";
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(DIGITS[(byte >> 4) as usize] as char);
        text.push(DIGITS[(byte & 15) as usize] as char);
    }
    text
}

pub fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err("invalid hex".into());
    }
    text.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => Ok(byte - b'0'),
                b'a'..=b'f' => Ok(byte - b'a' + 10),
                _ => Err("invalid hex".to_string()),
            };
            Ok((digit(pair[0])? << 4) | digit(pair[1])?)
        })
        .collect()
}

const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// SHA-256 continuation at an authenticated trim floor. The byte count includes
/// the removed prefix; hashing only the retained suffix would verify the wrong
/// message. `tail` is the unfinished block at the floor, not another prefix.
pub struct StreamHash {
    state: [u32; 8],
    tail: Vec<u8>,
    count: u64,
}

impl StreamHash {
    pub fn fresh() -> Self {
        Self {
            state: INITIAL,
            tail: Vec::new(),
            count: 0,
        }
    }
    pub fn resume(floor: u64, midstate: &Midstate) -> Result<Self, String> {
        let chaining = unhex(&midstate.chaining)?;
        let tail = unhex(&midstate.tail)?;
        if chaining.len() != 32 || tail.len() >= 64 || floor % 64 != tail.len() as u64 {
            return Err("invalid_midstate".into());
        }
        let mut state = [0u32; 8];
        for (word, bytes) in state.iter_mut().zip(chaining.chunks_exact(4)) {
            *word = u32::from_be_bytes(bytes.try_into().map_err(|_| "invalid_midstate")?);
        }
        if floor == 0 && state != INITIAL {
            return Err("invalid_midstate".into());
        }
        Ok(Self {
            state,
            tail,
            count: floor,
        })
    }
    pub fn update(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.count = self
            .count
            .checked_add(bytes.len() as u64)
            .ok_or("invalid length")?;
        let mut bytes = bytes;
        if !self.tail.is_empty() {
            let n = (64 - self.tail.len()).min(bytes.len());
            self.tail.extend_from_slice(&bytes[..n]);
            bytes = &bytes[n..];
            if self.tail.len() == 64 {
                compress256(
                    &mut self.state,
                    &[GenericArray::clone_from_slice(&self.tail)],
                );
                self.tail.clear();
            }
        }
        while bytes.len() >= 64 {
            compress256(
                &mut self.state,
                &[GenericArray::clone_from_slice(&bytes[..64])],
            );
            bytes = &bytes[64..];
        }
        self.tail.extend_from_slice(bytes);
        Ok(())
    }
    pub fn midstate(&self) -> Midstate {
        Midstate {
            chaining: hex(&self
                .state
                .iter()
                .flat_map(|word| word.to_be_bytes())
                .collect::<Vec<_>>()),
            tail: hex(&self.tail),
        }
    }
    pub fn finish(mut self) -> String {
        let length = self.count.wrapping_mul(8);
        self.tail.push(0x80);
        while self.tail.len() % 64 != 56 {
            self.tail.push(0);
        }
        self.tail.extend_from_slice(&length.to_be_bytes());
        for block in self.tail.chunks_exact(64) {
            compress256(&mut self.state, &[GenericArray::clone_from_slice(block)]);
        }
        hex(&self
            .state
            .iter()
            .flat_map(|word| word.to_be_bytes())
            .collect::<Vec<_>>())
    }
}

pub struct VerifiedStream {
    pub bytes: Vec<u8>,
    pub end: u64,
    pub floor: u64,
    pub digest: String,
}

/// No imported snapshot is decoded or installed until its complete contiguous
/// byte range matches the signed manifest digest supplied by the door.
pub fn read_verified_cut(
    door: &dyn SessionDoor,
    session: &str,
    cut: &str,
    cap: u64,
) -> Result<VerifiedStream, String> {
    read_verified_cut_from(door, session, cut, cap, 0)
}

/// A caller that already knows a trim floor may start there. A below_floor
/// refusal is terminal for this cut-bound import, never a hint to retry at the
/// error's floor. Only a successful page can replace the authenticated state.
pub fn read_verified_cut_from(
    door: &dyn SessionDoor,
    session: &str,
    cut: &str,
    cap: u64,
    initial_floor: u64,
) -> Result<VerifiedStream, String> {
    let first = door
        .read(session, initial_floor, PAGE_LIMIT, Some(cut))
        .map_err(|error| error.code)?;
    let mut floor = first.floor.ok_or("missing authenticated floor")?;
    let end = first.stream_end;
    if end > cap || floor > end {
        return Err("session_limit".into());
    }
    let digest = first.digest.clone().ok_or("missing authenticated digest")?;
    if unhex(&digest)?.len() != 32 {
        return Err("invalid digest".into());
    }
    let mut midstate = first
        .midstate
        .clone()
        .ok_or("missing authenticated midstate")?;
    let mut hash = StreamHash::resume(floor, &midstate)?;
    let mut offset = floor;
    let mut bytes = Vec::new();
    let mut page = first;
    let mut restarts = 0usize;
    loop {
        if page.stream_end != end || page.digest.as_deref() != Some(&digest) {
            return Err("noncontiguous or changed cut".into());
        }
        let observed_floor = page.floor.ok_or("missing authenticated floor")?;
        if observed_floor != floor {
            if observed_floor > end {
                return Err("invalid floor".into());
            }
            // An authenticated cloud snapshot can supersede a stale local floor.
            // Drop every buffered byte and restart its hash, preserving the
            // immutable cut digest and end rather than stitching two snapshots.
            floor = observed_floor;
            midstate = page
                .midstate
                .clone()
                .ok_or("missing authenticated midstate")?;
            hash = StreamHash::resume(floor, &midstate)?;
            bytes.clear();
            offset = floor;
            restarts += 1;
            if restarts > 8 {
                return Err("unavailable".into());
            }
            page = door
                .read(session, floor, PAGE_LIMIT, Some(cut))
                .map_err(|error| error.code)?;
            continue;
        }
        if page.from_offset != offset || page.midstate.as_ref() != Some(&midstate) {
            return Err("noncontiguous or changed cut".into());
        }
        if page.bytes.len() > PAGE_LIMIT * 2 {
            return Err("record_too_large".into());
        }
        let decoded = unhex(&page.bytes)?;
        if page.end_offset
            != offset
                .checked_add(decoded.len() as u64)
                .ok_or("invalid length")?
            || page.end_offset > end
        {
            return Err("noncontiguous cut".into());
        }
        hash.update(&decoded)?;
        bytes.extend(decoded);
        offset = page.end_offset;
        if offset == end {
            break;
        }
        if page.from_offset == offset {
            return Err("nonprogressing cut".into());
        }
        page = door
            .read(session, offset, PAGE_LIMIT, Some(cut))
            .map_err(|error| error.code)?;
    }
    if hash.finish() != digest {
        return Err("digest_mismatch".into());
    }
    Ok(VerifiedStream {
        bytes,
        end,
        floor,
        digest,
    })
}
