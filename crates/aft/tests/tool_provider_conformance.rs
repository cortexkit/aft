//! Published conformance cases driven through the real aft --subc process.
use async_trait::async_trait;
use cortexkit_role_harness::{
    Harness, HarnessError, KillPoint, KillReport, PointDeclaration, RouteStamp, Trigger,
};
use cortexkit_role_tool_provider_conformance::{
    CallSpec, Capability, Exchange, ObservedFrame, RouteFailure, ScopedPrincipals,
    ToolProviderSubject, ToolRoute,
};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use subc_protocol::session::{ModuleControlRequest, ModuleControlResponse};
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, Principal, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_server,
    connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION},
    read_frame, write_frame,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Mutex as AsyncMutex,
};

#[path = "helpers/aft_binary.rs"]
mod aft_binary;

/// The provider under test, fetching its catalog with `preset` (`None` sends
/// no preset, which AFT serves as `head`).
struct Subject {
    preset: Option<&'static str>,
}
impl Subject {
    const HEAD: Subject = Subject { preset: None };
}
struct Process {
    child: Mutex<Child>,
    stream: Arc<AsyncMutex<Wire>>,
    root: PathBuf,
}
struct Wire {
    stream: TcpStream,
    next_corr: u64,
    next_channel: u16,
}
#[derive(Clone)]
struct Route {
    wire: Arc<AsyncMutex<Wire>>,
    channel: u16,
    scoped_preset: Option<&'static str>,
}
impl Drop for Process {
    fn drop(&mut self) {
        let child = self.child.get_mut().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}
fn flags() -> Flags {
    Flags::new(false, Priority::Interactive, false)
}
fn harness_error(e: impl std::fmt::Display) -> HarnessError {
    HarnessError::new(e.to_string())
}
async fn next_frame(stream: &mut TcpStream) -> Result<Frame, HarnessError> {
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), read_frame(stream))
            .await
            .map_err(harness_error)?
            .map_err(harness_error)?
            .ok_or_else(|| HarnessError::new("module EOF"))?;
        if frame.header.ty == FrameType::Ping {
            let pong = Frame::build(FrameType::Pong, flags(), 0, 0, frame.header.corr, vec![])
                .map_err(harness_error)?;
            write_frame(stream, &pong).await.map_err(harness_error)?;
            continue;
        }
        if frame.header.ty == FrameType::Push {
            continue;
        }
        return Ok(frame);
    }
}
fn prepare_provider_project(root: &Path) -> Result<(), HarnessError> {
    // Construct the config location from the same components on every platform.
    let directory = root.join("project").join(".cortexkit");
    std::fs::create_dir_all(&directory).map_err(|error| {
        HarnessError::new(format!(
            "creating provider config directory {}: {error}",
            directory.display()
        ))
    })?;
    let config = directory.join("aft.jsonc");
    std::fs::write(&config, serde_json::to_vec(&json!({"disabled_tools": ["aft_outline"], "indexes": {"callgraph": false, "trigram": false, "semantic": false}})).unwrap()).map_err(|error| HarnessError::new(format!("writing provider config {}: {error}", config.display())))?;
    Ok(())
}

