use super::door::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Cut {
    bytes: Vec<u8>,
    digest: String,
    floor: u64,
    midstate: Midstate,
}
#[derive(Default)]
struct State {
    streams: BTreeMap<String, Vec<u8>>,
    cuts: BTreeMap<(String, String), Cut>,
    flip: bool,
    gap: bool,
    read_offsets: Vec<u64>,
    floor_change: Option<u64>,
}
#[derive(Clone, Default)]
pub(super) struct MemoryDoor {
    state: Arc<Mutex<State>>,
    caller: String,
}

impl MemoryDoor {
    pub(super) fn flip_reads(&self) {
        self.state.lock().unwrap().flip = true;
    }
    pub(super) fn aft() -> Self {
        Self {
            caller: "reserved:aft".into(),
            ..Self::default()
        }
    }
    pub(super) fn engram(&self) -> EngramDoor<Self> {
        EngramDoor(self.clone())
    }
    /// Only the fixture driver, acting as entorhinal, may seal a cut. The
    /// participant transport below always refuses AFT's cut-management calls.
    pub(super) fn entorhinal_seal(&self, cut: &str, session: &str, floor: u64) {
        let mut state = self.state.lock().unwrap();
        let bytes = state.streams.get(session).unwrap().clone();
        let mut prefix = StreamHash::fresh();
        prefix.update(&bytes[..floor as usize]).unwrap();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        state.cuts.insert(
            (cut.into(), session.into()),
            Cut {
                bytes,
                digest,
                floor,
                midstate: prefix.midstate(),
            },
        );
    }
}

impl JsonDoorTransport for MemoryDoor {
    fn call(
        &self,
        method: &str,
        params: Value,
        _timeout: std::time::Duration,
    ) -> Result<Value, DoorError> {
        if self.caller != "reserved:aft" || method.starts_with("cut.") {
            return Err(DoorError {
                code: "caller_forbidden".into(),
                detail: Value::Null,
            });
        }
        let session = params["sessionRef"]
            .as_str()
            .ok_or_else(|| DoorError::unavailable("missing session"))?;
        let mut state = self.state.lock().unwrap();
        let error = |code: &str, detail| DoorError {
            code: code.into(),
            detail,
        };
        match method {
            "session.admit" => {
                let bytes = unhex(params["bytes"].as_str().unwrap()).unwrap();
                if bytes.len() > PAGE_LIMIT {
                    return Err(error("record_too_large", json!({"cap":PAGE_LIMIT})));
                }
                let offset = params["fromOffset"].as_u64().unwrap();
                let end = state
                    .streams
                    .get(session)
                    .map_or(0, |bytes| bytes.len() as u64);
                if offset != end {
                    return Err(error("offset_mismatch", json!({"expectedOffset":end})));
                }
                state
                    .streams
                    .entry(session.into())
                    .or_default()
                    .extend(bytes);
                Ok(json!({"durableOffset":state.streams[session].len(),"cloudOffset":0}))
            }
            "session.offsets" | "session.cover" => {
                let end = state.streams.get(session).map_or(0, Vec::len);
                Ok(json!({"durableOffset":end,"cloudOffset":end}))
            }
            "session.read" => {
                let start = params["fromOffset"].as_u64().unwrap();
                state.read_offsets.push(start);
                if start > 0 {
                    if let Some(floor) = state.floor_change.take() {
                        let cut = state
                            .cuts
                            .get_mut(&(
                                params["cutId"].as_str().unwrap().to_owned(),
                                session.to_owned(),
                            ))
                            .unwrap();
                        let mut prefix = StreamHash::fresh();
                        prefix.update(&cut.bytes[..floor as usize]).unwrap();
                        cut.floor = floor;
                        cut.midstate = prefix.midstate();
                    }
                }
                let limit = params["limit"].as_u64().unwrap() as usize;
                if limit > PAGE_LIMIT {
                    return Err(error("record_too_large", json!({"cap":PAGE_LIMIT})));
                }
                let (bytes, sealed) = if let Some(cut) = params["cutId"].as_str() {
                    let cut = state
                        .cuts
                        .get(&(cut.to_owned(), session.to_owned()))
                        .ok_or_else(|| error("not_found", Value::Null))?;
                    if start < cut.floor {
                        return Err(error("below_floor", json!({"floor":cut.floor})));
                    }
                    (&cut.bytes, Some(cut))
                } else {
                    (
                        state
                            .streams
                            .get(session)
                            .ok_or_else(|| error("not_found", Value::Null))?,
                        None,
                    )
                };
                if start > bytes.len() as u64 {
                    return Err(error(
                        "offset_out_of_range",
                        json!({"streamEnd":bytes.len()}),
                    ));
                }
                let end = (start as usize + limit).min(bytes.len());
                let mut page = bytes[start as usize..end].to_vec();
                if state.flip && !page.is_empty() {
                    // Change a historical content byte without breaking the
                    // framing or JSON, so parsing cannot substitute for digest
                    // verification in the native import regression.
                    let needle = b"\"before\":[111,";
                    let byte = page
                        .windows(needle.len())
                        .position(|bytes| bytes == needle)
                        .map(|index| index + needle.len() - 2)
                        .unwrap_or(0);
                    page[byte] ^= 1;
                }
                let mut reply = json!({"bytes":hex(&page),"fromOffset":start + u64::from(state.gap),"endOffset":end,"streamEnd":bytes.len()});
                if let Some(cut) = sealed {
                    reply["digest"] = json!(cut.digest);
                    reply["floor"] = json!(cut.floor);
                    reply["midstate"] = json!(cut.midstate);
                }
                Ok(reply)
            }
            _ => Err(error(
                "invalid_request",
                json!({"method":method,"service":"session-door"}),
            )),
        }
    }
}

