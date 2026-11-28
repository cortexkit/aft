use super::{
    tests::{bind, registry, stamp, REMOTE},
    *,
};
use crate::db::remote_grant_rows::{self as grants, Mode};
use crate::subc::test_support::{bind_config_without_indexes, test_root};
use cortexkit_role_tool_provider::call::{SchemaPin, ToolMajorPin};
use tool_provider::CatalogPreset;

const HELD: &str = "held-reply/v1";
const RESULT: &str = result_body::RESULT_FORMAT;
const YIELD: &str = call_yield::CALL_YIELD;
const SCOPES: [&str; 3] = ["pairing-A", "pairing-B", "pairing-denied"];

static READS: LazyLock<StdMutex<Vec<String>>> = LazyLock::new(StdMutex::default);

struct Deny(Arc<AtomicU64>);

impl consent::ConsentTransport for Deny {
    fn call(&self, call: consent::ConsentCall) -> consent::ConsentFuture {
        match call.method {
            "consent.request" => {
                assert_eq!(call.params["operation"], "read");
                self.0.fetch_add(1, Ordering::Relaxed);
                Box::pin(async { Ok(json!({"elicitation_id":"acc_refused"})) })
            }
            "consent.await" => Box::pin(async {
                Ok(json!({"state":"settled", "answered_by":"user", "outcome":"deny"}))
            }),
            _ => panic!("unexpected consent operation: {}", call.method),
        }
    }
}

struct RestoreTransport;
impl Drop for RestoreTransport {
    fn drop(&mut self) {
        consent::install_test_transport(None);
    }
}

fn tool_dispatch(request: RawRequest, ctx: &AppContext) -> Response {
    if request.command == "configure" {
        return crate::commands::configure::handle_configure(&request, ctx);
    }
    READS.lock().unwrap().push(request.command.clone());
    match request.command.as_str() {
        "read" => crate::commands::read::handle_read(&request, ctx),
        "git_conflicts" => crate::commands::conflicts::handle_git_conflicts(ctx, &request),
        _ => Response::success(&request.id, json!({})),
    }
}

fn remote_bind(channel: u16, pairing: &str, caps: &[&str]) -> Value {
    let mut scope = stamp(json!({"kind":"agent", "agent_id":"remote-agent"}));
    scope["attributes"]["federated"]["pairing_id"] = json!(pairing);
    if pairing == SCOPES[2] {
        scope["ref"] = json!("deny-session");
    }
    let mut body = bind(
        channel,
        REMOTE,
        scope,
        Path::new("also-on-A"),
        "tool_provider",
    );
    body["consumer_capabilities"] = json!(caps);
    body
}

fn pin(
    name: &str,
    preset: CatalogPreset,
    runon: bool,
    revision: bool,
    semantics: Option<u64>,
) -> String {
    let tool = tool_provider::tools_for_session(preset, &[], false, runon, revision)
        .into_iter()
        .find(|tool| tool.name == name)
        .unwrap();
    SchemaPin::new(
        name,
        &tool.schema_digest,
        semantics.unwrap_or(tool.semantics),
    )
    .encode()
    .unwrap()
}

fn request(name: &str, preset: &str, args: Value, key: &str, pin: String) -> Value {
    json!({"op":"tool.call", "name":name, "preset":preset, "arguments":crate::subc::bash_selector_tests::shell_fixture_arguments(name, args), "call_key":key, "schema_pin":pin})
}

fn reply(frame: Frame) -> Value {
    let value: Value = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(frame.header.ty, FrameType::Response, "{value}");
    value
}

fn refusal(frame: Frame, code: &str) -> ErrorBody {
    assert_eq!(
        frame.header.ty,
        FrameType::Error,
        "{}",
        String::from_utf8_lossy(&frame.body)
    );
    let error: ErrorBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(error.code, code, "{}", error.message);
    error
}

fn feed() -> Value {
    json!({"name":"late_results", "arguments":{}})
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"].as_str().unwrap()
}