fn provider_command(root: &Path, connection_path: &Path) -> Result<Command, HarnessError> {
    std::fs::create_dir_all(root).map_err(|error| {
        HarnessError::new(format!("creating provider cwd {}: {error}", root.display()))
    })?;
    let mut command = Command::new(aft_binary::aft_binary());
    // CreateProcess uses the parent's cwd when none is supplied. Always launch
    // inside this fixture, independently of the test runner's working directory.
    command.current_dir(root);
    for (variable, component) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        let directory = root.join(component);
        std::fs::create_dir_all(&directory).map_err(|error| {
            HarnessError::new(format!(
                "creating {variable} {}: {error}",
                directory.display()
            ))
        })?;
        command.env(variable, directory);
    }
    command
        .arg("--subc")
        .arg(connection_path)
        .env_remove("SUBC_MODULE_ID")
        .env_remove("SUBC_LAUNCH_NONCE")
        .env_remove("AFT_STORAGE_DIR")
        .env("AFT_TEST_DISABLE_FILE_WATCHER", "1")
        .env(
            "AFT_TEST_CALL_LEDGER_CONTROL",
            root.join("ledger-control.json"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

#[test]
fn provider_command_uses_existing_fixture_cwd_and_xdg_directories() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("not-yet-created");
    let command = provider_command(&root, &root.join("connection.json")).unwrap();
    assert_eq!(command.get_current_dir(), Some(root.as_path()));
    if let Some(binary) = std::env::var_os("NEXTEST_BIN_EXE_aft") {
        if std::env::var_os("AFT_TEST_AFT_BINARY").is_none() {
            assert_eq!(
                command.get_program(),
                binary,
                "use nextest's relocated executable"
            );
        }
    }
    assert!(root.is_dir());
    let directories: Vec<_> = command
        .get_envs()
        .filter(|(name, _)| name.to_string_lossy().starts_with("XDG_"))
        .collect();
    assert_eq!(directories.len(), 4);
    for (_, value) in directories {
        let path = Path::new(value.unwrap());
        assert!(path.is_dir(), "{} must exist before spawn", path.display());
        assert_eq!(path.parent(), Some(root.as_path()));
    }
    // --version exits without attaching, so this directly exercises OS process
    // creation with the fixture cwd and env rather than requiring a fake daemon.
    let mut probe = provider_command(&root, &root.join("connection.json")).unwrap();
    probe.args(["--version"]);
    assert!(probe.status().unwrap().success());
}

#[test]
fn provider_project_setup_accepts_absent_and_canonical_roots() {
    let dir = tempfile::tempdir().unwrap();
    let absent = dir.path().join("absent").join("main");
    prepare_provider_project(&absent).unwrap();
    let canonical = absent.canonicalize().unwrap();
    prepare_provider_project(&canonical).unwrap();
    let config: Value = serde_json::from_slice(
        &std::fs::read(
            canonical
                .join("project")
                .join(".cortexkit")
                .join("aft.jsonc"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(config["disabled_tools"], json!(["aft_outline"]));
    assert_eq!(config["indexes"]["callgraph"], false);
    assert!(canonical.join("project").join(".cortexkit").is_dir());
}

#[async_trait]
impl Harness for Subject {
    type Handle = Process;
    type Route = Route;
    fn declared_points(&self) -> Vec<PointDeclaration> {
        vec![]
    }
    async fn spawn(&self, root: &Path) -> Result<Process, HarnessError> {
        prepare_provider_project(root)?;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(harness_error)?;
        let connection = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: "127.0.0.1".into(),
                port: listener.local_addr().map_err(harness_error)?.port(),
            }],
            key: vec![0x42; subc_transport::KEY_LEN],
            daemon_id: [0x24; subc_transport::DAEMON_ID_LEN],
            pid: std::process::id(),
            daemon_ver: "tool-provider-conformance".into(),
        };
        let connection_path = root.join("connection.json");
        connection_file::write_atomic(&connection_path, &connection).map_err(|error| {
            HarnessError::new(format!(
                "publishing connection file {}: {error}",
                connection_path.display()
            ))
        })?;
        let mut command = provider_command(root, &connection_path)?;
        let child = command.spawn().map_err(|error| {
            HarnessError::new(format!(
                "launching {} with --subc {} in cwd {} (exe_exists={}, cwd_exists={}): {error}",
                command.get_program().to_string_lossy(),
                connection_path.display(),
                root.display(),
                Path::new(command.get_program()).is_file(),
                root.is_dir()
            ))
        })?;
        // Own the child before awaiting the handshake so failed setup also reaps it.
        struct ChildGuard(Option<Child>);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                if let Some(child) = &mut self.0 {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let mut guard = ChildGuard(Some(child));
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
            .await
            .map_err(harness_error)?
            .map_err(harness_error)?;
        authenticate_server(
            &mut stream,
            &[0x42; subc_transport::KEY_LEN],
            &[0x24; subc_transport::DAEMON_ID_LEN],
            "tool-provider-conformance",
            Duration::from_secs(5),
        )
        .await
        .map_err(harness_error)?;
        let hello = next_frame(&mut stream).await?;
        if hello.header.ty != FrameType::Hello {
            return Err(HarnessError::new("expected module Hello"));
        }
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: vec![],
            subc_capabilities: vec![],
            storage: None,
            machine_id: None,
        };
        write_frame(
            &mut stream,
            &Frame::build(
                FrameType::HelloAck,
                flags(),
                0,
                0,
                hello.header.corr,
                serde_json::to_vec(&ack).unwrap(),
            )
            .map_err(harness_error)?,
        )
        .await
        .map_err(harness_error)?;
        Ok(Process {
            child: Mutex::new(guard.0.take().unwrap()),
            stream: Arc::new(AsyncMutex::new(Wire {
                stream,
                next_corr: 100,
                next_channel: 1,
            })),
            root: root.to_path_buf(),
        })
    }
    async fn route(&self, handle: &Process, stamp: &RouteStamp) -> Result<Route, HarnessError> {
        let mut route = bind_route_stamped(
            handle,
            &handle.root.join("project"),
            "runner",
            "conformance-session",
            Some(BTreeMap::from([("tool-provider".into(), "v1".into())])),
            stamp,
        )
        .await?;
        route.scoped_preset = stamp.scope.as_ref().map(|_| self.preset.unwrap_or("head"));
        Ok(route)
    }
    async fn kill_at(
        &self,
        handle: Process,
        point: &KillPoint,
        trigger: Trigger<'_>,
    ) -> Result<KillReport, HarnessError> {
        #[cfg(not(feature = "test-timing-hooks"))]
        {
            let _ = (handle, point, trigger);
            Err(HarnessError::new("kill hooks require test-timing-hooks"))
        }
        #[cfg(feature = "test-timing-hooks")]
        {
            let name = if point == &KillPoint::new("Prepared") {
                "Prepared"
            } else if point == &KillPoint::new("Authorized") {
                "Authorized"
            } else {
                return Err(HarnessError::new("unsupported kill point"));
            };
            let signal = handle.root.join("ledger-signal");
            let _ = std::fs::remove_file(&signal);
            std::fs::write(
                handle.root.join("ledger-control.json"),
                serde_json::to_vec(&json!({"point":name,"signal_path":signal})).unwrap(),
            )
            .map_err(harness_error)?;
            let reached = async {
                tokio::time::timeout(Duration::from_secs(30), async {
                    while !signal.exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .map_err(harness_error)
            };
            tokio::pin!(trigger);
            tokio::select! {
                result = reached => result?,
                _ = &mut trigger => return Err(HarnessError::new("trigger ended before durable kill point")),
            }
            let mut child = handle.child.lock().unwrap();
            child.kill().map_err(harness_error)?;
            child.wait().map_err(harness_error)?;
            Ok(KillReport {
                point: point.clone(),
                mechanism: cortexkit_role_harness::KillMechanism::FaultHookThenProcessKill,
            })
        }
    }
    async fn restart(&self, root: &Path) -> Result<Process, HarnessError> {
        let _ = std::fs::remove_file(root.join("ledger-control.json"));
        self.spawn(root).await
    }
}

fn ledger_database(root: &Path) -> PathBuf {
    fn visit(path: &Path) -> Option<PathBuf> {
        for entry in std::fs::read_dir(path).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                if let Some(found) = visit(&path) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|name| name == "aft.db") {
                return Some(path);
            }
        }
        None
    }
    visit(root).expect("fixture database exists")
}

fn scoped_stamp(carrier: &str, owner: &str, scope_ref: &str) -> RouteStamp {
    RouteStamp {
        principal: format!("reserved:{carrier}"),
        scope: Some(cortexkit_role_harness::ScopeStamp {
            owner: format!("reserved:{owner}"),
            scope_ref: scope_ref.into(),
            scope_epoch: 7,
        }),
    }
}

fn marker_command(marker: &Path) -> String {
    format!(
        "printf ran > '{}'",
        marker.display().to_string().replace('\'', "'\\''")
    )
}

fn response_json(frame: &Frame) -> Value {
    serde_json::from_slice(&frame.body).unwrap()
}

#[tokio::test]
async fn s1_keyed_replay_headers_bytes_expiry_and_keyless_legacy_exclusion() {
    use aft::db::call_ledger::{self as ledger, Key};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    let route = Subject::HEAD
        .route(&process, &Subject::HEAD.plain_stamp())
        .await
        .unwrap();
    std::fs::write(root.join("project/input.txt"), "first content\n").unwrap();
    let body = json!({"name":"read","arguments":{"filePath":"input.txt"},"call_key":"replay"});
    let first = route.raw(body.clone(), false).await;
    assert_eq!(first.header.ty, FrameType::Response);
    assert_eq!(response_json(&first)["isError"], false);
    std::fs::write(root.join("project/input.txt"), "changed content\n").unwrap();
    let repeat = route.raw(body.clone(), false).await;
    assert_ne!(first.header.corr, repeat.header.corr);
    assert_eq!(first.header.ty, repeat.header.ty);
    assert_eq!(first.body, repeat.body);
    let other = Subject::HEAD
        .route(&process, &Subject::HEAD.plain_stamp())
        .await
        .unwrap();
    let repeat = other.raw(body.clone(), false).await;
    assert_ne!(first.header.channel, repeat.header.channel);
    assert_eq!(first.body, repeat.body);
    let conflict = route
        .raw(
            json!({"name":"read","arguments":{"filePath":"missing.txt"},"call_key":"replay"}),
            false,
        )
        .await;
    assert_eq!(conflict.header.ty, FrameType::Error);
    assert_eq!(response_json(&conflict)["code"], "invalid_request");
    assert_eq!(response_json(&conflict)["detail"]["field"], "call_key");
    let plain = route
        .raw(
            json!({"name":"read","arguments":{"filePath":"input.txt"}}),
            false,
        )
        .await;
    assert!(String::from_utf8_lossy(&plain.body).contains("changed content"));
    let legacy = bind_route_declaring(&process, &root.join("project"), "runner", "legacy", None)
        .await
        .unwrap();
    assert_eq!(
        legacy
            .raw(
                json!({"name":"read","arguments":{"filePath":"input.txt"},"call_key":"legacy"}),
                false
            )
            .await
            .header
            .ty,
        FrameType::Response
    );
    drop(process);
    // Direct database assertions and forced clock changes are offline: tests
    // never open another SQLite handle while the actor owns this database.
    let path = ledger_database(&root);
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM call_ledger", [], |r| r
            .get::<_, usize>(0))
            .unwrap(),
        1
    );
    let key = Key {
        carrier: "direct".into(),
        call_key: "replay".into(),
    };
    let row = ledger::get(&conn, &key).unwrap().unwrap();
    assert!(row.scope.is_none());
    assert!(row.late_entry.is_none());
    assert_eq!(row.frame.unwrap().body, first.body);
    conn.execute(
        "UPDATE call_ledger SET settled_at=?1",
        [ledger::now_ms() - ledger::RETENTION_MS],
    )
    .unwrap();
    // An unscoped row expires by deletion, so expiry replay is covered on a
    // scoped row below rather than confusing deletion with retained identity.
    drop(conn);
    let process = Subject::HEAD.restart(&root).await.unwrap();
    let route = Subject::HEAD
        .route(&process, &Subject::HEAD.plain_stamp())
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&route.raw(body, false).await.body).contains("changed content"));
}