#[test]
fn cut_digest_accepts_correct_and_trimmed_streams() {
    let memory = MemoryDoor::aft();
    let door = memory.engram();
    let bytes: Vec<u8> = (0..=255).cycle().take(PAGE_LIMIT + 131).collect();
    door.admit("one", 0, &bytes[..PAGE_LIMIT]).unwrap();
    door.admit("one", PAGE_LIMIT as u64, &bytes[PAGE_LIMIT..])
        .unwrap();
    for floor in [0, 77, 128] {
        memory.entorhinal_seal("cut", "one", floor);
        let verified =
            read_verified_cut_from(&door, "one", "cut", 64 * 1024 * 1024, floor).unwrap();
        assert_eq!(verified.bytes, bytes[floor as usize..]);
        assert_eq!(verified.digest, format!("{:x}", Sha256::digest(&bytes)));
    }
}

#[test]
fn cut_digest_refuses_flipped_byte_without_publication() {
    let memory = MemoryDoor::aft();
    let door = memory.engram();
    door.admit("one", 0, b"authenticated bytes").unwrap();
    memory.entorhinal_seal("cut", "one", 0);
    memory.state.lock().unwrap().flip = true;
    assert_eq!(
        read_verified_cut(&door, "one", "cut", 64 * 1024 * 1024)
            .err()
            .as_deref(),
        Some("digest_mismatch")
    );
}

#[test]
fn cut_digest_refuses_gap_or_out_of_order_page() {
    let memory = MemoryDoor::aft();
    let door = memory.engram();
    door.admit("one", 0, b"authenticated bytes").unwrap();
    memory.entorhinal_seal("cut", "one", 0);
    memory.state.lock().unwrap().gap = true;
    assert!(read_verified_cut(&door, "one", "cut", 64 * 1024 * 1024).is_err());
}