async fn hello(daemon: &mut tokio::io::DuplexStream, ops: Vec<String>) {
    let hello = read_frame(daemon).await.unwrap().unwrap();
    assert_eq!(hello.header.ty, FrameType::Hello);
    write_frame(
        daemon,
        &Frame::build(
            FrameType::HelloAck,
            control_flags(),
            0,
            0,
            hello.header.corr,
            serde_json::to_vec(&ModuleHelloAckBody {
                negotiated_ver: PROTOCOL_VERSION,
                subc_ops: ops,
                subc_capabilities: vec![],
                storage: None,
                machine_id: None,
            })
            .unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn remote_wire_admission_audit_read_and_pairing_custody() {
    let _env = crate::test_env::process_env_lock();
    let _transport = RestoreTransport;
    let card_count = Arc::new(AtomicU64::new(0));
    consent::install_test_transport(Some(Arc::new(Deny(card_count.clone()))));
    READS.lock().unwrap().clear();
    let storage = tempfile::tempdir().unwrap();
    let (_project, root) = test_root("remote-wire-on-b");
    for (path, contents) in [
        ("a.txt", "first pairing contents"),
        ("b.txt", "second pairing contents"),
    ] {
        std::fs::write(root.as_path().join(path), contents).unwrap();
    }
    let outside = storage.path().join("outside.txt");
    std::fs::write(&outside, "must not leave B's selected root").unwrap();
    let app = crate::test_app();
    let db = app.open_db(&storage.path().join("aft.db")).unwrap();
    for pairing in &SCOPES[..2] {
        grants::save(
            &db.lock().unwrap(),
            &grants::Key {
                pairing_id: (*pairing).into(),
                caller_id: "remote-agent".into(),
                project_id: "B-project".into(),
                root_kind: "label",
                root_key: "B-root".into(),
            },
            root.as_path().to_str().unwrap(),
            Mode::ReadWrite,
            "Remote on A",
            "acc_seed",
        )
        .unwrap();
    }
    let entries = registry(root.as_path());
    let executor = Arc::new(Executor::new());
    let (daemon, stream) = tokio::io::duplex(1024 * 1024);
    let (read, write) = tokio::io::split(stream);
    let connection = storage.path().join("connection.json");
    let module = TEST_REGISTRY.scope(
        Arc::new(move || entries.clone()),
        run_module_loop(
            read,
            write,
            &connection,
            app,
            executor.clone(),
            tool_dispatch,
            Some(bind_config_without_indexes(storage.path())),
            false,
            usize::MAX,
            None,
            storage.path(),
            QuiescedLsp::ReapInBackground,
            None,
            None,
            None,
        ),
    );
    let exercise = async {
        let mut daemon = edit_lifetime::Peer::start(daemon).await;
        let mut local = bind(90, "opencode", Value::Null, root.as_path(), "tool_provider");
        local["principal"] = json!({"kind":"direct"});
        reply(daemon.call(0, 1, local).await);
        for (channel, pairing, caps) in [
            (1, SCOPES[0], &[HELD, RESULT][..]),
            (2, SCOPES[1], &[HELD, RESULT, YIELD][..]),
            (3, SCOPES[2], &[HELD, RESULT, YIELD][..]),
            (4, SCOPES[2], &[HELD, YIELD][..]),
            (5, SCOPES[2], &[RESULT, YIELD][..]),
        ] {
            reply(
                daemon
                    .call(
                        0,
                        u64::from(channel) + 1,
                        remote_bind(channel, pairing, caps),
                    )
                    .await,
            );
        }

        let read_pin = pin("read", CatalogPreset::Worker, false, true, None);
        let read = |preset: &str, file: &str, key: &str| {
            request("read", preset, json!({"path": file}), key, read_pin.clone())
        };
        let mut no_pin = read("head", "a.txt", "no-pin");
        no_pin.as_object_mut().unwrap().remove("schema_pin");
        refusal(daemon.call(3, 10, no_pin).await, "invalid_request");
        let mut malformed_pin = read("head", "a.txt", "malformed-pin");
        malformed_pin["schema_pin"] = json!("unparseable");
        refusal(
            daemon.call(3, 11, malformed_pin.clone()).await,
            "invalid_request",
        );
        let mismatch = request(
            "read",
            "head",
            json!({"path":"a.txt"}),
            "stale",
            pin("read", CatalogPreset::Head, false, false, Some(777)),
        );
        let versions = refusal(
            daemon.call(3, 12, mismatch).await,
            "remote_tool_versions_differ",
        );
        assert_eq!(
            versions.message,
            "This tool differs between the AFT versions on the two computers."
        );
        let current_read =
            tool_provider::tools_for_session(CatalogPreset::Head, &[], false, false, false)
                .into_iter()
                .find(|tool| tool.name == "read")
                .unwrap();
        let major = current_read.version.unwrap().major;
        for (corr, stale) in [
            (80, ToolMajorPin::new("read", "0".repeat(64), major)),
            (
                81,
                ToolMajorPin::new("read", &current_read.schema_digest, major + 1),
            ),
            (
                82,
                ToolMajorPin::new("edit", &current_read.schema_digest, major),
            ),
        ] {
            let mut mismatch = read("head", "a.txt", "stale-tp2");
            mismatch["schema_pin"] = json!(stale.encode().unwrap());
            let versions = refusal(
                daemon.call(3, corr, mismatch).await,
                "remote_tool_versions_differ",
            );
            assert_eq!(
                versions.message,
                "This tool differs between the AFT versions on the two computers."
            );
        }
        for (channel, corr) in [(1, 13), (4, 14), (5, 15)] {
            refusal(
                daemon
                    .call(channel, corr, read("head", "a.txt", "transport"))
                    .await,
                "remote_call_not_admitted",
            );
        }
        let bash = request(
            "bash",
            "reader",
            json!({"command":"not-dispatched"}),
            "reader-bash",
            pin("bash", CatalogPreset::Head, true, false, None),
        );
        refusal(daemon.call(1, 16, bash).await, "remote_tool_not_available");
        let mut off = read("head", "a.txt", "ref-off");
        off["arguments"]["ref"] = json!("HEAD");
        refusal(daemon.call(3, 17, off).await, "revision_reads_disabled");
        let escape = read("head", outside.to_str().unwrap(), "outside");
        refusal(daemon.call(3, 18, escape).await, "path_outside_root");
        let inspect = request(
            "aft_inspect",
            "reader",
            json!({}),
            "no-inspect",
            pin("inspect", CatalogPreset::Reader, false, false, None),
        );
        refusal(
            daemon.call(1, 19, inspect).await,
            "remote_tool_not_available",
        );
        assert_eq!(card_count.load(Ordering::Relaxed), 0);
        assert!(READS.lock().unwrap().is_empty());

        refusal(
            daemon.call(3, 20, read("head", "a.txt", "denied")).await,
            "remote_access_denied",
        );
        assert_eq!(card_count.load(Ordering::Relaxed), 1);
        assert!(READS.lock().unwrap().is_empty());
        let first = reply(
            daemon
                .call(1, 21, read("reader", "a.txt", "same-key"))
                .await,
        );
        let versioned_pin = current_read.schema_pin().encode().unwrap();
        assert!(versioned_pin.starts_with("tp2:"));
        let second = reply(
            daemon
                .call(
                    2,
                    22,
                    request(
                        "read",
                        "head",
                        json!({"path":"b.txt"}),
                        "same-key",
                        versioned_pin,
                    ),
                )
                .await,
        );
        assert!(text(&first).contains("first pairing contents"), "{first}");
        assert!(
            text(&second).contains("second pairing contents"),
            "{second}"
        );
        assert_eq!(READS.lock().unwrap().as_slice(), ["read", "read"]);
        assert_eq!(
            reply(
                daemon
                    .call(1, 23, read("reader", "a.txt", "same-key"))
                    .await
            ),
            first
        );
        assert_eq!(READS.lock().unwrap().as_slice(), ["read", "read"]);

        for (channel, pairing) in [(11, SCOPES[0]), (12, SCOPES[1])] {
            reply(
                daemon
                    .call(
                        0,
                        20 + u64::from(channel),
                        remote_bind(channel, pairing, &[HELD, RESULT]),
                    )
                    .await,
            );
        }
        let first_feed = reply(daemon.call(11, 33, feed()).await);
        assert_eq!(first_feed["entries"].as_array().unwrap().len(), 1);
        assert_eq!(first_feed["entries"][0]["call_key"], "same-key");
        assert_eq!(first_feed["entries"][0]["result"], first);
        reply(
            daemon.call(11, 34, json!({"name":"late_results.ack", "arguments":{"through":first_feed["cursor"]}}))
            .await,
        );
        assert_eq!(
            reply(daemon.call(11, 35, feed()).await)["entries"],
            json!([])
        );
        let second_feed = reply(daemon.call(12, 36, feed()).await);
        assert_eq!(second_feed["entries"].as_array().unwrap().len(), 1);
        assert_eq!(second_feed["entries"][0]["result"], second);
        reply(
            daemon
                .call(
                    12,
                    37,
                    json!({"op":"late_results.ack", "through":second_feed["cursor"]}),
                )
                .await,
        );
        assert_eq!(
            reply(daemon.call(12, 38, json!({"op":"late_results"})).await)["entries"],
            json!([])
        );

        check_rows(&db, root.as_path());
        db.lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
        refusal(
            daemon
                .call(1, 39, read("reader", "a.txt", "no-audit"))
                .await,
            "remote_call_not_admitted",
        );
        db.lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=OFF")
            .unwrap();
        check_rows(&db, root.as_path());
        assert_eq!(READS.lock().unwrap().as_slice(), ["read", "read"]);
        assert_eq!(card_count.load(Ordering::Relaxed), 1);

        let mut expected = vec!["read", "read"];
        let mut corr = 40;
        for (name, args, command) in [
            ("grep", json!({"pattern":"pairing"}), "grep"),
            ("glob", json!({"pattern":"*.txt"}), "glob"),
            ("search", json!({"query":"pairing"}), "semantic_search"),
            ("outline", json!({"target":"a.txt"}), "outline"),
            (
                "zoom",
                json!({"batch":[{"target":"a.txt","symbols":"x"}]}),
                "zoom",
            ),
            (
                "callgraph",
                json!({"op":"callers","path":"a.txt","symbol":"x"}),
                "callers",
            ),
            (
                "ast_search",
                json!({"pattern":"f()","lang":"rust"}),
                "ast_search",
            ),
        ] {
            let encoded = pin(name, CatalogPreset::Head, false, false, None);
            let (wire, digest, semantics) = {
                let parsed = SchemaPin::parse(&encoded).unwrap();
                (
                    crate::tool_gate::canonical_tool_name(name).unwrap(),
                    parsed.schema_digest,
                    parsed.semantics,
                )
            };
            let mut spellings = vec![(name, encoded)];
            if wire != name {
                spellings.push((
                    wire,
                    SchemaPin::new(wire, &digest, semantics).encode().unwrap(),
                ));
            }
            for (spelling, encoded) in spellings {
                let frame = daemon
                    .call(
                        2,
                        corr,
                        request(
                            spelling,
                            "head",
                            args.clone(),
                            &format!("spelling-{corr}"),
                            encoded,
                        ),
                    )
                    .await;
                reply(frame);
                expected.push(command);
                assert_eq!(*READS.lock().unwrap(), expected, "{spelling}");
                corr += 1;
            }
        }
        let conflicts_pin =
            SchemaPin::parse(&pin("conflicts", CatalogPreset::Head, false, false, None)).unwrap();
        for name in ["conflicts", "aft_conflicts"] {
            refusal(
                daemon
                    .call(
                        2,
                        corr,
                        request(
                            name,
                            "head",
                            json!({}),
                            &format!("conflicts-{corr}"),
                            SchemaPin::new(
                                name,
                                &conflicts_pin.schema_digest,
                                conflicts_pin.semantics,
                            )
                            .encode()
                            .unwrap(),
                        ),
                    )
                    .await,
                "remote_tool_not_available",
            );
            corr += 1;
        }
        assert_eq!(*READS.lock().unwrap(), expected);
        assert_eq!(card_count.load(Ordering::Relaxed), 1);
        executor
            .actor_context(&root)
            .unwrap()
            .update_config(|config| {
                config.revision_reads = true;
            });
        let mut enabled_ref = read("head", "a.txt", "ref-on");
        enabled_ref["arguments"]["ref"] = json!("HEAD");
        refusal(
            daemon.call(2, corr, enabled_ref).await,
            "revision_reads_disabled",
        );
        assert_eq!(*READS.lock().unwrap(), expected);
        // Remote scope admission confirms liveness before validating the
        // request. An unverified session must not see pin validation errors.
        let mut unchecked = remote_bind(6, SCOPES[2], &[HELD, RESULT, YIELD]);
        unchecked["scope"]["ref"] = json!("unverified-session");
        reply(daemon.call(0, corr + 1, unchecked).await);
        daemon.describe_unsynced_once("unverified-session");
        malformed_pin["call_key"] = json!("unverified-malformed-pin");
        refusal(
            daemon.call(6, corr + 2, malformed_pin).await,
            "remote_call_not_admitted",
        );
        daemon.saw_probe("unverified-session").await;
        let corr = corr + 3;
        executor
            .actor_context(&root)
            .unwrap()
            .update_config(|config| {
                config.disabled_tools.push("read".into());
            });
        refusal(
            daemon
                .call(1, corr, read("reader", "a.txt", "disabled"))
                .await,
            "remote_tool_not_available",
        );
        assert_eq!(*READS.lock().unwrap(), expected);
        assert_eq!(card_count.load(Ordering::Relaxed), 1);
        daemon.signal(FrameType::Goodbye, 0, corr + 1).await;
    };
    let (end, ()) = tokio::join!(module, exercise);
    end.unwrap();
}

#[test]
fn remote_pins_match_runon_variant_without_widening_base_arguments() {
    for offer_runon in [false, true] {
        let request: cortexkit_role_tool_provider::call::ToolCallRequest =
            serde_json::from_value(request(
                "bash",
                "worker",
                json!({"command":"not-dispatched", "description":"Validate the remote pin", "runon":"linux"}),
                "pin-only",
                pin("bash", CatalogPreset::Head, offer_runon, false, None),
            ))
            .unwrap();
        let pin = tool_provider::remote_pins::parse(&request).unwrap();
        let result = tool_provider::remote_pins::validate(&request, &pin);
        if offer_runon {
            result.unwrap();
        } else {
            assert_eq!(result.unwrap_err().code, "invalid_request");
        }
    }
}

fn check_rows(db: &record::Db, folder: &Path) {
    let conn = db.lock().unwrap();
    let mut rows = conn.prepare("SELECT received_at_ms, peer_box_id, pairing_id, caller_id, tool, folder, mode, decision, outcome FROM remote_tool_audit ORDER BY rowid").unwrap();
    let rows = rows
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                [
                    r.get::<_, String>(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get::<_, Option<String>>(6)?.unwrap_or_default(),
                    r.get(7)?,
                    r.get(8)?,
                ],
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 17, "{rows:?}");
    for (time, fields) in &rows {
        assert!(*time > 0);
        assert_eq!(fields[0], "b".repeat(32));
        assert_eq!(fields[2], "remote-agent");
        assert_eq!(fields[4], folder.to_str().unwrap());
    }
    assert_eq!(&rows[0].1[5..], ["read", "refused", "invalid_request"]);
    for (_, fields) in &rows[2..6] {
        assert_eq!(fields[1], SCOPES[2]);
        assert_eq!(
            &fields[5..],
            ["read", "refused", "remote_tool_versions_differ"]
        );
    }
    assert_eq!(&rows[13].1[5..], ["read", "denied", "remote_access_denied"]);
    assert_eq!(&rows[14].1[5..], ["read", "saved_grant", "ok"]);
    assert_eq!(rows[14].1[1], SCOPES[0]);
    assert_eq!(&rows[15].1[5..], ["read", "saved_grant", "ok"]);
    assert_eq!(rows[15].1[1], SCOPES[1]);
    assert_eq!(&rows[16].1[5..], ["read", "replay", "ok"]);
}

#[path = "remote_edit_lifetime_tests.rs"]
mod edit_lifetime;
