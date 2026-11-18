use super::*;
use crate::subc::test_support::{bind_config_without_indexes, route_identity, test_root};
use cortexkit_project_identity::RootIdentity;
use std::sync::atomic::AtomicUsize;

const REMOTE: &str = "fed:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

pub(super) fn stamp(claim: Value) -> Value {
    json!({
        "owner": {"kind":"reserved", "module_id":"callosum"},
        "ref":"remote-session", "kind":"federated", "scope_epoch":1,
        "workdir":"relative-on-A", "owner_authorized":false,
        "attributes": {
            "agent_id":"not-the-remote-caller",
            "federated": {
                "peer_class":"computer", "peer_fingerprint":"a".repeat(32),
                "peer_box_id":"b".repeat(32), "peer_name":"Computer A", "pairing_id":"pairing-A",
                "origin_principal":{"kind":"reserved", "module_id":"llm-runner"},
                "project":{"project_id":"B-project", "root_key":{"kind":"label", "key":"B-root"}},
                "remote_claim":claim,
            }
        }
    })
}

pub(super) fn registry(root: &Path) -> EnumerateReply {
    serde_json::from_value(json!({
        "workspaces": [], "generation": 1, "incarnation": "local-registry",
        "projects": [{
            "projectId": "B-project", "name": "B's project", "roots": [root],
            "implicit": false, "refKind": "local",
            "rootRecords": [{
                "root": root, "remotes": [], "rootKey": {"kind":"label", "rootKey":"B-root"},
                "identity":"unbound", "approval":{"state":"unapproved"}
            }]
        }]
    }))
    .unwrap()
}

fn bind(channel: u16, harness: &str, scope: Value, legacy: &Path, target: &str) -> Value {
    json!({
        "op": "route.bind", "route_channel":channel, "epoch":1,
        "target":{"kind":target, "module_id":"aft"},
        "principal":{"kind":"reserved", "module_id":"callosum"},
        "identity":{"harness":harness, "session":format!("session-{channel}"), "project_root":legacy},
        "role_versions":{"tool-provider":"v1"}, "scope":scope,
    })
}

