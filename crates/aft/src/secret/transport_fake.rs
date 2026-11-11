//! A single-record claustrum fixture over authenticated subc route plumbing.
//! Approval is an explicit test action representing a verified phone signature;
//! neither retrying a request nor opening another route grants access.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use subc_protocol::scope::{ParentState, ScopeKind, ScopeStamp};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION};

use super::super::wire::{Coverage, Target};

const GOLDENS: &str = include_str!("../../tests/fixtures/secret/secret-use-v1.jsonl");
pub(crate) const NONCE: &str = "fake-aft-launch-nonce";

type PendingKey = (String, String, String, String, String);

pub(crate) fn golden(index: usize) -> Value {
    serde_json::from_str(GOLDENS.lines().nth(index).unwrap()).unwrap()
}

pub(crate) fn refusal(code: &str, class: &str) -> Value {
    json!({"result":{"error":{"code":code,"class":class}}})
}

fn invalid() -> Value {
    json!({"code":"invalid_params","message":"invalid params"})
}

#[derive(Default)]
pub(crate) struct State {
    pub opens: Vec<Value>,
    pub calls: usize,
    pub params: Vec<Value>,
    pub connections: usize,
    pub hold_open: bool,
    pub hold_reply: bool,
    pub disconnect: bool,
    pub store_error: bool,
    pub setup_incomplete: bool,
    pub other_pending_in_scope: usize,
    pub other_pending_total: usize,
    pub record: Option<Record>,
    pub records: Vec<Record>,
    pub once: bool,
    pub next_request: usize,
    approved: HashSet<PendingKey>,
    pub(super) pending: HashMap<PendingKey, Value>,
}

#[derive(Clone)]
pub(crate) struct Record {
    pub name: String,
    pub host: String,
    pub path: String,
    pub location: String,
    pub key: Vec<u8>,
}

