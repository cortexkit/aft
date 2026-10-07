//! Isolated macOS allocation experiment; see docs/investigations/daemon-malloc-small-2026-09.md.
//! The broker speaks the production subc transport to a separate AFT child.
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use subc_protocol::session::ModuleControlRequest;
use subc_protocol::{
    BindIdentity, Flags, Frame, FrameType, ModuleHelloAckBody, Principal, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_server,
    connection_file::{self, ConnectionInfo, Endpoint, SCHEMA_VERSION},
    read_frame, write_frame,
};
use tokio::net::{TcpListener, TcpStream};

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn request(stream: &mut TcpStream, channel: u16, corr: &mut u64, body: Value, out: &Path) {
    *corr += 1;
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        channel,
        u32::from(channel != 0),
        *corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
    let response = tokio::time::timeout(Duration::from_secs(600), async {
        loop {
            let f = read_frame(stream)
                .await
                .unwrap()
                .expect("child disconnected");
            if f.header.corr == *corr
                && matches!(f.header.ty, FrameType::Response | FrameType::Error)
            {
                break f;
            }
        }
    })
    .await
    .expect("request deadline");
    fs::write(out.join(format!("response-{corr:06}.json")), &response.body).unwrap();
    let value: Value = serde_json::from_slice(&response.body).unwrap();
    assert_ne!(
        response.header.ty,
        FrameType::Error,
        "request failed: {value}"
    );
    // Cold scans can exceed the production inspect deadline under stack logging.
    // Keep their full terminal responses, but continue exercising the other roots.
    if value.get("isError").and_then(Value::as_bool) == Some(true) {
        eprintln!("tool terminal error corr={corr}: {value}");
        assert_ne!(
            value
                .pointer("/structuredContent/code")
                .and_then(Value::as_str),
            Some("unknown_tool")
        );
    }
    println!(
        "corr={corr} channel={channel} type={:?} request={body}",
        response.header.ty
    );
}

async fn bind(
    stream: &mut TcpStream,
    channel: u16,
    root: &Path,
    target: RouteTarget,
    corr: &mut u64,
    out: &Path,
) {
    request(
        stream,
        0,
        corr,
        serde_json::to_value(ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target,
            identity: BindIdentity::new(
                root.to_path_buf(),
                "opencode",
                format!("malloc-soak-{channel}"),
            ),
            consumer_capabilities: None,
            principal: Some(Principal::Direct),
            admission_facts: Default::default(),
            scope: None,
            role_versions: None,
        })
        .unwrap(),
        out,
    )
    .await;
}

