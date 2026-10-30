//! Synthetic SUBC router and controlled read holder for native broker tests.
//! Only RouteBind populates authority. A dial's selector must match a live bind;
//! request JSON cannot manufacture a scope or choose an upstream principal.

use super::*;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use subc_protocol::scope::ScopeStamp;

#[derive(Default)]
pub(super) struct ReadHolder {
    pub binds: HashMap<String, ScopeStamp>,
    pub calls: Vec<Value>,
    pub dials: Vec<Value>,
    pub hold_reads: Option<Arc<tokio::sync::Notify>>,
    pub results: VecDeque<Value>,
}

pub(super) async fn serve(listener: TcpListener, holder: Arc<Mutex<ReadHolder>>) {
    while let Ok((stream, _)) = listener.accept().await {
        let holder = holder.clone();
        tokio::spawn(connection(stream, holder));
    }
}
async fn connection(mut stream: TcpStream, holder: Arc<Mutex<ReadHolder>>) {
    if authenticate_server(
        &mut stream,
        &[0x42; subc_transport::KEY_LEN],
        &[0x24; subc_transport::DAEMON_ID_LEN],
        "worker-jail",
        Duration::from_secs(5),
    )
    .await
    .is_err()
    {
        return;
    }
    let mut channels: HashMap<u16, ScopeStamp> = HashMap::new();
    let mut channel = 40;
    while let Ok(Some(frame)) = read_frame(&mut stream).await {
        let header = frame.header;
        let respond = |ty, body: Value| {
            Frame::build_with_version(
                header.ver,
                ty,
                header.flags,
                header.channel,
                header.epoch,
                header.corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .unwrap()
        };
        let response = match header.ty {
            FrameType::Hello => Some(respond(
                FrameType::HelloAck,
                serde_json::to_value(ModuleHelloAckBody {
                    negotiated_ver: PROTOCOL_VERSION,
                    subc_ops: vec![],
                    subc_capabilities: vec![],
                    storage: None,
                    machine_id: None,
                })
                .unwrap(),
            )),
            FrameType::Ping => {
                Some(Frame::build(FrameType::Pong, flags(), 0, 0, header.corr, vec![]).unwrap())
            }
            FrameType::Goodbye => {
                channels.remove(&header.channel);
                None
            }
            FrameType::Request => {
                let body: Value = serde_json::from_slice(&frame.body).unwrap();
                if header.channel == 0 && body["op"] == "route.open" {
                    let selector = &body["scope"];
                    let plexus = body["target"]
                        == serde_json::to_value(RouteTarget::ToolProvider {
                            module_id: "plexus".into(),
                        })
                        .unwrap();
                    if plexus {
                        holder.lock().unwrap().dials.push(body.clone());
                    }
                    let found = holder
                        .lock()
                        .unwrap()
                        .binds
                        .values()
                        .find(|stamp| {
                            plexus
                                && selector["owner"] == serde_json::to_value(&stamp.owner).unwrap()
                                && selector["ref"] == stamp.scope_ref
                                && selector["scope_epoch"] == stamp.scope_epoch
                        })
                        .cloned();
                    if let Some(stamp) = found {
                        channel += 1;
                        channels.insert(channel, stamp);
                        Some(respond(
                            FrameType::Response,
                            json!({"op":"route.open","route_channel":channel,"route_epoch":1}),
                        ))
                    } else {
                        Some(respond(
                            FrameType::Error,
                            json!({"code":"scope_ended","message":"no matching live bind"}),
                        ))
                    }
                } else if let Some(stamp) = channels.get(&header.channel) {
                    let active = holder.lock().unwrap().binds.values().any(|live| {
                        serde_json::to_value(live).unwrap() == serde_json::to_value(stamp).unwrap()
                    });
                    if !active
                        || header.epoch != 1
                        || body["name"] != "github_read"
                        || body["arguments"]["repository"] != "cortexkit/aft"
                    {
                        Some(respond(
                            FrameType::Error,
                            json!({"code":"scope_ended","message":"scope or repository refused"}),
                        ))
                    } else {
                        holder.lock().unwrap().calls.push(json!({"principal":stamp.attributes.agent_id,"repository":body["arguments"]["repository"],"scope":stamp,"request":body}));
                        let hold = holder.lock().unwrap().hold_reads.clone();
                        if let Some(hold) = hold {
                            hold.notified().await;
                        }
                        let replay = holder.lock().unwrap().results.pop_front();
                        let result = if let Some(result) = replay {
                            result
                        } else if body["arguments"]["action"]
                            .as_str()
                            .unwrap_or("")
                            .starts_with("github.search.")
                        {
                            json!({"total_count":0,"incomplete_results":false,"items":[],"next_page":null})
                        } else {
                            json!({"status":200,"body":{"sentinel":"worker-bound-read","full_name":"cortexkit/aft","number":7,"workflow_runs":[]},"next_page":null})
                        };
                        Some(respond(
                            FrameType::Response,
                            json!({"result":result,"isError":false}),
                        ))
                    }
                } else {
                    Some(respond(
                        FrameType::Error,
                        json!({"code":"scope_absent","message":"scoped read required"}),
                    ))
                }
            }
            _ => None,
        };
        if let Some(response) = response {
            if write_frame(&mut stream, &response).await.is_err() {
                return;
            }
        }
    }
}

pub(super) fn configure(base: &Path, command: &mut Command) {
    use base64::Engine;
    use ring::signature::Ed25519KeyPair;
    const SEED: [u8; 32] = [
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ];
    git(
        &base.join("main"),
        &[
            "remote",
            "set-url",
            "origin",
            "https://github.com/cortexkit/aft.git",
        ],
    );
    let state = base.join("xdg/state/cortexkit/aft/gh-shim");
    std::fs::create_dir_all(&state).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut manifest: Value =
        serde_json::from_str(include_str!("../fixtures/gh_shim/v20-manifest.json")).unwrap();
    manifest["issued_at_unix_secs"] = json!(now);
    let bytes = serde_json::to_vec(&manifest).unwrap();
    let key = Ed25519KeyPair::from_seed_unchecked(&SEED).unwrap();
    let envelope = json!({"artifact_id":"gh-routing-manifest","envelope_version":2,"key_id":"gh-routing-dev-test-key-v1","fetched_at_unix_secs":now,
        "signature":base64::engine::general_purpose::STANDARD.encode(key.sign(&bytes).as_ref()),"manifest_bytes":String::from_utf8(bytes).unwrap()});
    std::fs::write(
        state.join("gh-routing-manifest.json"),
        serde_json::to_vec(&envelope).unwrap(),
    )
    .unwrap();
    let config = base.join("xdg/config/cortexkit/aft.jsonc");
    let mut settings: Value = serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
    settings["subc"] = json!({"connection_file":base.join("connection.json")});
    std::fs::write(config, serde_json::to_vec(&settings).unwrap()).unwrap();
    executable(
        &base.join("bin/gh"),
        "#!/bin/sh\nprintf 'native called\\n' >> \"$PWD/native-gh.log\"\nexit 97\n",
    );
    command
        .env("SUBC_MODULE_ID", "aft")
        .env("SUBC_LAUNCH_NONCE", "worker-gh-read-fixture-launch")
        .env(
            "PATH",
            format!(
                "{}:{}",
                base.join("bin").display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env_remove("AFT_GH_SHIM_STATE_DIR")
        .env_remove("AFT_GH_SHIM_TICKET")
        .env_remove("AFT_GH_READ_SOCKET")
        .env_remove("AFT_GH_READ_CAPABILITY_FILE")
        .stderr(std::fs::File::create(base.join("daemon.log")).unwrap());
}

/// On classic V1 routes, a successful bash reply carries terminal state even
/// for background:true. Tests identify running tasks through the daemon's
/// private-temp-to-task-metadata association instead of awaiting an early reply.
pub(super) fn observe_task(temp: &Path) -> (String, PathBuf) {
    let name = temp
        .file_name()
        .expect("private temp name")
        .to_string_lossy();
    let owner = temp.with_file_name(format!("{name}.owner"));
    let io = PathBuf::from(
        std::fs::read_to_string(&owner)
            .unwrap_or_else(|error| panic!("task temp association {}: {error}", owner.display())),
    );
    assert_eq!(io.file_name().unwrap(), "io");
    let task_dir = io.parent().unwrap();
    let metadata = task_dir.join("control/metadata.json");
    let task = aft::bash_background::persistence::read_task(&metadata).unwrap();
    assert_eq!(task.sandbox_temp_dir.as_deref(), Some(temp));
    assert_eq!(task_dir.file_name().unwrap(), task.task_id.as_str());
    (task.task_id, metadata)
}