impl State {
    /// Stand in for the vault verifying the phone's signature over these bytes.
    pub fn approve(&mut self, statement_b64url: &str) {
        let keys: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, reply)| reply["statement_b64url"] == statement_b64url)
            .map(|(key, _)| key.clone())
            .collect();
        assert!(!keys.is_empty());
        self.approved.extend(keys);
    }

    fn serve(&mut self, principal: &str, params: Value) -> Value {
        let not_found = || refusal("not_found", "permanent");
        if !matches!(principal, "aft" | "cerebellum") {
            return not_found();
        }
        let Ok(params) = serde_json::from_value::<Params>(params) else {
            return invalid();
        };
        if params.name.is_empty()
            || !params.name.as_bytes()[0].is_ascii_alphanumeric()
            || !params
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return invalid();
        }
        if params.name.len() > 64 {
            return refusal("limit_exceeded", "context_overflow");
        }
        if let Err(error) = valid_target(&params.target) {
            return match error {
                secret_rules::scope::ScopeRefusal::LimitExceeded => {
                    refusal("limit_exceeded", "context_overflow")
                }
                _ => invalid(),
            };
        }
        let stamp: ScopeStamp = match serde_json::from_value(params.stamp) {
            Ok(stamp) => stamp,
            Err(error) if error.to_string().starts_with("missing field ") => return not_found(),
            Err(_) => return invalid(),
        };
        if stamp.scope_epoch > (1 << 53) - 1
            || stamp
                .parent
                .as_ref()
                .is_some_and(|p| p.scope_epoch > (1 << 53) - 1)
        {
            return invalid();
        }
        if stamp.attributes.flow_id.is_some() {
            return not_found();
        }
        let worker_head = stamp
            .parent
            .as_ref()
            .filter(|parent| {
                stamp.parent_state == Some(ParentState::Linked)
                    && parent.scope_epoch == stamp.scope_epoch
            })
            .map(|parent| (render_owner(&parent.owner), parent.scope_ref.clone()));
        let (owner, kind, reference, epoch) = match stamp.kind {
            ScopeKind::Head => (stamp.owner, "head", stamp.scope_ref, stamp.scope_epoch),
            ScopeKind::Worker => (stamp.owner, "worker", stamp.scope_ref, stamp.scope_epoch),
            ScopeKind::Ephemeral => {
                let Some(parent) = stamp.parent.filter(|parent| {
                    stamp.parent_state == Some(ParentState::Linked)
                        && parent.scope_epoch == stamp.scope_epoch
                        && parent.owner
                            == (subc_protocol::Principal::Reserved {
                                module_id: "prefrontal-core".into(),
                            })
                }) else {
                    return not_found();
                };
                (parent.owner, "head", parent.scope_ref, parent.scope_epoch)
            }
        };
        let coverage = params.coverage;
        let record = self
            .records
            .iter()
            .find(|record| record.name == params.name)
            .or(self.record.as_ref())
            .cloned();
        if coverage == Coverage::AgentAndWorkers && kind != "head" {
            return invalid();
        }
        if self.store_error {
            return refusal("store_error", "transient");
        }
        let matches = match &params.target {
            Target::ApiKey(target) => {
                principal == "aft"
                    && params.name == record.as_ref().map_or("key", |r| r.name.as_str())
                    && target.scheme == "https"
                    && target.port.unwrap_or(443) == 443
                    && target.host == record.as_ref().map_or("x.test", |r| r.host.as_str())
                    && {
                        let prefix = record.as_ref().map_or("/repos/a", |r| r.path.as_str());
                        target.path == prefix || target.path.starts_with(&format!("{prefix}/"))
                    }
            }
            Target::Login(_) => false,
        };
        if !matches {
            return not_found();
        }
        let owner = render_owner(&owner);
        let key = (
            params.name.clone(),
            owner.clone(),
            kind.into(),
            reference.clone(),
            coverage.as_str().into(),
        );
        let covered = self.approved.iter().any(
            |(name, grant_owner, grant_kind, grant_ref, grant_coverage)| {
                name == &params.name
                    && ((grant_owner == &owner && grant_kind == kind && grant_ref == &reference)
                        || (kind == "worker"
                            && grant_kind == "head"
                            && grant_coverage == "agent_and_workers"
                            && worker_head.as_ref().is_some_and(|(owner, reference)| {
                                owner == grant_owner && reference == grant_ref
                            })))
            },
        );
        if covered {
            if self.once {
                self.approved.remove(&key);
                self.pending.remove(&key);
            }
            let mut released = golden(3);
            if let Some(record) = &record {
                released["location"] = json!(record.location);
                released["key_b64url"] =
                    json!(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&record.key));
            }
            return json!({"result":released});
        }
        if self.setup_incomplete {
            return refusal("secret_setup_incomplete", "permanent");
        }
        if let Some(pending) = self.pending.get(&key) {
            return json!({"result":pending});
        }
        let scope_pending = self
            .pending
            .keys()
            .filter(|(_, pending_owner, pending_kind, pending_ref, _)| {
                pending_owner == &owner && pending_kind == kind && pending_ref == &reference
            })
            .count();
        if self.other_pending_in_scope + scope_pending >= 8
            || self.other_pending_total + self.pending.len() >= 64
        {
            return refusal("pending_limit", "transient");
        }
        let mut pending = golden(5);
        self.next_request += 1;
        pending["request_id"] = json!(format!("01HF7YAT{:018}", self.next_request));
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let mut statement: Value = serde_json::from_slice(
            &engine
                .decode(pending["statement_b64url"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        statement["request_id"] = pending["request_id"].clone();
        statement["scope"] = json!({"epoch":epoch,"kind":kind,"owner":owner,"ref":reference});
        statement["coverage"] = json!(coverage.as_str());
        if coverage == Coverage::AgentAndWorkers {
            statement["durations"] = json!(["permanent"]);
        }
        statement["agent_id"] = json!(stamp.attributes.agent_id);
        if let Some(record) = &record {
            statement["secret"]["name"] = json!(record.name);
            statement["use"] =
                json!({"host":record.host, "path_prefix":record.path, "location":record.location});
        }
        pending["statement_b64url"] = json!(engine.encode(serde_json::to_vec(&statement).unwrap()));
        self.pending.insert(key, pending.clone());
        json!({"result":pending})
    }
}

fn render_owner(owner: &subc_protocol::Principal) -> String {
    match owner {
        subc_protocol::Principal::Reserved { module_id } => format!("reserved:{module_id}"),
        subc_protocol::Principal::Direct => "direct".into(),
        subc_protocol::Principal::Unverified => "unverified".into(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Params {
    name: String,
    #[serde(default = "default_coverage")]
    coverage: Coverage,
    stamp: Value,
    target: Target,
}

fn default_coverage() -> Coverage {
    Coverage::Scope
}

fn valid_target(target: &Target) -> Result<(), secret_rules::scope::ScopeRefusal> {
    use secret_rules::scope::{ScopeRefusal, SecretScope};
    match target {
        Target::ApiKey(target) => {
            if target.scheme.is_empty()
                || !target
                    .scheme
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"+-.".contains(&b))
            {
                return Err(ScopeRefusal::InvalidParams);
            }
            SecretScope::parse_input("agent_api_key", &json!({"host":target.host}))?;
            if !valid_path(&target.path) {
                return Err(ScopeRefusal::InvalidParams);
            }
            Ok(())
        }
        Target::Login(target) => secret_rules::scope::canonical_origin(&target.origin).map(|_| ()),
    }
}

fn valid_path(path: &str) -> bool {
    if !path.starts_with('/') {
        return false;
    }
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let Some(a) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
                return false;
            };
            let Some(b) = bytes.next().and_then(|b| (b as char).to_digit(16)) else {
                return false;
            };
            let decoded = (a * 16 + b) as u8;
            if !(0x21..0x7f).contains(&decoded) || b"/.%\\;".contains(&decoded) {
                return false;
            }
        } else if !(byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&byte)) {
            return false;
        }
    }
    let segments: Vec<_> = path[1..].split('/').collect();
    segments.iter().enumerate().all(|(i, segment)| {
        (!segment.is_empty() || i == segments.len() - 1)
            && !matches!(segment.split(';').next(), Some("." | ".."))
    })
}