fn main() {
    let args: Vec<_> = std::env::args_os().collect();
    assert_eq!(
        args.len(),
        4,
        "usage: malloc_soak AFT_BINARY EXPERIMENT_DIR EMBEDDING_BASE_URL"
    );
    let binary = fs::canonicalize(&args[1]).unwrap();
    let out = fs::canonicalize(&args[2]).unwrap();
    let base_url = args[3].to_str().unwrap();
    let root = out.join("repo");
    let scaling = std::env::var_os("AFT_SOAK_SCALING").is_some();
    assert!(
        root.join(".git").exists(),
        "prepare an independent clone in EXPERIMENT_DIR/repo first"
    );
    let mut roots = vec![root.clone()];
    for n in 1..=if scaling { 10 } else { 4 } {
        let path = out.join(format!("worktree-{n}"));
        assert!(Command::new("git")
            .args(["worktree", "add", "--detach"])
            .arg(&path)
            .arg("HEAD")
            .current_dir(&root)
            .status()
            .unwrap()
            .success());
        roots.push(path);
    }
    fs::create_dir_all(out.join("config/cortexkit")).unwrap();
    let mut config = json!({
        "indexes": {"trigram": true, "semantic": true, "callgraph": true},
        "disabled_tools": [], "inspect": {"enabled": true},
        "lsp": {"auto_install": false, "disabled": ["rust", "typescript", "python", "go", "bash", "yaml", "ty"]},
        "semantic": {"backend": "openai_compatible", "model": "malloc-soak", "base_url": base_url, "max_files": 10000, "max_batch_size": 64, "timeout_ms": 60000}
    });
    if scaling {
        config["indexes"]["callgraph"] = json!(false);
        config["inspect"]["enabled"] = json!(false);
        config["idle"] = json!({"root_ttl_minutes": 5});
        config["lsp"]["idle_minutes"] = json!(5);
    }
    fs::write(out.join("config/cortexkit/aft.jsonc"), config.to_string()).unwrap();
    for root in &roots {
        fs::create_dir_all(root.join(".cortexkit")).unwrap();
        fs::write(root.join(".cortexkit/aft.jsonc"), json!({"indexes":{"trigram":true,"semantic":true,"callgraph":!scaling},"inspect":{"enabled":!scaling},"disabled_tools":[]}).to_string()).unwrap();
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let key = vec![0x42; subc_transport::KEY_LEN];
        let daemon_id = [0x24; subc_transport::DAEMON_ID_LEN];
        let conn = out.join("connection.json");
        connection_file::write_atomic(&conn, &ConnectionInfo {
            schema: SCHEMA_VERSION, wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {host:"127.0.0.1".into(), port:listener.local_addr().unwrap().port()}],
            key:key.clone(), daemon_id, pid:std::process::id(), daemon_ver:"isolated-malloc-soak".into(),
        }).unwrap();
        let log = fs::File::create(out.join("daemon.log")).unwrap();
        let mut child = ChildGuard(Command::new(binary).arg("--subc").arg(&conn)
            .env("MallocStackLogging", "1").env("MallocStackLoggingDirectory", &out)
            .env("AFT_STORAGE_DIR", out.join("storage")).env("XDG_CONFIG_HOME", out.join("config"))
            .env("XDG_DATA_HOME", out.join("data")).env("XDG_CACHE_HOME", out.join("cache"))
            .env_remove("SUBC_MODULE_ID").env_remove("SUBC_LAUNCH_NONCE")
            .current_dir(&root).stdin(Stdio::null()).stdout(log.try_clone().unwrap()).stderr(log).spawn().unwrap());
        fs::write(out.join("pid"), child.0.id().to_string()).unwrap();
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(60), listener.accept()).await.unwrap().unwrap();
        authenticate_server(&mut stream, &key, &daemon_id, "isolated-malloc-soak", Duration::from_secs(5)).await.unwrap();
        let hello = read_frame(&mut stream).await.unwrap().unwrap();
        assert_eq!(hello.header.ty, FrameType::Hello);
        write_frame(&mut stream, &Frame::build(FrameType::HelloAck, Flags::new(false, Priority::Passive, false), 0, 0, hello.header.corr,
            serde_json::to_vec(&ModuleHelloAckBody {negotiated_ver:PROTOCOL_VERSION, subc_ops:vec![], subc_capabilities:vec![], storage:None, machine_id:None}).unwrap()).unwrap()).await.unwrap();
        let mut corr = 100;
        bind(&mut stream, 100, &root, RouteTarget::ManagementSurface {module_id:"aft".into()}, &mut corr, &out).await;
        request(&mut stream, 100, &mut corr, json!({"op":"memory.census"}), &out).await;
        for (n, root) in roots.iter().enumerate() {
            bind(&mut stream, n as u16 + 1, root, RouteTarget::ToolProvider {module_id:"aft".into()}, &mut corr, &out).await;
            if scaling {
                // Cold parsing and embedding can exceed two minutes on a loaded
                // host. Allow a longer owner warmup before any borrower binds.
                let owner_warmup = std::env::var("AFT_SOAK_OWNER_WARMUP_SECS")
                    .ok()
                    .map(|value| value.parse::<u64>().expect("owner warmup must be seconds"))
                    .unwrap_or(120);
                tokio::time::sleep(Duration::from_secs(if n == 0 { owner_warmup } else { 5 })).await;
                request(&mut stream, 100, &mut corr, json!({"op":"memory.census"}), &out).await;
            } else {
                request(&mut stream, n as u16 + 1, &mut corr, json!({"name":"inspect","arguments":{"sections":"all"}}), &out).await;
            }
        }
        if scaling {
            for edited in [0, 1, 3, 5, 10] {
                for n in 1..=edited {
                    request(&mut stream, n as u16 + 1, &mut corr, json!({"name":"write","arguments":{"filePath":"malloc_soak_probe.rs","content":format!("pub fn scaling_{edited}() -> usize {{ {edited} }}\n")}}), &out).await;
                }
                tokio::time::sleep(Duration::from_secs(90)).await;
                request(&mut stream, 100, &mut corr, json!({"op":"memory.census"}), &out).await;
                fs::write(out.join(format!("stage-{edited}.json")), fs::read(out.join(format!("response-{corr:06}.json"))).unwrap()).unwrap();
            }
            // Same roots, ten further edits each: separate per-edit growth from root cardinality.
            for revision in 0..10 {
                for n in 1..=10 {
                    request(&mut stream, n as u16 + 1, &mut corr, json!({"name":"write","arguments":{"filePath":"malloc_soak_probe.rs","content":format!("pub fn repeated_{revision}() -> usize {{ {revision} }}\n")}}), &out).await;
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            tokio::time::sleep(Duration::from_secs(90)).await;
            request(&mut stream, 100, &mut corr, json!({"op":"memory.census"}), &out).await;
            fs::write(out.join("stage-repeat.json"), fs::read(out.join(format!("response-{corr:06}.json"))).unwrap()).unwrap();
            for channel in 1..=11 {
                write_frame(&mut stream, &Frame::build(FrameType::Goodbye, Flags::new(false, Priority::Passive, false), channel, 1, 0, vec![]).unwrap()).await.unwrap();
            }
            fs::write(out.join("unbound"), "short TTL phase\n").unwrap();
            for second in 0..36 {
                tokio::time::sleep(Duration::from_secs(10)).await;
                request(&mut stream, 100, &mut corr, json!({"op":"memory.census"}), &out).await;
                fs::write(out.join(format!("idle-{}.json", (second + 1) * 10)), fs::read(out.join(format!("response-{corr:06}.json"))).unwrap()).unwrap();
            }
            fs::write(out.join("finished"), "ready for final sample\n").unwrap();
            tokio::time::sleep(Duration::from_secs(180)).await;
            write_frame(&mut stream, &Frame::build(FrameType::Goodbye, Flags::new(false, Priority::Passive, false), 0, 0, 0, vec![]).unwrap()).await.unwrap();
            println!("child exit: {}", child.0.wait().unwrap());
            return;
        }
        // Bind three additional sessions to existing worktree roots; these routes
        // issue only searches, while the original routes continue to edit.
        for n in 0..3 {
            bind(&mut stream, n + 10, &roots[n as usize + 1], RouteTarget::ToolProvider {module_id:"aft".into()}, &mut corr, &out).await;
        }
        let start = Instant::now();
        let active_secs = std::env::var("AFT_SOAK_ACTIVE_SECS").ok().map(|v| v.parse().unwrap()).unwrap_or(2400);
        let idle_secs = std::env::var("AFT_SOAK_IDLE_SECS").ok().map(|v| v.parse().unwrap()).unwrap_or(1980);
        let mut round = 0;
        while start.elapsed() < Duration::from_secs(active_secs) {
            for n in 0..roots.len() {
                request(&mut stream, n as u16 + 1, &mut corr, json!({"name":"write","arguments":{"filePath":"malloc_soak_probe.rs","content":format!("/// Allocation experiment revision {round}.\npub fn malloc_soak_probe_{round}() -> usize {{ {round} }}\n")}}), &out).await;
                request(&mut stream, n as u16 + 1, &mut corr, json!({"name":"search","arguments":{"query":"how are idle root artifacts released"}}), &out).await;
                request(&mut stream, n as u16 + 1, &mut corr, json!({"name":"inspect","arguments":{"sections":["dead_code","cycles","duplicates"]}}), &out).await;
            }
            for channel in 10..13 {
                request(&mut stream, channel, &mut corr, json!({"name":"search","arguments":{"query":"where is semantic refresh handled"}}), &out).await;
            }
            request(&mut stream, 100, &mut corr, json!({"op":"memory.census"}), &out).await;
            round += 1;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        for channel in [1,2,3,4,5,10,11,12] {
            write_frame(&mut stream, &Frame::build(FrameType::Goodbye, Flags::new(false, Priority::Passive, false), channel, 1, 0, vec![]).unwrap()).await.unwrap();
        }
        fs::write(out.join("unbound"), format!("{:?}", std::time::SystemTime::now())).unwrap();
        let idle_start = Instant::now();
        while idle_start.elapsed() < Duration::from_secs(idle_secs) {
            request(&mut stream, 100, &mut corr, json!({"op":"memory.census"}), &out).await;
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        fs::write(out.join("finished"), "ready for final sample\n").unwrap();
        tokio::time::sleep(Duration::from_secs(180)).await;
        write_frame(&mut stream, &Frame::build(FrameType::Goodbye, Flags::new(false, Priority::Passive, false), 0, 0, 0, vec![]).unwrap()).await.unwrap();
        let status = child.0.wait().unwrap();
        println!("child exit: {status}");
    });
}
