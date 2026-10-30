//! `page.collect`, `page.read` and `page.release` through the real module
//! loop: core's management route asks for a session's page, and the reply
//! lists exactly what that session's read policy lets through.
//!
//! With `PAGE_COLLECT_PRINT_SAMPLES=1` the test prints every request and
//! reply as `PAGE_COLLECT_SAMPLE <name> <json>` lines; those are the
//! samples core and the phone app are built against. With
//! `PAGE_COLLECT_PRINT_FRAMES=1` it prints the reply frames themselves as
//! `PAGE_COLLECT_FRAME <name> <json>` lines: flags byte, body length, and
//! the body (hex when the frame is binary, parsed JSON otherwise).

use super::*;
use base64::Engine;

const HARNESS: &str = "runner";
const SESSION: &str = "page-session";
const CORE_CHANNEL: u16 = 12;
const OTHER_CHANNEL: u16 = 13;

fn sample(name: &str, request: &Value, reply: &Value) {
    if std::env::var_os("PAGE_COLLECT_PRINT_SAMPLES").is_some() {
        println!(
            "PAGE_COLLECT_SAMPLE {name} {}",
            json!({"request": request, "reply": reply})
        );
    }
}

struct Site {
    corr: u64,
    /// The last request body sent and the reply frame it got.
    last: Option<(Value, Frame)>,
}

impl Site {
    /// Print the last reply frame as a capture named `name`.
    fn frame(&self, name: &str) {
        if std::env::var_os("PAGE_COLLECT_PRINT_FRAMES").is_none() {
            return;
        }
        let (request, frame) = self.last.as_ref().expect("a reply frame");
        let flags = frame.header.flags;
        let body = if flags.is_binary() {
            json!({"body_hex": frame.body.iter().map(|byte| format!("{byte:02x}")).collect::<String>()})
        } else {
            json!({"body_json": serde_json::from_slice::<Value>(&frame.body).unwrap()})
        };
        let mut captured = json!({
            "request_body": request,
            "frame": {
                "type": format!("{:?}", frame.header.ty),
                "channel": frame.header.channel,
                "epoch": frame.header.epoch,
                "corr": frame.header.corr,
                "flags": flags.0,
                "flags_binary": flags.is_binary(),
                "body_len": frame.body.len(),
            },
        });
        captured["frame"]
            .as_object_mut()
            .unwrap()
            .extend(body.as_object().unwrap().clone());
        println!("PAGE_COLLECT_FRAME {name} {captured}");
    }

    async fn exchange(
        &mut self,
        stream: &mut tokio::net::TcpStream,
        channel: u16,
        request: Value,
    ) -> Frame {
        self.corr += 1;
        send_management_request(stream, channel, self.corr, request.clone()).await;
        let frame = read_frame_timeout(stream, "page operation reply").await;
        assert_eq!(frame.header.ty, FrameType::Response);
        assert_eq!(frame.header.channel, channel);
        assert_eq!(frame.header.epoch, 1);
        assert_eq!(frame.header.corr, self.corr);
        self.last = Some((request, frame.clone()));
        frame
    }

    async fn call(
        &mut self,
        stream: &mut tokio::net::TcpStream,
        channel: u16,
        op: &str,
        params: Value,
    ) -> (Value, Value) {
        let request = json!({"op": op, "params": params});
        let frame = self.exchange(stream, channel, request.clone()).await;
        assert!(!frame.header.flags.is_binary(), "{op} answered with bytes");
        let reply: Value = serde_json::from_slice(&frame.body).expect("management reply body");
        assert_eq!(reply["op"], op);
        (request, reply)
    }

    /// `page.read` answers with a binary frame of raw bytes, or a JSON
    /// refusal.
    async fn read(
        &mut self,
        stream: &mut tokio::net::TcpStream,
        params: Value,
    ) -> (Value, Result<Vec<u8>, Value>, Value) {
        let request = json!({"op": "page.read", "params": params});
        let frame = self.exchange(stream, CORE_CHANNEL, request.clone()).await;
        if frame.header.flags.is_binary() {
            let shown = json!({
                "frame_flags_binary": true,
                "body_len": frame.body.len(),
                "body_base64": base64::engine::general_purpose::STANDARD.encode(&frame.body),
            });
            (request, Ok(frame.body), shown)
        } else {
            let reply: Value = serde_json::from_slice(&frame.body).unwrap();
            (request, Err(reply.clone()), reply)
        }
    }
}