pub(crate) struct FakeClaustrum {
    _directory: tempfile::TempDir,
    pub path: PathBuf,
    pub state: Arc<Mutex<State>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    server: Option<std::thread::JoinHandle<()>>,
}

impl FakeClaustrum {
    pub fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("connection.json");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let key = vec![0x42; subc_transport::KEY_LEN];
        let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
        connection_file::write_atomic(
            &path,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(subc_protocol::PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".into(),
                    port: listener.local_addr().unwrap().port(),
                }],
                key: key.clone(),
                daemon_id,
                pid: std::process::id(),
                daemon_ver: "0.39.0".into(),
            },
        )
        .unwrap();
        let state = Arc::new(Mutex::new(State::default()));
        let served = Arc::clone(&state);
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let mut tasks = tokio::task::JoinSet::new();
                tokio::pin!(stopped);
                loop {
                    tokio::select! {
                        _ = &mut stopped => break,
                        accepted = listener.accept() => {
                            let (mut stream, _) = accepted.unwrap();
                            let state = Arc::clone(&served);
                            let key = key.clone();
                            tasks.spawn(async move {
                                if subc_transport::authenticate_server(&mut stream, &key, &daemon_id, "0.39.0", Duration::from_secs(2)).await.is_err() { return; }
                                state.lock().unwrap().connections += 1;
                                let mut principal = String::new();
                                while let Ok(Some(request)) = subc_transport::read_frame(&mut stream).await {
                                    if request.header.ty == FrameType::Ping {
                                        let pong = Frame::build(FrameType::Pong, flags(), 0, 0, request.header.corr, Vec::new()).unwrap();
                                        if subc_transport::write_frame(&mut stream, &pong).await.is_err() { break; }
                                        continue;
                                    }
                                    if request.header.ty != FrameType::Request { continue; }
                                    let value: Value = serde_json::from_slice(&request.body).unwrap();
                                    let (reply, error) = if value["op"] == "route.open" {
                                        assert_eq!(value["target"], json!({"kind":"management_surface","module_id":"claustrum"}));
                                        assert!(value.get("scope").is_some());
                                        let hold = {
                                            let mut state = state.lock().unwrap();
                                            state.opens.push(value.clone());
                                            state.hold_open
                                        };
                                        if hold { continue; }
                                        if value["consumer_identity"]["launch_nonce"] == NONCE {
                                            principal = value["consumer_identity"]["module_id"].as_str().unwrap().into();
                                        }
                                        (json!({"op":"route.open","route_channel":7,"route_epoch":1}), false)
                                    } else {
                                        assert_eq!(value["method"], "secret.use");
                                        let mut state = state.lock().unwrap();
                                         state.calls += 1;
                                         state.params.push(value["params"].clone());
                                        if state.disconnect { break; }
                                        if state.hold_reply { continue; }
                                        let reply = state.serve(&principal, value["params"].clone());
                                        let error = reply.get("code").is_some();
                                        (reply, error)
                                    };
                                    let frame = Frame::build(if error { FrameType::Error } else { FrameType::Response }, flags(), request.header.channel, request.header.epoch, request.header.corr, serde_json::to_vec(&reply).unwrap()).unwrap();
                                    if subc_transport::write_frame(&mut stream, &frame).await.is_err() { break; }
                                }
                            });
                        }
                        Some(result) = tasks.join_next(), if !tasks.is_empty() => result.unwrap(),
                    }
                }
                tasks.abort_all();
            });
        });
        Self {
            _directory: directory,
            path,
            state,
            stop: Some(stop),
            server: Some(server),
        }
    }
}

impl Drop for FakeClaustrum {
    fn drop(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.server.take().unwrap().join().unwrap();
    }
}

fn flags() -> Flags {
    Flags::new(false, Priority::Interactive, false)
}
