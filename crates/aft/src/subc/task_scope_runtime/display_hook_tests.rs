use super::*;
use crate::late_result_bus::{self, display, views};
use cortexkit_role_tool_display::DisplayKind;
use subc_transport::connection_file::{self, ConnectionInfo, Endpoint};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Registered,
    StartingBus,
    StartedDisplay,
}

type StartupObserver = (
    mpsc::UnboundedSender<SubcLifecycleEvent>,
    mpsc::UnboundedSender<Step>,
);
static OBSERVER: StdMutex<Option<StartupObserver>> = StdMutex::new(None);

pub(super) fn observe(probe: Option<&SubcTestLifecycleProbe>, step: Step) {
    if let Some((invocation, order)) = OBSERVER.lock().unwrap().as_ref() {
        if probe.is_some_and(|probe| probe.events_tx.same_channel(invocation)) {
            order.send(step).expect("startup order receiver");
        }
    }
}

#[derive(Default)]
struct ReadyEvents(StdMutex<Vec<(String, Vec<u8>)>>);

impl display::ReadyPublisher for ReadyEvents {
    async fn ready(&self, id: &str, body: Vec<u8>) -> Result<(), &'static str> {
        // BusClient uses this argument as Nats-Msg-Id, separately from the payload.
        self.0.lock().unwrap().push((id.to_owned(), body));
        Ok(())
    }
}

#[derive(Default)]
struct ViewWrites(Vec<(String, Vec<u8>)>);

impl views::Writer for ViewWrites {
    async fn put(&mut self, key: &str, value: Vec<u8>) -> Result<(), &'static str> {
        self.0.push((key.to_owned(), value));
        Ok(())
    }
}

#[test]
fn daemon_startup_hooks_publish_display_and_shipped_views() {
    const CHILD: &str = "AFT_TEST_DAEMON_DISPLAY_HOOK_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let test = concat!(
            module_path!(),
            "::daemon_startup_hooks_publish_display_and_shipped_views"
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test.split_once("::").unwrap().1, "--nocapture"])
            .env(CHILD, "1")
            .env("SUBC_MODULE_ID", "aft")
            .env("AFT_TOOL_DISPLAY", "1")
            .env_remove("AFT_TEST_SETTINGS_SCHEMA")
            .output()
            .unwrap();
        assert!(
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    let _launch = crate::launch_nonce::install_for_tests(Some("daemon-display-hook-test"));
    let root = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let storage = root.path().join("storage");
    let connection = root.path().join("connection.json");
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let key = vec![0x42; subc_transport::KEY_LEN];
    let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
    const VERSION: &str = "0.39.0";
    connection_file::write_atomic(
        &connection,
        &ConnectionInfo {
            schema: connection_file::SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
            }],
            key: key.clone(),
            daemon_id,
            pid: std::process::id(),
            daemon_ver: VERSION.to_owned(),
        },
    )
    .unwrap();
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::context_storage::isolate(crate::config::Config {
            storage_dir: Some(storage.clone()),
            ..Default::default()
        }),
    ));
    let storage = ctx.storage_dir();
    let (lifecycle, _events) = mpsc::unbounded_channel();
    let probe = SubcTestLifecycleProbe::new(lifecycle.clone());
    let (order, mut recorded) = mpsc::unbounded_channel();
    *OBSERVER.lock().unwrap() = Some((lifecycle, order));

    let daemon = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            tokio::time::timeout(Duration::from_secs(15), async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let (mut stream, _) = listener.accept().await.unwrap();
                subc_transport::authenticate_server(
                    &mut stream,
                    &key,
                    &daemon_id,
                    VERSION,
                    Duration::from_secs(5),
                )
                .await
                .unwrap();
                let hello = crate::subc_fake::read_frame(&mut stream)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(hello.header.ty, FrameType::Hello);
                let ack = Frame::build(
                    FrameType::HelloAck,
                    control_flags(),
                    0,
                    0,
                    hello.header.corr,
                    serde_json::to_vec(&ModuleHelloAckBody {
                        negotiated_ver: PROTOCOL_VERSION,
                        subc_ops: Vec::new(),
                        subc_capabilities: Vec::new(),
                        storage: None,
                        machine_id: None,
                    })
                    .unwrap(),
                )
                .unwrap();
                subc_transport::write_frame(&mut stream, &ack)
                    .await
                    .unwrap();

                for step in [Step::Registered, Step::StartingBus, Step::StartedDisplay] {
                    assert_eq!(recorded.recv().await, Some(step));
                }
                assert_publications(&storage).await;

                let goodbye =
                    Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 99, Vec::new())
                        .unwrap();
                subc_transport::write_frame(&mut stream, &goodbye)
                    .await
                    .unwrap();
                stream.shutdown().await.unwrap();
                // Keep reading until module teardown closes this end of the session.
                let mut drain = Vec::new();
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut drain).await;
            })
            .await
            .expect("display hook daemon test timed out");
        });
    });
    let result = run_subc_mode_with_probe(
        &connection,
        ctx,
        Arc::new(Executor::with_config(Default::default())),
        |request, _| Response::success(request.id, serde_json::json!({})),
        Some(root.path().join("no-user-config.jsonc")),
        Some(probe),
        None,
    );
    daemon.join().unwrap();
    result.unwrap();
}

async fn assert_publications(storage: &Path) {
    use crate::db::display_records;
    use rich_ui::outbox::{ceilings, Identity};

    let pin = tool_views::SHIPPED.first_key_value().unwrap().0;
    let (id, expected) = rich_ui::display::with_outbox(|outbox| {
        let now = Instant::now();
        assert!(outbox.admit(
            Identity {
                runner: Principal::Reserved {
                    module_id: "runner".to_owned()
                },
                call_key: "display-hook:startup".to_owned(),
                schema_pin: pin.clone(),
            },
            DisplayKind::Detail,
            serde_json::json!({"nested": {"text": "frozen pull output"}}),
            &ceilings(),
            now,
        ));
        let id = outbox.reads.due(now).unwrap();
        let bytes = outbox.reads.read(&id, now).unwrap();
        (id, bytes)
    })
    .expect("daemon startup installed the display writer");
    assert!(!expected.is_empty());

    let hooks = late_result_bus::hooks().expect("daemon startup registered the hooks");
    let ready = ReadyEvents::default();
    display::publish_one(&ready, Some(hooks)).await;
    let body = display::body(&id).unwrap();
    assert_ne!(id.as_bytes(), body);
    assert_eq!(*ready.0.lock().unwrap(), vec![(id.clone(), body)]);
    assert!(
        (hooks.take_due_read)().is_none(),
        "mark published cleared the ready wake"
    );

    let retained = crate::db::open(&storage.join("aft.db")).unwrap();
    let actual = display_records::read(&retained, &id, display_records::now_ms()).unwrap();
    assert_eq!(actual, Some(expected));

    let mut writer = ViewWrites::default();
    views::Publication::default()
        .sync_with_hooks(&mut writer, Some(hooks))
        .await
        .unwrap();
    let (_, bytes) = tool_views::bus_snapshot()
        .into_iter()
        .find(|(candidate, _)| candidate == pin)
        .unwrap();
    assert!(writer.0.contains(&(views::key(pin), bytes)));
}