/// `params` for `page.read` or `page.release` as core sends them: keyed by
/// the session the snapshot was collected for.
fn keyed(mut params: Value) -> Value {
    params["harness"] = json!(HARNESS);
    params["session"] = json!(SESSION);
    params
}

/// `page.collect` params as core sends them.
fn collect_params(entry: &str) -> Value {
    collect_params_for_session(SESSION, entry)
}

/// `page.collect` params as core sends them, for another session. Refusals that
/// happen before the limits are read still carry core's full request, so the
/// captured request bytes match what core sends.
fn collect_params_for_session(session: &str, entry: &str) -> Value {
    json!({
        "harness": HARNESS,
        "session": session,
        "entry": entry,
        "limits": {
            "entry_max_bytes": 2_097_152, "max_files": 64, "max_file_bytes": 8_388_608,
            "max_total_bytes": 20_971_520, "import_depth": 1, "deadline_ms": 5000,
        },
    })
}

fn write_site(root: &std::path::Path) {
    let site = root.join("site");
    std::fs::create_dir_all(site.join("css")).unwrap();
    std::fs::create_dir_all(site.join("img")).unwrap();
    std::fs::write(
        site.join("index.html"),
        r#"<!doctype html>
<html><head>
  <link rel="stylesheet" href="css/site.css">
  <link rel="icon" href="img/icon.png">
  <link rel="stylesheet" href="https://cdn.example.com/x.css">
</head><body>
  <img src="img/logo%20mark.png" srcset="img/logo%20mark.png 1x, img/logo@2x.png 2x">
  <img src="data:image/png;base64,AAAA">
  <img src="//cdn.example.com/p.png">
  <img src="http://example.com/h.png">
  <img src="img/missing.png">
  <img src="cache.db">
  <img src="../../outside.png">
  <script src="app.js?v=3"></script>
</body></html>
"#,
    )
    .unwrap();
    std::fs::write(
        site.join("css/site.css"),
        "@import 'theme.css';\nbody { background: url(../img/bg.png); }\n",
    )
    .unwrap();
    std::fs::write(site.join("css/theme.css"), ".t { color: red; }\n").unwrap();
    std::fs::write(site.join("img/logo mark.png"), b"\x89PNG logo").unwrap();
    std::fs::write(site.join("img/logo@2x.png"), b"\x89PNG logo2x").unwrap();
    std::fs::write(site.join("img/icon.png"), b"\x89PNG icon").unwrap();
    std::fs::write(site.join("img/bg.png"), b"\x89PNG bg").unwrap();
    std::fs::write(site.join("app.js"), "console.log('page');\n").unwrap();
    std::fs::write(site.join("cache.db"), "SQLite format 3\0").unwrap();
    // Outside the bound root: the session's policy refuses it.
    std::fs::write(root.parent().unwrap().join("outside.png"), b"outside").unwrap();
    std::fs::write(root.parent().unwrap().join("outside.html"), "<html>").unwrap();
    std::fs::write(site.join("notes.txt"), "not a page").unwrap();
    std::fs::write(
        site.join("limits.html"),
        "<link rel=stylesheet href=css/theme.css><img src=app.js><img src=img/bg.png><img src=img/icon.png>\n",
    )
    .unwrap();
    std::fs::write(
        site.join("small.html"),
        "<!doctype html><img src=\"img/bg.png\"><img src=\"cache.db\">\n",
    )
    .unwrap();
    std::fs::write(
        site.join("big.html"),
        format!("<html>{}</html>", "x".repeat(64)),
    )
    .unwrap();
}