async fn rpc(stream: &mut tokio::io::DuplexStream, channel: u16, corr: u64, body: Value) -> Frame {
    let epoch = u32::from(channel != 0);
    write_frame(
        stream,
        &Frame::build(
            FrameType::Request,
            control_flags(),
            channel,
            epoch,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let frame = read_frame(stream).await.unwrap().unwrap();
            if frame.header.corr == corr {
                return frame;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no reply for correlation {corr}: {body}"))
}

async fn call(
    stream: &mut tokio::io::DuplexStream,
    channel: u16,
    corr: u64,
    tool: &str,
) -> ErrorBody {
    let frame = rpc(stream, channel, corr,
        json!({"op":"tool.call", "name":tool, "preset":"head", "arguments":{}, "call_key":format!("remote-{corr}")})).await;
    assert_eq!(frame.header.ty, FrameType::Error);
    serde_json::from_slice(&frame.body).unwrap()
}

#[tokio::test]
async fn real_binds_refuse_every_call_without_opening_a_workdir() {
    let _env = crate::test_env::process_env_lock();
    let _comparison = scope_workdir_tests::observe_next_comparison();
    let (storage, root) = test_root("remote-call-folder-on-b");
    let lookups = Arc::new(AtomicUsize::new(0));
    let registry = Arc::new(StdMutex::new(EnumerateReply::new(
        vec![],
        vec![],
        0,
        "empty".into(),
    )));
    let fixture = {
        let registry = Arc::clone(&registry);
        let lookups = Arc::clone(&lookups);
        Arc::new(move || {
            lookups.fetch_add(1, Ordering::Relaxed);
            registry.lock().unwrap().clone()
        })
    };
    let executor = Arc::new(Executor::new());
    let app = App::default_shared();
    let (mut daemon, module) = tokio::io::duplex(1024 * 1024);
    let (read, write) = tokio::io::split(module);
    let connection = storage.path().join("connection.json");
    let module = TEST_REGISTRY.scope(
        fixture,
        run_module_loop(
            read,
            write,
            &connection,
            app.clone(),
            executor.clone(),
            |request, ctx| {
                assert_eq!(request.command, "configure", "remote tool reached dispatch");
                crate::commands::configure::handle_configure(&request, ctx)
            },
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
        let hello = read_frame(&mut daemon).await.unwrap().unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        assert!(!String::from_utf8_lossy(&hello.body).contains("scope-federated/v1"));
        write_frame(
            &mut daemon,
            &Frame::build(
                FrameType::HelloAck,
                control_flags(),
                0,
                0,
                HELLO_CORR,
                b"{}".to_vec(),
            )
            .unwrap(),
        )
        .await
        .unwrap();

        let agent = stamp(json!({"kind":"agent", "agent_id":"remote-agent"}));
        let mut cases = vec![
            (agent.clone(), true),
            (
                stamp(json!({"kind":"run", "agent_id":"remote-agent"})),
                true,
            ),
            (
                stamp(json!({"kind":"worker", "owner_module_id":"worker-owner",
                "worker_ref":"c".repeat(64), "on_behalf_of":"remote-agent"})),
                true,
            ),
            (
                stamp(json!({"kind":"worker", "owner_module_id":"worker-owner",
                "worker_ref":"c".repeat(64)})),
                false,
            ),
            (
                stamp(json!({"kind":"flow", "flow_id":"flow", "flow_version":1,
                "flow_install_id":"install", "flow_code_hash":"d".repeat(64)})),
                false,
            ),
            (stamp(json!({"kind":"future"})), false),
            (stamp(json!({"kind":"agent"})), false),
            (Value::Null, false),
            (json!({"undecodable":true}), false),
        ];
        let mut wrong_kind = agent.clone();
        wrong_kind["kind"] = json!("head");
        cases.push((wrong_kind, false));
        for field in [
            "peer_class",
            "peer_box_id",
            "pairing_id",
            "project",
            "origin_principal",
            "remote_claim",
        ] {
            let mut missing = agent.clone();
            missing["attributes"]["federated"]
                .as_object_mut()
                .unwrap()
                .remove(field)
                .unwrap();
            cases.push((missing, false));
        }
        for class in ["app", "phone", "sim", "unknown"] {
            let mut other_peer = agent.clone();
            other_peer["attributes"]["federated"]["peer_class"] = json!(class);
            cases.push((other_peer, false));
        }
        let mut direct = agent.clone();
        direct["attributes"]["federated"]["origin_principal"] = json!({"kind":"direct"});
        cases.push((direct, false));

        for (index, (scope, admissible)) in cases.into_iter().enumerate() {
            if admissible {
                assert_eq!(
                    admit(Some(&serde_json::from_value(scope.clone()).unwrap()))
                        .unwrap()
                        .1,
                    "remote-agent"
                );
            }
            let channel = u16::try_from(index + 1).unwrap();
            let corr = u64::from(channel) * 2;
            let mut bind = bind(channel, REMOTE, scope, root.as_path(), "tool_provider");
            if index == 7 {
                bind.as_object_mut().unwrap().remove("scope");
            }
            let ack = rpc(&mut daemon, 0, corr, bind).await;
            assert_eq!(
                ack.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&ack.body)
            );
            let error = call(&mut daemon, channel, corr + 1, "read").await;
            assert_eq!(
                error.code,
                if admissible {
                    "remote_folder_not_registered"
                } else {
                    "remote_call_not_admitted"
                }
            );
        }
        assert_eq!(lookups.load(Ordering::Relaxed), 3);
        assert_eq!(executor.actor_count(), 0);
        assert_eq!(app.watcher_count(), 0);
        assert!(
            scope_workdir_tests::take_comparison_observer().is_some(),
            "remote bind must not schedule the stamped/legacy workdir comparison"
        );

        let local_with_stamp = rpc(
            &mut daemon,
            0,
            97,
            bind(89, "opencode", agent, root.as_path(), "tool_provider"),
        )
        .await;
        assert_eq!(local_with_stamp.header.ty, FrameType::Error);
        assert_eq!(
            serde_json::from_slice::<ErrorBody>(&local_with_stamp.body)
                .unwrap()
                .code,
            "invalid_workdir"
        );

        // The matched registry root needs a live local bind, not just a remote route.
        *registry.lock().unwrap() = self::registry(root.as_path());
        assert_eq!(
            call(&mut daemon, 1, 101, "write").await.code,
            "remote_folder_not_working"
        );
        let mut local = bind(90, "opencode", Value::Null, root.as_path(), "tool_provider");
        local["principal"] = json!({"kind":"direct"});
        let ack = rpc(&mut daemon, 0, 103, local).await;
        assert_eq!(
            ack.header.ty,
            FrameType::Response,
            "{}",
            String::from_utf8_lossy(&ack.body)
        );
        for (corr, tool) in [(105, "read"), (107, "write"), (109, "bash")] {
            let error = call(&mut daemon, 1, corr, tool).await;
            assert_eq!(error.code, "remote_tool_not_available");
            assert_eq!(
                error.message,
                "This tool isn't available to agents on other computers."
            );
        }

        for (index, harness) in ["fed-phone:person", "fed-sim:person", "fed-app:person"]
            .into_iter()
            .enumerate()
        {
            let channel = 91 + u16::try_from(index).unwrap();
            let corr = 200 + 4 * u64::from(channel);
            let ack = rpc(
                &mut daemon,
                0,
                corr,
                bind(
                    channel,
                    harness,
                    Value::Null,
                    Path::new("/"),
                    "management_surface",
                ),
            )
            .await;
            assert_eq!(
                ack.header.ty,
                FrameType::Response,
                "{}",
                String::from_utf8_lossy(&ack.body)
            );
            assert_eq!(
                rpc(
                    &mut daemon,
                    channel,
                    corr + 1,
                    json!({"method":"shells.list"})
                )
                .await
                .header
                .ty,
                FrameType::Response
            );
            assert_eq!(
                call(&mut daemon, channel, corr + 2, "read").await.code,
                "route_not_admitted"
            );
        }
        write_frame(
            &mut daemon,
            &Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 1000, vec![]).unwrap(),
        )
        .await
        .unwrap();
    };
    let (end, ()) = tokio::join!(Box::pin(module), exercise);
    end.unwrap();
}

#[test]
fn registry_selection_counts_only_usable_records_matching_both_keys() {
    let scope: ScopeStamp =
        serde_json::from_value(stamp(json!({"kind":"agent","agent_id":"agent"}))).unwrap();
    let (project, _) = admit(Some(&scope)).unwrap();
    let mut entries = registry(Path::new("registered"));
    let record = entries.projects[0].root_records.as_ref().unwrap()[0].clone();
    for unusable in [
        {
            let mut r = record.clone();
            r.root_key_mismatch = Some(true);
            r
        },
        {
            let mut r = record.clone();
            r.root_key = None;
            r
        },
        {
            let mut r = record.clone();
            r.identity = RootIdentity::Replaced;
            r
        },
        {
            let mut r = record.clone();
            r.identity = RootIdentity::Unverifiable;
            r
        },
        {
            let mut r = record.clone();
            r.identity = RootIdentity::Retired;
            r
        },
        {
            let mut r = record.clone();
            r.root_key.as_mut().unwrap().kind = RegistryKeyKind::Remote;
            r
        },
        {
            let mut r = record.clone();
            r.root_key.as_mut().unwrap().root_key = "other".into();
            r
        },
    ] {
        entries.projects[0].root_records = Some(vec![unusable.clone()]);
        assert_eq!(
            registered_root(project, &entries).unwrap_err().code,
            "remote_folder_not_registered"
        );
        entries.projects[0].root_records = Some(vec![unusable, record.clone()]);
        assert_eq!(
            registered_root(project, &entries).unwrap(),
            Path::new("registered")
        );
    }
    let mut wrong_project = entries.projects[0].clone();
    wrong_project.project_id = "other-project".into();
    wrong_project.root_records = Some(vec![record.clone()]);
    entries.projects[0].root_records = Some(vec![]);
    entries.projects.push(wrong_project);
    assert_eq!(
        registered_root(project, &entries).unwrap_err().code,
        "remote_folder_not_registered"
    );
    let mut other = record.clone();
    other.identity = RootIdentity::Bound;
    other.root = "also-registered".into();
    entries.projects[0].root_records = Some(vec![other.clone(), record]);
    assert_eq!(
        registered_root(project, &entries).unwrap_err().code,
        "remote_folder_not_registered"
    );
    entries.projects[0].root_records = Some(vec![other]);
    assert_eq!(
        registered_root(project, &entries).unwrap(),
        Path::new("also-registered")
    );
}

#[test]
fn a_remote_channel_in_root_channels_does_not_supply_liveness() {
    let (_dir, root) = test_root("remote-channel-not-local-liveness");
    let mut remote = route_identity(&root, "remote-channel");
    Arc::get_mut(&mut remote.0).unwrap().harness = REMOTE.into();
    let mut routes = HashMap::new();
    let mut root_channels = HashMap::new();
    installed_routes::insert_route_channel(
        &mut routes,
        &mut root_channels,
        route_key(1, 1),
        remote,
    );
    let bound = |routes: &HashMap<_, _>, roots: &HashMap<_, _>| {
        locally_bound_root(root.as_path(), roots, routes).map(|_| ())
    };
    assert_eq!(
        bound(&routes, &root_channels).unwrap_err().code,
        "remote_folder_not_working"
    );
    installed_routes::insert_route_channel(
        &mut routes,
        &mut root_channels,
        route_key(2, 1),
        route_identity(&root, "local-channel"),
    );
    assert!(bound(&routes, &root_channels).is_ok());
    installed_routes::remove_route_channel(&mut routes, &mut root_channels, route_key(2, 1));
    assert_eq!(
        bound(&routes, &root_channels).unwrap_err().code,
        "remote_folder_not_working"
    );
    installed_routes::remove_route_channel(&mut routes, &mut root_channels, route_key(1, 1));
}