#[test]
fn door_double_enforces_real_admission_and_lease_free_reads() {
    let memory = MemoryDoor::aft();
    let door = memory.engram();
    assert_eq!(
        door.admit("one", 1, b"x").unwrap_err().code,
        "offset_mismatch"
    );
    assert_eq!(
        door.admit("one", 0, &vec![0; PAGE_LIMIT + 1])
            .unwrap_err()
            .code,
        "record_too_large"
    );
    door.admit("one", 0, b"ab").unwrap();
    assert_eq!(door.read("one", 0, 1, None).unwrap().bytes, "61");
    assert_eq!(
        door.read("unadmitted", 0, 1, None).unwrap_err().code,
        "not_found"
    );
    assert_eq!(
        memory
            .call("cut.read", json!({}), std::time::Duration::ZERO)
            .unwrap_err()
            .code,
        "caller_forbidden"
    );
    let forbidden = MemoryDoor {
        caller: "direct".into(),
        state: memory.state.clone(),
    };
    assert_eq!(
        forbidden.engram().read("one", 0, 1, None).unwrap_err().code,
        "caller_forbidden"
    );
    assert_eq!(
        classify("session.read", "a_future_refusal"),
        FailureClass::Retry
    );
}

#[test]
fn cut_floor_change_discards_buffer_and_restarts_from_new_midstate() {
    let memory = MemoryDoor::aft();
    let door = memory.engram();
    let bytes: Vec<u8> = (0..=255).cycle().take(PAGE_LIMIT + 131).collect();
    door.admit("one", 0, &bytes[..PAGE_LIMIT]).unwrap();
    door.admit("one", PAGE_LIMIT as u64, &bytes[PAGE_LIMIT..])
        .unwrap();
    memory.entorhinal_seal("cut", "one", 0);
    memory.state.lock().unwrap().floor_change = Some(77);
    let verified = read_verified_cut(&door, "one", "cut", 64 * 1024 * 1024).unwrap();
    assert_eq!(verified.floor, 77);
    assert_eq!(verified.bytes, bytes[77..]);
    assert_eq!(verified.digest, format!("{:x}", Sha256::digest(&bytes)));
    assert_eq!(
        &memory.state.lock().unwrap().read_offsets[..3],
        &[0, PAGE_LIMIT as u64, 77]
    );
}

#[test]
fn cut_below_floor_stops_without_retrying_error_floor() {
    let memory = MemoryDoor::aft();
    let door = memory.engram();
    door.admit("one", 0, b"retained bytes").unwrap();
    memory.entorhinal_seal("cut", "one", 3);
    assert_eq!(
        read_verified_cut(&door, "one", "cut", 64 * 1024 * 1024)
            .err()
            .as_deref(),
        Some("below_floor")
    );
    assert_eq!(memory.state.lock().unwrap().read_offsets, [0]);
}

#[test]
fn door_stream_refusal_table_matches_published_contract() {
    let doc =
        include_str!("../../tests/fixtures/agent_data/session-door-stream-refusals-6ca43ec0.md");
    let mut expected = Vec::new();
    for line in doc.lines().filter(|line| line.starts_with("| `")) {
        let cells: Vec<_> = line.split('|').map(str::trim).collect();
        let code = cells[1].trim_matches('`').to_owned();
        let methods = cells[2]
            .split(',')
            .map(|method| method.trim().trim_matches('`').to_owned())
            .collect::<Vec<_>>();
        let next = match cells[4] {
            "Stop" => FailureClass::Stop,
            "Retry" => FailureClass::Retry,
            unknown => panic!("unknown contract class {unknown}"),
        };
        expected.push((code, methods, next));
    }
    assert_eq!(expected.len(), 19);
    let actual = STREAM_REFUSALS
        .iter()
        .map(|row| {
            (
                row.code.to_owned(),
                row.methods
                    .iter()
                    .map(|method| (*method).to_owned())
                    .collect::<Vec<_>>(),
                row.next,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    for row in STREAM_REFUSALS {
        for method in row.methods {
            assert_eq!(classify(method, row.code), row.next);
        }
    }
    assert_eq!(
        classify("session.offsets", "offset_mismatch"),
        FailureClass::Retry
    );
    struct InternalRefusal;
    impl JsonDoorTransport for InternalRefusal {
        fn call(&self, _: &str, _: Value, _: std::time::Duration) -> Result<Value, DoorError> {
            Err(DoorError {
                code: "an_internal_engram_code".into(),
                detail: Value::Null,
            })
        }
    }
    assert_eq!(
        EngramDoor(InternalRefusal).offsets("one").unwrap_err().code,
        "unavailable"
    );
}