async fn bind_tool_route(
    stream: &mut tokio::net::TcpStream,
    channel: u16,
    root: &std::path::Path,
    session: &str,
    principal: Principal,
) {
    send_route_bind_with_harness_session_principal_and_doc(
        stream,
        channel,
        channel as u64 + 900,
        root,
        HARNESS,
        session,
        Some(principal),
        json!({"indexes": {"trigram": false, "semantic": false, "callgraph": false}}),
    )
    .await;
    expect_route_bind_ack(stream, channel as u64 + 900).await;
}

fn code(reply: &Value) -> &str {
    assert_eq!(reply["status"], "error", "{reply}");
    reply["data"]["code"].as_str().unwrap()
}

fn file_hashes(reply: &Value) -> Vec<(String, String)> {
    reply["data"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            (
                file["path"].as_str().unwrap().to_string(),
                file["sha256"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

async fn drive_page_collect_daemon(input: FakeDaemonInput) {
    // The daemon's storage root: the harness keeps the user config file there.
    let storage = input.user_config_path.parent().unwrap().to_path_buf();
    let FakeDaemonSession {
        mut stream, root1, ..
    } = open_fake_daemon_session(input).await;
    // The session's bind root is a folder inside the temp root, so the
    // files beside it are outside the session's reach.
    let project = root1.join("project");
    std::fs::create_dir_all(&project).unwrap();
    write_site(&project);
    bind_tool_route(&mut stream, 1, &project, SESSION, Principal::Direct).await;
    send_management_route_bind(
        &mut stream,
        CORE_CHANNEL,
        20,
        &root1,
        Some(Principal::Reserved {
            module_id: "prefrontal-core".into(),
        }),
    )
    .await;
    expect_route_bind_ack(&mut stream, 20).await;
    send_management_route_bind(
        &mut stream,
        OTHER_CHANNEL,
        21,
        &root1,
        Some(Principal::Reserved {
            module_id: "prefrontal".into(),
        }),
    )
    .await;
    expect_route_bind_ack(&mut stream, 21).await;
    let mut site = Site {
        corr: 1000,
        last: None,
    };
    let stream = &mut stream;

    // A bundle with omissions.
    let (request, first) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.collect",
            collect_params("site/index.html"),
        )
        .await;
    sample("collect-bundle-with-omitted", &request, &first);
    site.frame("collect-bundle-with-omitted");
    assert_eq!(first["status"], "ok", "{first}");
    let data = &first["data"];
    assert_eq!(data["entry"], "index.html");
    let paths = file_hashes(&first)
        .into_iter()
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        vec![
            "index.html",
            "css/site.css",
            "img/icon.png",
            "img/logo mark.png",
            "img/logo@2x.png",
            "app.js",
            "img/bg.png",
        ]
    );
    assert_eq!(
        data["omitted"],
        json!([
            {"path": "https://cdn.example.com/x.css", "reason": "remote"},
            {"path": "data:image/png;base64,AAAA", "reason": "not_relative"},
            {"path": "//cdn.example.com/p.png", "reason": "remote"},
            {"path": "http://example.com/h.png", "reason": "remote"},
            {"path": "img/missing.png", "reason": "missing"},
            {"path": "cache.db", "reason": "denied"},
            {"path": "../../outside.png", "reason": "denied"},
            {"path": "css/theme.css", "reason": "depth_limit"},
        ])
    );
    let snapshot = data["snapshot"].as_str().unwrap().to_string();
    assert!(data["expires_at_ms"].as_u64().unwrap() > 0);

    // The same page again: a new snapshot with identical hashes.
    let (request, second) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.collect",
            collect_params("site/index.html"),
        )
        .await;
    sample("collect-same-page-again", &request, &second);
    site.frame("collect-same-page-again");
    assert_eq!(file_hashes(&first), file_hashes(&second));
    let second_snapshot = second["data"]["snapshot"].as_str().unwrap().to_string();
    assert_ne!(snapshot, second_snapshot);

    // page.read and its refusals.
    let (request, bytes, shown) = site
        .read(
            stream,
            keyed(json!({"snapshot": snapshot, "path": "img/logo mark.png", "offset": 0, "length": 1_048_576})),
        )
        .await;
    sample("read-bytes", &request, &shown);
    site.frame("read-bytes");
    assert_eq!(bytes.unwrap(), b"\x89PNG logo");
    let (request, bytes, shown) = site
        .read(
            stream,
            keyed(json!({"snapshot": snapshot, "path": "css/site.css", "offset": 21, "length": 4})),
        )
        .await;
    sample("read-range", &request, &shown);
    site.frame("read-range");
    assert_eq!(bytes.unwrap(), b"body");
    for (name, params, expected) in [
        (
            "read-refused-not-in-snapshot",
            keyed(json!({"snapshot": snapshot, "path": "cache.db", "offset": 0, "length": 16})),
            "not_in_snapshot",
        ),
        (
            "read-refused-bad-range",
            keyed(
                json!({"snapshot": snapshot, "path": "app.js", "offset": 0, "length": 1_048_577}),
            ),
            "bad_range",
        ),
        (
            "read-refused-snapshot-expired",
            keyed(
                json!({"snapshot": "00000000000000000000000000000000", "path": "app.js", "offset": 0, "length": 16}),
            ),
            "snapshot_expired",
        ),
    ] {
        let (request, bytes, shown) = site.read(stream, params).await;
        sample(name, &request, &shown);
        site.frame(name);
        assert_eq!(code(&bytes.unwrap_err()), expected);
    }

    // Release is idempotent.
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.release",
            keyed(json!({"snapshot": snapshot})),
        )
        .await;
    sample("release", &request, &reply);
    site.frame("release");
    assert_eq!(
        reply["data"],
        json!({"snapshot": snapshot, "released": true})
    );
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.release",
            keyed(json!({"snapshot": snapshot})),
        )
        .await;
    sample("release-again", &request, &reply);
    site.frame("release-again");
    assert_eq!(
        reply["data"],
        json!({"snapshot": snapshot, "released": false})
    );
    let (_, bytes, _) = site
        .read(
            stream,
            keyed(json!({"snapshot": snapshot, "path": "app.js", "offset": 0, "length": 1})),
        )
        .await;
    assert_eq!(code(&bytes.unwrap_err()), "snapshot_expired");

    // Refusals about the page itself.
    let mut too_small = collect_params("site/big.html");
    too_small["limits"]["entry_max_bytes"] = json!(32);
    for (name, params, expected) in [
        (
            "collect-refused-entry-unreadable-missing",
            collect_params("site/absent.html"),
            "entry_unreadable",
        ),
        (
            "collect-refused-entry-unreadable-denied",
            collect_params("../outside.html"),
            "entry_unreadable",
        ),
        (
            "collect-refused-entry-too-large",
            too_small,
            "entry_too_large",
        ),
        (
            "collect-refused-entry-not-html",
            collect_params("site/notes.txt"),
            "entry_not_html",
        ),
        (
            "collect-refused-session-not-bound",
            collect_params_for_session("no-such-session", "site/index.html"),
            "session_not_bound",
        ),
    ] {
        let (request, reply) = site
            .call(stream, CORE_CHANNEL, "page.collect", params)
            .await;
        sample(name, &request, &reply);
        site.frame(name);
        assert_eq!(code(&reply), expected, "{name}");
    }

    // Two live routes of one session that disagree on trust.
    bind_tool_route(stream, 2, &project, "split-session", Principal::Direct).await;
    bind_tool_route(stream, 3, &project, "split-session", Principal::Unverified).await;
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.collect",
            collect_params_for_session("split-session", "site/index.html"),
        )
        .await;
    sample("collect-refused-session-route-ambiguous", &request, &reply);
    site.frame("collect-refused-session-route-ambiguous");
    assert_eq!(code(&reply), "session_route_ambiguous");

    // A caller other than core.
    let (request, reply) = site
        .call(
            stream,
            OTHER_CHANNEL,
            "page.collect",
            collect_params("site/index.html"),
        )
        .await;
    sample("collect-refused-not-core", &request, &reply);
    site.frame("collect-refused-not-core");
    assert_eq!(code(&reply), "page_caller_refused");

    // At most four live snapshots: the second collection above is live.
    let mut live = vec![second_snapshot];
    for _ in 0..3 {
        let (_, reply) = site
            .call(
                stream,
                CORE_CHANNEL,
                "page.collect",
                collect_params("site/index.html"),
            )
            .await;
        live.push(reply["data"]["snapshot"].as_str().unwrap().to_string());
    }
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.collect",
            collect_params("site/index.html"),
        )
        .await;
    sample("collect-refused-busy", &request, &reply);
    site.frame("collect-refused-busy");
    assert_eq!(code(&reply), "busy");
    assert!(reply["data"]["detail"]["retry_after_ms"].as_u64().unwrap() > 0);
    for snapshot in live {
        let (_, reply) = site
            .call(
                stream,
                CORE_CHANNEL,
                "page.release",
                keyed(json!({"snapshot": snapshot})),
            )
            .await;
        assert_eq!(reply["data"]["released"], true);
    }

    // A small page with one denied file.
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.collect",
            collect_params("site/small.html"),
        )
        .await;
    sample("collect-small-page-one-denied", &request, &reply);
    site.frame("collect-small-page-one-denied");
    assert_eq!(
        reply["data"]["omitted"],
        json!([{"path": "cache.db", "reason": "denied"}])
    );
    let small = reply["data"]["snapshot"].as_str().unwrap().to_string();
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.release",
            keyed(json!({"snapshot": small})),
        )
        .await;
    sample("release-small-page", &request, &reply);
    site.frame("release-small-page");

    // Limits never refuse: a file past a limit is listed in `omitted`.
    let mut tight = collect_params("site/limits.html");
    tight["limits"]["max_files"] = json!(2);
    tight["limits"]["max_file_bytes"] = json!(8);
    tight["limits"]["import_depth"] = json!(0);
    let (request, reply) = site.call(stream, CORE_CHANNEL, "page.collect", tight).await;
    sample("collect-over-limits-omitted", &request, &reply);
    site.frame("collect-over-limits-omitted");
    let reasons = reply["data"]["omitted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|omitted| omitted["reason"].as_str().unwrap())
        .collect::<HashSet<_>>();
    for reason in ["depth_limit", "over_file_limit", "over_count_limit"] {
        assert!(reasons.contains(reason), "{reason}: {reply}");
    }
    let tight = reply["data"]["snapshot"].as_str().unwrap().to_string();
    site.call(
        stream,
        CORE_CHANNEL,
        "page.release",
        keyed(json!({"snapshot": tight})),
    )
    .await;

    // Malformed parameters.
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.collect",
            json!({"harness": HARNESS, "session": SESSION}),
        )
        .await;
    sample("collect-refused-invalid-request", &request, &reply);
    site.frame("collect-refused-invalid-request");
    assert_eq!(code(&reply), "invalid_request");
    let (request, bytes, shown) = site
        .read(
            stream,
            keyed(json!({"snapshot": small, "path": "index.html"})),
        )
        .await;
    sample("read-refused-invalid-request", &request, &shown);
    site.frame("read-refused-invalid-request");
    assert_eq!(code(&bytes.unwrap_err()), "invalid_request");

    // The snapshot folder cannot be created: a plain file stands where it
    // belongs under the daemon's storage.
    let snapshots = storage.join("page-snapshots");
    let _ = std::fs::remove_dir_all(&snapshots);
    std::fs::write(&snapshots, "not a folder").unwrap();
    let (request, reply) = site
        .call(
            stream,
            CORE_CHANNEL,
            "page.collect",
            collect_params("site/small.html"),
        )
        .await;
    sample("collect-refused-snapshot-unavailable", &request, &reply);
    site.frame("collect-refused-snapshot-unavailable");
    assert_eq!(code(&reply), "snapshot_unavailable");
    std::fs::remove_file(&snapshots).unwrap();

    send_connection_goodbye(stream).await;
}

#[test]
fn subc_bridge_page_collect_management_operations() {
    run_subc_bridge_production_test(
        "subc_bridge_page_collect_management_operations",
        Duration::from_secs(60),
        drive_page_collect_daemon,
        |_, _, _| {},
    );
}