#[tokio::test]
async fn s1_scoped_owner_identity_conflicts_and_expired_replay() {
    use aft::db::call_ledger::{self as ledger, Key};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    std::fs::write(root.join("project/input.txt"), "content\n").unwrap();
    let stamp = scoped_stamp("carrier-a", "owner-a", "same-ref");
    let route = Subject::HEAD.route(&process, &stamp).await.unwrap();
    let body = json!({"name":"read","arguments":{"filePath":"input.txt"},"call_key":"scoped"});
    let first = route.raw(body.clone(), false).await;
    assert_eq!(response_json(&first)["isError"], false);
    let other = Subject::HEAD.route(&process, &stamp).await.unwrap();
    let repeat = other.raw(body.clone(), false).await;
    assert_eq!(first.body, repeat.body);
    assert_eq!(first.header.ty, repeat.header.ty);
    for conflict_stamp in [
        scoped_stamp("carrier-a", "owner-b", "same-ref"),
        scoped_stamp("carrier-a", "owner-a", "different-ref"),
        RouteStamp {
            scope: Some(cortexkit_role_harness::ScopeStamp {
                scope_epoch: 8,
                ..stamp.scope.clone().unwrap()
            }),
            ..stamp.clone()
        },
        RouteStamp {
            principal: stamp.principal.clone(),
            scope: None,
        },
    ] {
        let other = Subject::HEAD
            .route(&process, &conflict_stamp)
            .await
            .unwrap();
        let refused = other.raw(body.clone(), false).await;
        assert_eq!(refused.header.ty, FrameType::Error);
        assert_eq!(response_json(&refused)["detail"]["field"], "call_key");
    }
    let stamp_b = scoped_stamp("carrier-b", "owner-b", "same-ref");
    let other = Subject::HEAD.route(&process, &stamp_b).await.unwrap();
    assert_eq!(
        response_json(&other.raw(body.clone(), false).await)["isError"],
        false
    );
    let trusted = Subject::HEAD
        .route(&process, &Subject::HEAD.plain_stamp())
        .await
        .unwrap();
    let missing = json!({"name":"read","arguments":{"filePath":"missing.txt"}});
    let native = response_json(&trusted.raw(missing.clone(), false).await)["structuredContent"]
        ["code"]
        .as_str()
        .unwrap()
        .to_string();
    let mut missing_keyed = missing;
    missing_keyed["call_key"] = json!("error-replay");
    let error = route.raw(missing_keyed.clone(), false).await;
    assert_eq!(response_json(&error)["isError"], true);
    assert!(response_json(&error).get("structuredContent").is_none());
    drop(process);
    let path = ledger_database(&root);
    let conn = rusqlite::Connection::open(&path).unwrap();
    for (carrier, owner) in [("carrier-a", "owner-a"), ("carrier-b", "owner-b")] {
        let row = ledger::get(
            &conn,
            &Key {
                carrier: format!("reserved:{carrier}"),
                call_key: "scoped".into(),
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(row.scope.unwrap().owner, format!("reserved:{owner}"));
        assert_eq!(
            serde_json::from_str::<Value>(row.late_entry.as_deref().unwrap()).unwrap()["owner"],
            format!("reserved:{owner}")
        );
    }
    conn.execute(
        "UPDATE call_ledger SET settled_at=?1",
        [ledger::now_ms() - ledger::RETENTION_MS],
    )
    .unwrap();
    drop(conn);
    let process = Subject::HEAD.restart(&root).await.unwrap();
    let route = Subject::HEAD.route(&process, &stamp).await.unwrap();
    let expired = route.raw(body, false).await;
    assert_eq!(expired.header.ty, FrameType::Error);
    let expired = response_json(&expired);
    assert_eq!(expired["code"], "result_not_retained");
    assert_eq!(expired["detail"]["outcome"], "ok");
    let expired_error = response_json(&route.raw(missing_keyed, false).await);
    assert_eq!(expired_error["detail"]["outcome"], native);
}

#[tokio::test]
async fn s1_database_unavailable_refuses_keyed_read_and_shell_but_not_keyless_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    let route = Subject::HEAD
        .route(&process, &Subject::HEAD.plain_stamp())
        .await
        .unwrap();
    std::fs::write(root.join("project/input.txt"), "readable\n").unwrap();
    route
        .raw(
            json!({"name":"read","arguments":{"filePath":"input.txt"},"call_key":"seed"}),
            false,
        )
        .await;
    drop(process);
    let path = ledger_database(&root);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute("DELETE FROM call_ledger", []).unwrap();
    conn.execute(
        "UPDATE schema_version SET version=?1",
        [aft::db::CURRENT_SCHEMA_VERSION + 1],
    )
    .unwrap();
    drop(conn);
    let process = Subject::HEAD.restart(&root).await.unwrap();
    let route = Subject::HEAD
        .route(&process, &Subject::HEAD.plain_stamp())
        .await
        .unwrap();
    let marker = root.join("project/should-not-run");
    for body in [
        json!({"name":"read","arguments":{"filePath":"input.txt"},"call_key":"blocked-read"}),
        json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":"blocked-bash"}),
    ] {
        let frame = route.raw(body, false).await;
        let response = response_json(&frame);
        assert_eq!(
            response["structuredContent"]["code"], "database_unavailable",
            "{response}"
        );
        // The persistence gate retries opening on the next call. Refusal occurs
        // before durable admission or execution, so retrying cannot duplicate work.
        assert_eq!(response["structuredContent"]["retryable"], true);
    }
    let plain = route
        .raw(
            json!({"name":"read","arguments":{"filePath":"input.txt"}}),
            false,
        )
        .await;
    assert!(String::from_utf8_lossy(&plain.body).contains("readable"));
    assert!(!marker.exists());
    drop(process);
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM call_ledger", [], |r| r
            .get::<_, usize>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn s1_keyless_untrusted_shell_keeps_slice_a_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    let route = Subject::HEAD
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    let marker = root.join("project/keyless-marker");
    let reply = route
        .raw(
            json!({"name":"bash","arguments":{"command":marker_command(&marker)}}),
            true,
        )
        .await;
    assert_eq!(reply.header.ty, FrameType::Error);
    assert_eq!(response_json(&reply)["code"], "capability_not_admitted");
    assert!(!marker.exists());
}

#[tokio::test]
async fn s1_untrusted_keyed_shell_without_elicitation_is_refused_at_admission() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    let route = bind_route_stamped_with_elicitation(
        &process,
        &root.join("project"),
        "runner",
        "conformance-session",
        Some(BTreeMap::from([("tool-provider".into(), "v1".into())])),
        &scoped_stamp("carrier", "owner", "scope"),
        false,
    )
    .await
    .unwrap();
    let marker = root.join("project/no-elicitation-marker");
    let reply = route
        .raw(
            json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":"no-elicitation"}),
            false,
        )
        .await;
    assert_eq!(reply.header.ty, FrameType::Error);
    assert_eq!(response_json(&reply)["code"], "capability_not_admitted");
    assert!(!marker.exists());
    let conn = rusqlite::Connection::open(ledger_database(&root)).unwrap();
    let executions: usize = conn
        .query_row("SELECT COUNT(*) FROM bash_tasks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(executions, 0);
    let admissions: usize = conn
        .query_row("SELECT COUNT(*) FROM call_ledger", [], |row| row.get(0))
        .unwrap();
    assert_eq!(admissions, 0);
}

#[tokio::test]
async fn s1_untrusted_keyed_shell_denial_and_elicitation_errors_never_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    let warm = Subject::HEAD
        .route(&process, &Subject::HEAD.plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"read","arguments":{"path":"input.txt"}}),
        false,
    )
    .await;
    let route = Subject::HEAD
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    for (case, ty, body) in [
        (
            "deny",
            FrameType::Response,
            br#"{"decision":"deny"}"#.to_vec(),
        ),
        ("malformed", FrameType::Response, b"not JSON".to_vec()),
        (
            "failed",
            FrameType::Error,
            br#"{"code":"elicitation_failed"}"#.to_vec(),
        ),
    ] {
        let marker = root.join(format!("project/{case}-marker"));
        let mut call =
            json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":case});
        route.adapt_preset(&mut call);
        let mut wire = route.wire.lock().await;
        let corr = wire.next_corr;
        wire.next_corr += 1;
        write_frame(
            &mut wire.stream,
            &Frame::build(
                FrameType::Request,
                flags(),
                route.channel,
                1,
                corr,
                serde_json::to_vec(&call).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let ask = next_frame(&mut wire.stream).await.unwrap();
        assert_eq!(ask.header.ty, FrameType::Request, "{case}: {ask:?}");
        assert_eq!(ask.header.channel, route.channel);
        assert!(!marker.exists(), "{case}: execution must wait for a grant");
        write_frame(
            &mut wire.stream,
            &Frame::build(ty, flags(), route.channel, 1, ask.header.corr, body).unwrap(),
        )
        .await
        .unwrap();
        let reply = next_frame(&mut wire.stream).await.unwrap();
        assert_eq!(reply.header.corr, corr, "{case}");
        assert_eq!(reply.header.ty, FrameType::Response, "{case}");
        assert_eq!(response_json(&reply)["isError"], true, "{case}");
        assert!(
            !marker.exists(),
            "{case}: refusal must never spawn the shell"
        );
    }
    let conn = rusqlite::Connection::open(ledger_database(&root)).unwrap();
    let executions: usize = conn
        .query_row("SELECT COUNT(*) FROM bash_tasks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(executions, 0);
    let refusals: usize = conn
        .query_row("SELECT COUNT(*) FROM call_ledger WHERE state = 'Settled' AND outcome = 'bash_denied_untrusted'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(refusals, 3);
}

#[tokio::test]
async fn s1_cancelled_prepared_repeat_ends_without_settling_the_execution() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    let route = Subject::HEAD
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    let warm = Subject::HEAD
        .route(&process, &Subject::HEAD.plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"bash","arguments":{"command":"printf warm"}}),
        false,
    )
    .await;
    let marker = root.join("project/cancelled-repeat");
    let mut body = json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":"cancelled-repeat"});
    route.adapt_preset(&mut body);
    let mut wire = route.wire.lock().await;
    let first = wire.next_corr;
    let repeat = first + 1;
    wire.next_corr += 2;
    write_frame(
        &mut wire.stream,
        &Frame::build(
            FrameType::Request,
            flags(),
            route.channel,
            1,
            first,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let ask = next_frame(&mut wire.stream).await.unwrap();
    assert_eq!(ask.header.ty, FrameType::Request);
    write_frame(
        &mut wire.stream,
        &Frame::build(
            FrameType::Request,
            flags(),
            route.channel,
            1,
            repeat,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    write_frame(
        &mut wire.stream,
        &Frame::build(FrameType::Cancel, flags(), route.channel, 1, repeat, vec![]).unwrap(),
    )
    .await
    .unwrap();
    let cancelled = tokio::time::timeout(Duration::from_secs(5), next_frame(&mut wire.stream))
        .await
        .expect("a cancelled attachment must receive a terminal frame")
        .unwrap();
    assert_eq!(cancelled.header.corr, repeat);
    assert_eq!(cancelled.header.ty, FrameType::Error);
    assert_eq!(response_json(&cancelled)["code"], "cancelled");
    assert!(!marker.exists());
    write_frame(
        &mut wire.stream,
        &Frame::build(
            FrameType::Response,
            flags(),
            route.channel,
            1,
            ask.header.corr,
            serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let completed = next_frame(&mut wire.stream).await.unwrap();
    assert_eq!(completed.header.corr, first);
    assert_eq!(completed.header.ty, FrameType::Response);
    assert_eq!(response_json(&completed)["isError"], false);
    assert!(
        tokio::time::timeout(Duration::from_millis(400), next_frame(&mut wire.stream))
            .await
            .is_err(),
        "the cancelled repeat must not also receive the result"
    );
    drop(wire);
    let replay = route.raw(body, false).await;
    assert_eq!(replay.body, completed.body);
    assert_eq!(std::fs::read(marker).unwrap(), b"ran");
}

#[tokio::test]
async fn s1_prepared_and_running_repeats_attach_without_second_question_or_execution() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    let route = Subject::HEAD
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    let warm = Subject::HEAD
        .route(&process, &Subject::HEAD.plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"bash","arguments":{"command":"printf warm"}}),
        false,
    )
    .await;
    let marker = root.join("project/executions");
    let mut body = json!({"name":"bash","arguments":{"command":format!("printf x >> '{}'; sleep 1",marker.display())},"call_key":"held-repeat"});
    route.adapt_preset(&mut body);
    let mut wire = route.wire.lock().await;
    let first = wire.next_corr;
    wire.next_corr += 1;
    let send = |corr| {
        Frame::build(
            FrameType::Request,
            flags(),
            route.channel,
            1,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap()
    };
    write_frame(&mut wire.stream, &send(first)).await.unwrap();
    let ask = next_frame(&mut wire.stream).await.unwrap();
    assert_eq!(
        ask.header.ty,
        FrameType::Request,
        "{}",
        String::from_utf8_lossy(&ask.body)
    );
    let held = wire.next_corr;
    wire.next_corr += 1;
    write_frame(&mut wire.stream, &send(held)).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(150), next_frame(&mut wire.stream))
            .await
            .is_err(),
        "a Prepared repeat must not file a second question"
    );
    write_frame(
        &mut wire.stream,
        &Frame::build(
            FrameType::Response,
            flags(),
            route.channel,
            1,
            ask.header.corr,
            serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let running = wire.next_corr;
    wire.next_corr += 1;
    write_frame(&mut wire.stream, &send(running)).await.unwrap();
    let mut replies = BTreeMap::new();
    for _ in 0..3 {
        let frame = next_frame(&mut wire.stream).await.unwrap();
        assert_eq!(frame.header.ty, FrameType::Response);
        assert_eq!(response_json(&frame)["isError"], false);
        replies.insert(frame.header.corr, frame.body);
    }
    assert_eq!(replies.len(), 3);
    assert_eq!(replies[&first], replies[&held]);
    assert_eq!(replies[&first], replies[&running]);
    assert_eq!(std::fs::read(&marker).unwrap(), b"x");
    drop(wire);
    drop(process);
    let conn = rusqlite::Connection::open(ledger_database(&root)).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM call_ledger WHERE late_entry IS NOT NULL",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn s1_cross_actor_prepared_repeat_does_not_run_startup_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    let stamp = scoped_stamp("carrier", "owner", "scope");
    let route = Subject::HEAD.route(&process, &stamp).await.unwrap();
    let warm = Subject::HEAD
        .route(&process, &Subject::HEAD.plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"bash","arguments":{"command":"printf warm"}}),
        false,
    )
    .await;
    let marker = root.join("project/cross-actor");
    let mut body = json!({"name":"bash","arguments":{"command":format!("printf x >> '{}'",marker.display())},"call_key":"cross-actor"});
    route.adapt_preset(&mut body);
    let mut wire = route.wire.lock().await;
    let first = wire.next_corr;
    wire.next_corr += 1;
    write_frame(
        &mut wire.stream,
        &Frame::build(
            FrameType::Request,
            flags(),
            route.channel,
            1,
            first,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let ask = next_frame(&mut wire.stream).await.unwrap();
    assert_eq!(ask.header.ty, FrameType::Request);
    drop(wire);
    let second_root = root.join("project2");
    std::fs::create_dir_all(second_root.join(".cortexkit")).unwrap();
    std::fs::copy(
        root.join("project/.cortexkit/aft.jsonc"),
        second_root.join(".cortexkit/aft.jsonc"),
    )
    .unwrap();
    let second_warm = bind_route(&process, &second_root, "runner", "second-root")
        .await
        .unwrap();
    second_warm
        .raw(
            json!({"name":"bash","arguments":{"command":"printf warm"}}),
            false,
        )
        .await;
    let second = bind_route_stamped(
        &process,
        &second_root,
        "runner",
        "conformance-session",
        Some(BTreeMap::from([("tool-provider".into(), "v1".into())])),
        &stamp,
    )
    .await
    .unwrap();
    let mut wire = route.wire.lock().await;
    let repeat = wire.next_corr;
    wire.next_corr += 1;
    write_frame(
        &mut wire.stream,
        &Frame::build(
            FrameType::Request,
            flags(),
            second.channel,
            1,
            repeat,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    write_frame(
        &mut wire.stream,
        &Frame::build(
            FrameType::Response,
            flags(),
            route.channel,
            1,
            ask.header.corr,
            serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let mut replies = BTreeMap::new();
    for _ in 0..2 {
        let frame = next_frame(&mut wire.stream).await.unwrap();
        assert_eq!(
            frame.header.ty,
            FrameType::Response,
            "{}",
            String::from_utf8_lossy(&frame.body)
        );
        replies.insert(frame.header.corr, frame.body);
    }
    assert_eq!(replies[&first], replies[&repeat]);
    assert_eq!(std::fs::read(marker).unwrap(), b"x");
}

#[cfg(feature = "test-timing-hooks")]
async fn crash_before_dispatch(point: &str, shell: &str) {
    use aft::db::call_ledger::{self as ledger, Key, State};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    let stamp = scoped_stamp("carrier", "owner", "scope");
    let route = Subject::HEAD.route(&process, &stamp).await.unwrap();
    let marker = root.join("project/crash-marker");
    let command = if shell == "powershell" {
        format!(
            "Set-Content -LiteralPath '{}' -Value ran",
            marker.display().to_string().replace('\'', "''")
        )
    } else {
        marker_command(&marker)
    };
    let body = json!({"name":shell,"arguments":{"command":command},"call_key":"crash"});
    let trigger: Trigger<'_> = Box::pin(async {
        let frame = route.raw(body, true).await;
        panic!(
            "call returned before kill point: {}",
            String::from_utf8_lossy(&frame.body)
        );
    });
    let report = Subject::HEAD
        .kill_at(process, &KillPoint::new(point), trigger)
        .await
        .unwrap();
    assert_eq!(report.point, KillPoint::new(point));
    assert_eq!(
        report.mechanism,
        cortexkit_role_harness::KillMechanism::FaultHookThenProcessKill
    );
    let path = ledger_database(&root);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let key = Key {
        carrier: "reserved:carrier".into(),
        call_key: "crash".into(),
    };
    let row = ledger::get(&conn, &key).unwrap().unwrap();
    assert_eq!(
        row.state,
        if point == "Prepared" {
            State::Prepared
        } else {
            State::Authorized
        }
    );
    assert!(row.task_id.is_none());
    drop(conn);
    let process = Subject::HEAD.restart(&root).await.unwrap();
    let route = Subject::HEAD.route(&process, &stamp).await.unwrap();
    let replay = route
        .raw(
            json!({"name":shell,"arguments":{"command":command},"call_key":"crash"}),
            false,
        )
        .await;
    assert_eq!(replay.header.ty, FrameType::Error);
    assert_eq!(
        response_json(&replay)["detail"]["reason"],
        "restart_before_dispatch"
    );
    assert!(!marker.exists());
    drop(process);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let row = ledger::get(&conn, &key).unwrap().unwrap();
    assert_eq!(row.state, State::Settled);
    assert_eq!(row.seq, Some(1));
    let entry: Value = serde_json::from_str(row.late_entry.as_deref().unwrap()).unwrap();
    assert_eq!(entry["kind"], "not_started");
    assert_eq!(entry["reason"], "restart_before_dispatch");
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM call_ledger WHERE seq IS NOT NULL",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        1
    );
}

#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn s1_crash_at_prepared_restarts_not_started() {
    crash_before_dispatch("Prepared", "bash").await;
}
#[cfg(feature = "test-timing-hooks")]
#[tokio::test]
async fn s1_crash_at_authorized_restarts_not_started() {
    crash_before_dispatch("Authorized", "bash").await;
}
#[cfg(all(windows, feature = "test-timing-hooks"))]
#[tokio::test]
async fn s1_powershell_crash_at_prepared_restarts_not_started() {
    crash_before_dispatch("Prepared", "powershell").await;
}

#[tokio::test]
async fn s1_unarmed_shell_commits_both_held_states_and_replays_before_injections() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    let route = Subject::HEAD
        .route(&process, &scoped_stamp("carrier", "owner", "scope"))
        .await
        .unwrap();
    let marker = root.join("project/marker");
    assert!(!root.join("ledger-control.json").exists());
    let body =
        json!({"name":"bash","arguments":{"command":marker_command(&marker)},"call_key":"unarmed"});
    let first = route.raw(body.clone(), true).await;
    assert_eq!(
        response_json(&first)["isError"],
        false,
        "{}",
        String::from_utf8_lossy(&first.body)
    );
    assert!(marker.exists());
    std::fs::remove_file(&marker).unwrap();
    let repeat = route.raw(body, false).await;
    assert_eq!(first.body, repeat.body);
    assert!(!marker.exists());
}

#[tokio::test]
async fn s1_restart_recovers_running_shell_once_and_reduces_unobserved_read() {
    use aft::db::call_ledger::{self as ledger, Key, ScopeIdentity, State};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("module");
    let process = Subject::HEAD.spawn(&root).await.unwrap();
    let stamp = scoped_stamp("carrier", "owner", "scope");
    let route = Subject::HEAD.route(&process, &stamp).await.unwrap();
    let warm = Subject::HEAD
        .route(&process, &Subject::HEAD.plain_stamp())
        .await
        .unwrap();
    warm.raw(
        json!({"name":"bash","arguments":{"command":"printf warm"}}),
        false,
    )
    .await;
    let started = root.join("project/started");
    let finished = root.join("project/finished");
    let command = format!(
        "{}; sleep 2; printf recovered-output; {}",
        marker_command(&started),
        marker_command(&finished)
    );
    let mut body =
        json!({"name":"bash","arguments":{"command":command},"call_key":"recover-shell"});
    route.adapt_preset(&mut body);
    let mut wire = route.wire.lock().await;
    let corr = wire.next_corr;
    wire.next_corr += 1;
    write_frame(
        &mut wire.stream,
        &Frame::build(
            FrameType::Request,
            flags(),
            route.channel,
            1,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let ask = next_frame(&mut wire.stream).await.unwrap();
    assert_eq!(ask.header.ty, FrameType::Request);
    write_frame(
        &mut wire.stream,
        &Frame::build(
            FrameType::Response,
            flags(),
            route.channel,
            1,
            ask.header.corr,
            serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
        )
        .unwrap(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !started.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    drop(wire);
    drop(process);
    let path = ledger_database(&root);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let shell_key = Key {
        carrier: "reserved:carrier".into(),
        call_key: "recover-shell".into(),
    };
    assert_eq!(
        ledger::get(&conn, &shell_key).unwrap().unwrap().state,
        State::DispatchStarted
    );
    let read_key = Key {
        carrier: "reserved:reader".into(),
        call_key: "unknown-read".into(),
    };
    let read_body = json!({"name":"read","schema_pin":null,"arguments":{"filePath":"missing.txt"}});
    let digest = cortexkit_role_tool_provider::catalog::composition_digest(&read_body).unwrap();
    ledger::admit(
        &conn,
        &read_key,
        &digest,
        Some(&ScopeIdentity {
            owner: "reserved:owner".into(),
            scope_ref: "scope".into(),
            scope_epoch: 7,
        }),
        State::DispatchStarted,
    )
    .unwrap();
    drop(conn);
    let process = Subject::HEAD.restart(&root).await.unwrap();
    let route = Subject::HEAD.route(&process, &stamp).await.unwrap();
    let response = route.raw(body, false).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response_json(&response)["isError"], false);
    assert!(
        response_json(&response)["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("recovered-output"),
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    assert!(finished.exists());
    drop(process);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let shell = ledger::get(&conn, &shell_key).unwrap().unwrap();
    assert_eq!(shell.state, State::Settled);
    assert_eq!(
        serde_json::from_str::<Value>(shell.late_entry.as_deref().unwrap()).unwrap()["kind"],
        "result"
    );
    let read = ledger::get(&conn, &read_key).unwrap().unwrap();
    assert_eq!(read.state, State::Settled);
    assert_eq!(read.outcome.as_deref(), Some("unknown"));
    assert_eq!(
        serde_json::from_str::<Value>(read.late_entry.as_deref().unwrap()).unwrap()["outcome"],
        "unknown"
    );
    assert_eq!(
        conn.query_row(
            "SELECT COUNT(*) FROM call_ledger WHERE late_entry IS NOT NULL",
            [],
            |r| r.get::<_, usize>(0)
        )
        .unwrap(),
        2
    );
}
/// Bind a tool-provider v1 route, the way a consumer that declared
/// `role_versions: {"tool-provider": "v1"}` on route open reaches AFT.
async fn bind_route(
    handle: &Process,
    project: &Path,
    harness: &str,
    session: &str,
) -> Result<Route, HarnessError> {
    bind_route_declaring(
        handle,
        project,
        harness,
        session,
        Some(BTreeMap::from([("tool-provider".into(), "v1".into())])),
    )
    .await
}
async fn bind_route_declaring(
    handle: &Process,
    project: &Path,
    harness: &str,
    session: &str,
    role_versions: Option<BTreeMap<String, String>>,
) -> Result<Route, HarnessError> {
    bind_route_stamped(
        handle,
        project,
        harness,
        session,
        role_versions,
        &RouteStamp {
            principal: "direct".into(),
            scope: None,
        },
    )
    .await
}

async fn bind_route_stamped(
    handle: &Process,
    project: &Path,
    harness: &str,
    session: &str,
    role_versions: Option<BTreeMap<String, String>>,
    stamp: &RouteStamp,
) -> Result<Route, HarnessError> {
    bind_route_stamped_with_elicitation(
        handle,
        project,
        harness,
        session,
        role_versions,
        stamp,
        stamp.scope.is_some(),
    )
    .await
}

async fn bind_route_stamped_with_elicitation(
    handle: &Process,
    project: &Path,
    harness: &str,
    session: &str,
    role_versions: Option<BTreeMap<String, String>>,
    stamp: &RouteStamp,
    elicitation: bool,
) -> Result<Route, HarnessError> {
    let mut wire = handle.stream.lock().await;
    let channel = wire.next_channel;
    wire.next_channel += 1;
    let corr = wire.next_corr;
    wire.next_corr += 1;
    let bind = ModuleControlRequest::RouteBind {
        route_channel: channel,
        epoch: 1,
        target: RouteTarget::ToolProvider {
            module_id: "aft".into(),
        },
        identity: BindIdentity::new(project, harness, session),
        principal: Some(decode_principal(&stamp.principal)?),
        consumer_capabilities: elicitation.then(|| vec!["elicitation".into()]),
        admission_facts: None,
        scope: stamp
            .scope
            .as_ref()
            .map(|scope| {
                Ok(subc_protocol::scope::ScopeStamp {
                    owner: decode_principal(&scope.owner)?,
                    scope_ref: scope.scope_ref.clone(),
                    scope_epoch: scope.scope_epoch,
                    kind: subc_protocol::scope::ScopeKind::Head,
                    parent: None,
                    parent_state: None,
                    attributes: Default::default(),
                    owner_authorized: true,
                })
            })
            .transpose()?,
        role_versions,
    };
    write_frame(
        &mut wire.stream,
        &Frame::build(
            FrameType::Request,
            flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&bind).unwrap(),
        )
        .map_err(harness_error)?,
    )
    .await
    .map_err(harness_error)?;
    let ack = next_frame(&mut wire.stream).await?;
    if ack.header.ty != FrameType::Response || ack.header.corr != corr {
        return Err(HarnessError::new(format!(
            "unexpected bind frame: {:?} {}",
            ack.header.ty,
            String::from_utf8_lossy(&ack.body)
        )));
    }
    let response: ModuleControlResponse =
        serde_json::from_slice(&ack.body).map_err(harness_error)?;
    if response != (ModuleControlResponse::RouteBindAck {}) {
        return Err(HarnessError::new("no bind acknowledgement"));
    }
    Ok(Route {
        wire: handle.stream.clone(),
        channel,
        scoped_preset: stamp.scope.as_ref().map(|_| "head"),
    })
}
fn decode_principal(principal: &str) -> Result<Principal, HarnessError> {
    match principal {
        "direct" => Ok(Principal::Direct),
        "unverified" => Ok(Principal::Unverified),
        _ => principal
            .strip_prefix("reserved:")
            .map(|module_id| Principal::Reserved {
                module_id: module_id.into(),
            })
            .ok_or_else(|| HarnessError::new("invalid principal spelling")),
    }
}
impl Route {
    async fn exchange(&self, body: Value, cancel: bool) -> Result<Exchange, RouteFailure> {
        self.exchange_inner(body, cancel)
            .await
            .map_err(|error| RouteFailure::new(error.to_string()))
    }
    async fn exchange_inner(
        &self,
        mut body: Value,
        cancel: bool,
    ) -> Result<Exchange, HarnessError> {
        self.adapt_preset(&mut body);
        let mut wire = self.wire.lock().await;
        let corr = wire.next_corr;
        wire.next_corr += 1;
        write_frame(
            &mut wire.stream,
            &Frame::build(
                FrameType::Request,
                flags(),
                self.channel,
                1,
                corr,
                serde_json::to_vec(&body).unwrap(),
            )
            .map_err(harness_error)?,
        )
        .await
        .map_err(harness_error)?;
        if cancel {
            tokio::time::sleep(Duration::from_millis(100)).await;
            write_frame(
                &mut wire.stream,
                &Frame::build(FrameType::Cancel, flags(), self.channel, 1, corr, vec![])
                    .map_err(harness_error)?,
            )
            .await
            .map_err(harness_error)?;
        }
        let mut exchange = Exchange::default();
        let mut terminal = false;
        loop {
            let frame = if terminal {
                match tokio::time::timeout(Duration::from_millis(100), next_frame(&mut wire.stream))
                    .await
                {
                    Ok(frame) => frame?,
                    Err(_) => break,
                }
            } else {
                next_frame(&mut wire.stream).await?
            };
            if frame.header.channel != self.channel || frame.header.corr != corr {
                return Err(HarnessError::new(
                    "unexpected correlation during conformance exchange",
                ));
            }
            let observed = match frame.header.ty {
                FrameType::Response => ObservedFrame::Response(
                    serde_json::from_slice(&frame.body).map_err(harness_error)?,
                ),
                FrameType::Error => ObservedFrame::Error(
                    serde_json::from_slice(&frame.body).map_err(harness_error)?,
                ),
                FrameType::StreamEnd => ObservedFrame::StreamEnd,
                _ => {
                    ObservedFrame::Data(serde_json::from_slice(&frame.body).map_err(harness_error)?)
                }
            };
            terminal |= observed.is_terminal();
            exchange.frames.push(observed);
        }
        Ok(exchange)
    }

    fn adapt_preset(&self, body: &mut Value) {
        // The published runner omits presets on scoped calls. Adapt only tool
        // calls, not role operations; production admission still requires one.
        if let Some(preset) = self.scoped_preset {
            let name = body["name"].as_str().unwrap_or_default();
            if !matches!(
                name,
                "role.describe"
                    | "tool.catalog"
                    | "tool.withdraw"
                    | "late_results"
                    | "late_results.ack"
            ) && body.get("name").is_some()
            {
                body.as_object_mut()
                    .unwrap()
                    .entry("preset")
                    .or_insert(json!(preset));
            }
        }
    }

    async fn raw(&self, mut body: Value, approve: bool) -> Frame {
        self.adapt_preset(&mut body);
        let mut wire = self.wire.lock().await;
        for _ in 0..100 {
            let corr = wire.next_corr;
            wire.next_corr += 1;
            write_frame(
                &mut wire.stream,
                &Frame::build(
                    FrameType::Request,
                    flags(),
                    self.channel,
                    1,
                    corr,
                    serde_json::to_vec(&body).unwrap(),
                )
                .unwrap(),
            )
            .await
            .unwrap();
            loop {
                let frame = next_frame(&mut wire.stream).await.unwrap();
                if frame.header.ty == FrameType::Request && approve {
                    write_frame(
                        &mut wire.stream,
                        &Frame::build(
                            FrameType::Response,
                            flags(),
                            self.channel,
                            1,
                            frame.header.corr,
                            serde_json::to_vec(&json!({"decision":"allow"})).unwrap(),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
                    continue;
                }
                assert_eq!(frame.header.channel, self.channel);
                assert_eq!(frame.header.corr, corr);
                assert!(matches!(
                    frame.header.ty,
                    FrameType::Response | FrameType::Error | FrameType::StreamEnd
                ));
                if frame.header.ty == FrameType::Response
                    && (response_json(&frame)["structuredContent"]["code"]
                        == "database_initializing"
                        || String::from_utf8_lossy(&frame.body)
                            .contains("Project persistence is still initializing"))
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    break;
                }
                return frame;
            }
        }
        panic!("database initialization never completed");
    }
}
#[async_trait]
impl ToolRoute for Route {
    async fn request(&self, body: Value) -> Result<Exchange, RouteFailure> {
        self.exchange(body, false).await
    }
    async fn request_then_cancel(&self, body: Value) -> Result<Exchange, RouteFailure> {
        self.exchange(body, true).await
    }
}
#[async_trait]
impl ToolProviderSubject for Subject {
    fn capabilities(&self) -> BTreeSet<Capability> {
        BTreeSet::from([
            Capability::DisableTool,
            Capability::Cancellation,
            Capability::CallKey,
            Capability::SchemaPin,
            Capability::SystemText,
        ])
    }
    fn plain_stamp(&self) -> RouteStamp {
        RouteStamp {
            principal: "direct".into(),
            scope: None,
        }
    }
    fn scoped_principals(&self) -> Option<ScopedPrincipals> {
        None
    }
    fn catalog_arguments(&self) -> Value {
        match self.preset {
            Some(preset) => json!({ "preset": preset }),
            None => json!({}),
        }
    }
    fn system_text_catalog_arguments(&self) -> Option<Value> {
        let preset = self.preset.unwrap_or("head");
        Some(json!({
            "preset": preset,
            "system_text": {"preset": preset, "params": {}}
        }))
    }
    fn quick_call(&self) -> CallSpec {
        // The quick call's tool must be in the catalog under test, and the
        // reader preset serves no `status`.
        if self.preset == Some("reader") {
            ("glob".into(), json!({"pattern": "*"}))
        } else {
            ("status".into(), json!({}))
        }
    }
    fn slow_call(&self) -> Option<CallSpec> {
        Some((
            "bash".into(),
            json!({"command": "sleep 5", "wait": true, "timeout": 5000}),
        ))
    }
    fn disabled_tool(&self) -> Option<String> {
        Some("outline".into())
    }
    fn held_call(&self, _marker: &Path) -> Option<CallSpec> {
        None
    }
    async fn await_held(&self, _key: &str) -> Result<(), HarnessError> {
        Err(HarnessError::new("not declared in Slice A"))
    }
    async fn approve(&self, _key: &str) -> Result<(), HarnessError> {
        Err(HarnessError::new("not declared in Slice A"))
    }
    async fn settle(&self) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn slice_a_real_module_conformance_inventory() {
    conformance_inventory(Subject::HEAD).await;
}

/// The commons suite holds for every catalog preset AFT serves: each one is a
/// complete catalog, refuses an undefined preset, and admits its own quick
/// call and schema pins.
#[tokio::test]
async fn every_catalog_preset_passes_the_conformance_suite() {
    for preset in ["head", "worker", "reader"] {
        eprintln!("preset {preset}:");
        conformance_inventory(Subject {
            preset: Some(preset),
        })
        .await;
    }
}

async fn conformance_inventory(subject: Subject) {
    use cortexkit_role_tool_provider_conformance::CaseOutcome;
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/tool_provider_conformance.json")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let report =
        cortexkit_role_tool_provider_conformance::run_suite(&subject, &dir.path().join("run"))
            .await
            .unwrap();
    let mut enabled = BTreeSet::new();
    let mut skipped = BTreeSet::new();
    let mut failed = BTreeSet::new();
    for case in &report.cases {
        match &case.outcome {
            CaseOutcome::Skipped { .. } => {
                skipped.insert(case.case.to_string());
            }
            CaseOutcome::Passed => {
                enabled.insert(case.case.to_string());
            }
            CaseOutcome::Failed { reason } => {
                enabled.insert(case.case.to_string());
                failed.insert(case.case.to_string());
                eprintln!(
                    "FAIL {}: {}",
                    case.case,
                    reason.chars().take(200).collect::<String>()
                );
            }
        }
    }
    let expected = |field| {
        serde_json::from_value::<BTreeSet<String>>(fixtures["slice_a"][field].clone()).unwrap()
    };
    assert_eq!(enabled, expected("enabled_cases"));
    assert_eq!(skipped, expected("skipped_cases"));
    // Every route is bound with role_versions {"tool-provider": "v1"}, so the
    // real module admits these calls under the v1 grammar. Every enabled case
    // must pass; the provider conforms for the capabilities it declares and
    // the skipped ones remain undeclared.
    assert!(failed.is_empty(), "failed conformance cases: {failed:?}");
    eprintln!(
        "v1 binds: {} passed, {} failed, {} capability skips",
        enabled.len() - failed.len(),
        failed.len(),
        skipped.len()
    );
    assert!(
        matches!(
            report.verdict,
            cortexkit_role_tool_provider_conformance::SuiteVerdict::ConformingForDeclaredCapabilities { .. }
        ),
        "{:?}",
        report.verdict
    );
}

#[test]
fn registry_manifest_lock_fixture_equalities_and_independent_declaration() {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/tool_provider_conformance.json")).unwrap();
    let manifest: toml::Value = toml::from_str(include_str!("../Cargo.toml")).unwrap();
    let lock: toml::Value = toml::from_str(include_str!("../../../Cargo.lock")).unwrap();
    for artifact in fixtures["registry_artifacts"].as_array().unwrap() {
        let name = artifact["name"].as_str().unwrap();
        let section = if artifact["dependency_kind"] == "normal" {
            "dependencies"
        } else {
            "dev-dependencies"
        };
        assert_eq!(
            manifest[section][name].as_str().unwrap(),
            format!("={}", artifact["version"].as_str().unwrap())
        );
        let package = lock["package"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"].as_str() == Some(name))
            .unwrap();
        for field in ["version", "source", "checksum"] {
            assert_eq!(
                package[field].as_str().unwrap(),
                artifact[field].as_str().unwrap()
            );
        }
        assert!(!artifact.as_object().unwrap().contains_key("sha"));
    }
    assert_eq!(
        fixtures["slice_a"]["declaration"]["majors"][0]["ops"],
        json!(["role.describe", "tool.catalog", "tool.call"])
    );
}

async fn catalog_reply(route: &Route, request: Value) -> Value {
    let exchange = route
        .request(json!({"name":"tool.catalog","arguments":request}))
        .await
        .unwrap();
    match cortexkit_role_tool_provider_conformance::single_terminal(&exchange).unwrap() {
        ObservedFrame::Response(reply) => reply.clone(),
        other => panic!("catalog refused: {other:?}"),
    }
}

#[tokio::test]
async fn real_module_project_harness_matrix_rebind_and_restart_identity() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let process = Subject::HEAD.spawn(&state).await.unwrap();
    std::fs::create_dir_all(state.join("config/cortexkit")).unwrap();
    std::fs::write(state.join("config/cortexkit/aft.jsonc"), serde_json::to_vec(&json!({"disabled_tools": [], "harnesses": {"runner": {"disabled_tools": ["aft_outline"]}, "opencode": {"disabled_tools": ["aft_search"]}}})).unwrap()).unwrap();
    let p1 = state.join("project");
    let p2 = state.join("project2");
    std::fs::create_dir_all(p2.join(".cortexkit")).unwrap();
    let project_config = |disabled: Vec<&str>| {
        serde_json::to_vec(&json!({"disabled_tools": disabled, "indexes": {"callgraph": false, "trigram": false, "semantic": false}})).unwrap()
    };
    std::fs::write(p1.join(".cortexkit/aft.jsonc"), project_config(vec![])).unwrap();
    std::fs::write(
        p2.join(".cortexkit/aft.jsonc"),
        project_config(vec!["aft_inspect"]),
    )
    .unwrap();
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/tool_provider_catalog.json")).unwrap();
    let request = json!({"system_text":{"preset":"broca","params":{"worker":false}}});
    let mut routes = vec![];
    for (root, harness, fixture) in [
        (&p1, "runner", "p1_runner"),
        (&p1, "opencode", "p1_opencode"),
        (&p2, "runner", "p2_runner"),
        (&p2, "opencode", "p2_opencode"),
    ] {
        let route = bind_route(&process, root, harness, fixture).await.unwrap();
        let reply = catalog_reply(&route, request.clone()).await;
        let available = reply["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "powershell");
        let name = format!("{fixture}{}", if available { "_pwsh" } else { "_no_pwsh" });
        let expected = fixtures
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .unwrap();
        assert!(
            serde_json::to_vec(&reply).unwrap() == serde_json::to_vec(&expected["reply"]).unwrap(),
            "{name} full bytes differ"
        );
        routes.push((route, reply));
    }
    std::fs::write(
        p1.join(".cortexkit/aft.jsonc"),
        project_config(vec!["aft_zoom"]),
    )
    .unwrap();
    for (route, reply) in &routes {
        assert_eq!(&catalog_reply(route, request.clone()).await, reply);
    }
    for (harness, fixture) in [
        ("runner", "p1_runner_rebound"),
        ("opencode", "p1_opencode_rebound"),
    ] {
        let route = bind_route(&process, &p1, harness, fixture).await.unwrap();
        let reply = catalog_reply(&route, request.clone()).await;
        let available = reply["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "powershell");
        let name = format!("{fixture}{}", if available { "_pwsh" } else { "_no_pwsh" });
        let expected = fixtures
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == name)
            .unwrap();
        assert_eq!(reply, expected["reply"]);
    }
    for (route, reply) in &routes {
        assert_eq!(&catalog_reply(route, request.clone()).await, reply);
    }
    std::fs::write(p1.join(".cortexkit/aft.jsonc"), project_config(vec![])).unwrap();
    let restored = bind_route(&process, &p1, "runner", "restored")
        .await
        .unwrap();
    assert_eq!(catalog_reply(&restored, request.clone()).await, routes[0].1);
    drop(routes);
    drop(restored);
    drop(process);
    let restarted = Subject::HEAD.restart(&state).await.unwrap();
    std::fs::write(p1.join(".cortexkit/aft.jsonc"), project_config(vec![])).unwrap();
    let route = bind_route(&restarted, &p1, "runner", "restart")
        .await
        .unwrap();
    let reply = catalog_reply(&route, request).await;
    let available = reply["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["name"] == "powershell");
    let expected = fixtures
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == format!("p1_runner{}", if available { "_pwsh" } else { "_no_pwsh" }))
        .unwrap();
    assert_eq!(reply, expected["reply"]);
    assert!(bind_route(&restarted, &p1, "broca", "unsupported-harness")
        .await
        .is_err());
    // A tool-provider version AFT does not serve refuses the bind and names
    // the versions it does serve, instead of falling back to a legacy route.
    let refused = bind_route_declaring(
        &restarted,
        &p1,
        "runner",
        "unsupported-version",
        Some(BTreeMap::from([("tool-provider".into(), "v2".into())])),
    )
    .await
    .err()
    .expect("an unserved tool-provider version refuses the bind")
    .to_string();
    assert!(
        refused.contains("unsupported_role_version") && refused.contains("[\"v1\"]"),
        "{refused}"
    );
}
