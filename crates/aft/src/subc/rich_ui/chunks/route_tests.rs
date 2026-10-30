//! The rich-UI chunk queries and Occipital's route shapes, driven through the
//! real module frame loop: binds go through the channel-0 RouteBind arm and
//! requests through the routed-request arm, exactly as the daemon sends them.

use std::collections::BTreeMap;

use base64::Engine as _;
use sha2::{Digest, Sha256};

use super::rich_ui::chunks;
use super::*;

const GOLDEN_MANIFEST: &[u8] =
    include_bytes!("../../../../tests/fixtures/rich_ui/manifest.jcs.json");

fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

/// Removing fixture actors releases process-wide root-cache read markers.
struct FixtureActors(Arc<Executor>);

impl Drop for FixtureActors {
    fn drop(&mut self) {
        for (root, ctx) in self.0.actor_entries() {
            ctx.bash_background().shutdown();
            ctx.clear_db();
            self.0.remove_actor(&root);
        }
    }
}

struct Wire {
    daemon: tokio::io::DuplexStream,
    module: Option<std::thread::JoinHandle<Result<ModuleLoopExit, SubcError>>>,
    ctx: Arc<AppContext>,
    root: PathBuf,
    _actors: FixtureActors,
    _project: tempfile::TempDir,
}

impl Wire {
    async fn start() -> Self {
        let (project, root_id) = test_support::test_root("rich-ui-chunk-route");
        let root = project.path().to_path_buf();
        let mut config = crate::config::Config::default();
        config.project_root = Some(root.clone());
        config.storage_dir = Some(root.join(".storage"));
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        std::fs::create_dir_all(ctx.storage_dir()).unwrap();
        ctx.set_db(Arc::new(StdMutex::new(
            crate::db::open(&ctx.storage_dir().join("aft.db")).unwrap(),
        )));
        let ctx = Arc::new(ctx);
        let executor = Arc::new(Executor::new());
        let actors = FixtureActors(executor.clone());
        executor.register_actor(root_id, ctx.clone());
        let app = ctx.app().clone();
        let module_root = root.clone();
        let storage = ctx.storage_dir().to_path_buf();
        let (daemon, module) = tokio::io::duplex(1024 * 1024);
        let module = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (read, write) = tokio::io::split(module);
            runtime.block_on(run_module_loop(
                read,
                write,
                &module_root.join("absent-connection.json"),
                app,
                executor,
                |request, ctx| {
                    if request.command == "configure" {
                        crate::commands::configure::handle_configure(&request, ctx)
                    } else {
                        Response::success(request.id, json!({"executed": true}))
                    }
                },
                Some(test_support::bind_config_without_indexes(&module_root)),
                false,
                usize::MAX,
                None,
                &storage,
                QuiescedLsp::ReapInBackground,
                None,
                None,
            ))
        });
        let mut wire = Self {
            daemon,
            module: Some(module),
            ctx,
            root,
            _actors: actors,
            _project: project,
        };
        let hello = wire.receive(0, HELLO_CORR).await;
        assert_eq!(hello.header.ty, FrameType::Hello);
        wire.send(FrameType::HelloAck, 0, HELLO_CORR, json!({}))
            .await;
        wire
    }

    async fn send(&mut self, ty: FrameType, channel: u16, corr: u64, body: Value) {
        let frame = Frame::build(
            ty,
            control_flags(),
            channel,
            u32::from(channel != 0),
            corr,
            if matches!(ty, FrameType::Goodbye) {
                Vec::new()
            } else {
                serde_json::to_vec(&body).unwrap()
            },
        )
        .unwrap();
        write_frame(&mut self.daemon, &frame).await.unwrap();
    }

    async fn receive(&mut self, channel: u16, corr: u64) -> Frame {
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let frame = read_frame(&mut self.daemon).await.unwrap().unwrap();
                if frame.header.channel == channel && frame.header.corr == corr {
                    return frame;
                }
            }
        })
        .await
        .expect("route reply timed out")
    }

    /// Bind `channel` and return the bind reply frame.
    async fn bind(
        &mut self,
        channel: u16,
        target: RouteTarget,
        principal: Principal,
        scope: Option<subc_protocol::scope::ScopeStamp>,
        v1: bool,
    ) -> Frame {
        let bind = ModuleControlRequest::RouteBind {
            route_channel: channel,
            epoch: 1,
            target,
            identity: subc_protocol::BindIdentity::new(
                &self.root,
                "runner",
                &format!("session-{channel}"),
            ),
            principal: Some(principal),
            consumer_capabilities: None,
            admission_facts: Default::default(),
            scope,
            role_versions: v1
                .then(|| BTreeMap::from([("tool-provider".to_string(), "v1".to_string())])),
            call_mode: None,
        };
        let corr = 1_000 + u64::from(channel);
        self.send(
            FrameType::Request,
            0,
            corr,
            serde_json::to_value(bind).unwrap(),
        )
        .await;
        self.receive(0, corr).await
    }

    async fn bind_provider(
        &mut self,
        channel: u16,
        principal: Principal,
        scope: Option<subc_protocol::scope::ScopeStamp>,
    ) {
        let ack = self
            .bind(
                channel,
                RouteTarget::ToolProvider {
                    module_id: "aft".into(),
                },
                principal,
                scope,
                true,
            )
            .await;
        assert!(
            matches!(
                serde_json::from_slice::<ModuleControlResponse>(&ack.body),
                Ok(ModuleControlResponse::RouteBindAck {})
            ),
            "channel {channel} bind: {:?}",
            String::from_utf8_lossy(&ack.body)
        );
    }

    async fn request(&mut self, channel: u16, corr: u64, body: Value) -> (FrameType, Value) {
        self.send(FrameType::Request, channel, corr, body).await;
        let frame = self.receive(channel, corr).await;
        (
            frame.header.ty,
            serde_json::from_slice(&frame.body).unwrap(),
        )
    }

    /// A database lookup that retries `database_busy`. The module's own
    /// maintenance timer briefly holds aft.db, and a chunk lookup reports that
    /// as busy instead of waiting. Tests that are not about busy replies must
    /// not fail on that timing. Each attempt takes a fresh correlation id.
    async fn request_unbusy(
        &mut self,
        channel: u16,
        corr: &mut u64,
        body: Value,
    ) -> (FrameType, Value) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            *corr += 1;
            let reply = self.request(channel, *corr, body.clone()).await;
            let busy = reply.0 == FrameType::Error && reply.1["code"] == "database_busy";
            if !busy || std::time::Instant::now() >= deadline {
                return reply;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    async fn stop(mut self) {
        self.send(FrameType::Goodbye, 0, 9_999, json!({})).await;
        self.module.take().unwrap().join().unwrap().unwrap();
    }
}

fn occipital() -> Principal {
    Principal::Reserved {
        module_id: "occipital".into(),
    }
}

fn stamp(owner: Principal, scope_ref: &str) -> subc_protocol::scope::ScopeStamp {
    serde_json::from_value(json!({
        "owner": owner, "ref": scope_ref, "scope_epoch": 1,
        "kind": "head", "owner_authorized": true,
    }))
    .unwrap()
}

/// Assert a typed protocol Error frame carrying `code` and its fixed message.
fn assert_refusal(reply: (FrameType, Value), code: &str) {
    let (ty, body) = reply;
    assert_eq!(ty, FrameType::Error, "{body}");
    assert_eq!(body["code"], code, "{body}");
    assert_eq!(
        body["message"],
        super::rich_ui::operations::fixed_message(code),
        "{body}"
    );
    assert!(
        body.get("content").is_none() && body.get("structuredContent").is_none(),
        "a chunk refusal is never a tool-result body: {body}"
    );
}

const UNSCOPED: u16 = 1;
const SELF_OWNED: u16 = 2;
const CARRIED: u16 = 3;
const BROCA_UNSCOPED: u16 = 4;
const BROCA_SCOPED: u16 = 5;

async fn occipital_wire() -> Wire {
    let mut wire = Wire::start().await;
    wire.bind_provider(UNSCOPED, occipital(), None).await;
    wire.bind_provider(
        SELF_OWNED,
        occipital(),
        Some(stamp(occipital(), "occipital-own")),
    )
    .await;
    let core = Principal::Reserved {
        module_id: "prefrontal-core".into(),
    };
    wire.bind_provider(CARRIED, occipital(), Some(stamp(core.clone(), "head-s")))
        .await;
    let broca = Principal::Reserved {
        module_id: "broca".into(),
    };
    wire.bind_provider(BROCA_UNSCOPED, broca.clone(), None)
        .await;
    wire.bind_provider(BROCA_SCOPED, broca, Some(stamp(core, "worker-w")))
        .await;
    wire
}

#[tokio::test]
async fn chunk_queries_answer_occipital_on_every_route_shape_and_refuse_others() {
    let _watcher_disabled = crate::test_env::without_project_watcher();
    let _manifest = chunks::set_test_manifest_for_test(Some(GOLDEN_MANIFEST.to_vec()));
    let mut wire = occipital_wire().await;
    let manifest_digest = sha256(GOLDEN_MANIFEST);

    // A 300,000-byte stored template whose first chunk boundary splits a
    // two-byte UTF-8 character.
    let asset = chunks::tests::split_utf8_asset();
    let template_digest = sha256(&asset);
    // Store the bytes with a real view record, as the task writer does.
    // Unreferenced templates can be swept by the module's maintenance timer.
    let view_manifest = crate::db::rich_ui::ViewManifest {
        row_digest: template_digest.clone(),
        row_bytes: asset.clone(),
        ..crate::db::rich_ui::test_view_manifest_for_test()
    };
    let metadata =
        crate::db::rich_ui::terminal_task_metadata_for_test(Some(&view_manifest.pins[0]));
    let _view_writing = crate::db::rich_ui::enable_test_view_writing_for_test(view_manifest);
    let row =
        crate::db::rich_ui::task_row_for_test(&metadata, "runner", "completed", Some(7), false);
    let db = wire.ctx.app().db().expect("the shared App holds aft.db");
    {
        let mut conn = db.lock().unwrap();
        let tx = conn.transaction().unwrap();
        crate::db::rich_ui::upsert_task_row_with_view(&tx, &row, &metadata).unwrap();
        tx.commit().unwrap();
    }

    let mut corr = 100;
    for channel in [UNSCOPED, SELF_OWNED, CARRIED] {
        let mut replies = Vec::new();
        for index in 0..3u64 {
            let (ty, reply) = wire
                .request_unbusy(
                    channel,
                    &mut corr,
                    json!({"op": "rich_ui.template_chunk", "templateDigest": template_digest, "index": index}),
                )
                .await;
            assert_eq!(ty, FrameType::Response, "{reply}");
            replies.push(reply);
            if channel == UNSCOPED && index == 0 {
                // Retention between chunk reads must keep the referenced bytes.
                let mut conn = db.lock().unwrap();
                crate::db::compression_events::prune_retention_tick(&mut conn, 1).unwrap();
            }
        }
        let mut sizes = Vec::new();
        let joined = chunks::tests::reassemble(&template_digest, |index| {
            let reply = replies[index as usize].clone();
            sizes.push(
                base64::engine::general_purpose::STANDARD
                    .decode(reply["data"].as_str().unwrap())
                    .unwrap()
                    .len(),
            );
            reply
        });
        assert_eq!(replies[0]["chunks"], 3);
        assert_eq!(replies[0]["bytes"], 300_000);
        assert_eq!(sizes, [131_072, 131_072, 37_856]);
        assert_eq!(joined, asset);
        assert_eq!(sha256(&joined), template_digest);

        corr += 1;
        let (ty, reply) = wire
            .request(
                channel,
                corr,
                json!({"op": "rich_ui.manifest_chunk", "manifestDigest": manifest_digest, "index": 0}),
            )
            .await;
        assert_eq!(ty, FrameType::Response, "{reply}");
        let joined = chunks::tests::reassemble(&manifest_digest, |_| reply.clone());
        assert_eq!(joined, GOLDEN_MANIFEST);

        // The bare form names the query in `name` with an `arguments` body.
        let (ty, reply) = wire
            .request_unbusy(
                channel,
                &mut corr,
                json!({"name": "rich_ui.template_chunk", "arguments": {"templateDigest": template_digest, "index": 2}}),
            )
            .await;
        assert_eq!(ty, FrameType::Response, "{reply}");
        assert_eq!(reply["index"], 2);

        assert_refusal(
            wire.request_unbusy(
                channel,
                &mut corr,
                json!({"op": "rich_ui.template_chunk", "templateDigest": template_digest, "index": 3}),
            )
            .await,
            "chunk_index_out_of_range",
        );
        assert_refusal(
            wire.request_unbusy(
                channel,
                &mut corr,
                json!({"op": "rich_ui.template_chunk", "templateDigest": sha256(b"missing"), "index": 0}),
            )
            .await,
            "digest_unknown",
        );
        corr += 1;
        assert_refusal(
            wire.request(
                channel,
                corr,
                json!({"op": "rich_ui.manifest_chunk", "manifestDigest": sha256(b"missing"), "index": 0}),
            )
            .await,
            "digest_unknown",
        );
    }

    // Any other principal is refused before a lookup, scoped or unscoped.
    for channel in [BROCA_UNSCOPED, BROCA_SCOPED] {
        for body in [
            json!({"op": "rich_ui.manifest_chunk", "manifestDigest": manifest_digest, "index": 0}),
            json!({"op": "rich_ui.template_chunk", "templateDigest": template_digest, "index": 0}),
            json!({"name": "rich_ui.template_chunk", "arguments": {"templateDigest": template_digest, "index": 0}}),
        ] {
            corr += 1;
            assert_refusal(
                wire.request(channel, corr, body).await,
                "principal_not_allowed",
            );
        }
    }

    // A held database lock is busy, never a miss; the lookup does not wait.
    {
        let _held = db.lock().unwrap();
        for channel in [UNSCOPED, CARRIED] {
            corr += 1;
            assert_refusal(
                wire.request(
                    channel,
                    corr,
                    json!({"op": "rich_ui.template_chunk", "templateDigest": template_digest, "index": 0}),
                )
                .await,
                "database_busy",
            );
        }
        // The manifest query never touches the database, so it still answers.
        corr += 1;
        let (ty, reply) = wire
            .request(
                UNSCOPED,
                corr,
                json!({"op": "rich_ui.manifest_chunk", "manifestDigest": manifest_digest, "index": 0}),
            )
            .await;
        assert_eq!(ty, FrameType::Response, "{reply}");
    }

    // With no database handle the template lookup is unavailable, not a miss.
    drop(db);
    wire.ctx.clear_db();
    for channel in [UNSCOPED, CARRIED] {
        corr += 1;
        assert_refusal(
            wire.request(
                channel,
                corr,
                json!({"op": "rich_ui.template_chunk", "templateDigest": template_digest, "index": 0}),
            )
            .await,
            "database_unavailable",
        );
    }
    wire.stop().await;
}

#[tokio::test]
async fn manifest_chunk_before_publication_refuses_digest_unknown_on_the_route() {
    let _watcher_disabled = crate::test_env::without_project_watcher();
    let _manifest = chunks::set_test_manifest_for_test(None);
    let mut wire = occipital_wire().await;
    for (corr, channel) in [(10, UNSCOPED), (11, CARRIED)] {
        assert_refusal(
            wire.request(
                channel,
                corr,
                json!({"op": "rich_ui.manifest_chunk", "manifestDigest": sha256(GOLDEN_MANIFEST), "index": 0}),
            )
            .await,
            "digest_unknown",
        );
    }
    wire.stop().await;
}

#[tokio::test]
async fn non_carrier_occipital_routes_admit_provider_reads_only() {
    let _watcher_disabled = crate::test_env::without_project_watcher();
    let mut wire = occipital_wire().await;
    let mut corr = 200;
    for channel in [UNSCOPED, SELF_OWNED] {
        for body in [
            json!({"op": "role.describe"}),
            json!({"op": "tool.catalog"}),
        ] {
            corr += 1;
            let (ty, reply) = wire.request(channel, corr, body.clone()).await;
            assert_eq!(ty, FrameType::Response, "{body}: {reply}");
        }
        // Every tool-call form refuses `session_not_bound`, before the carrier
        // gate would have judged the tool (so `write` gets the same code).
        for body in [
            json!({"op": "tool.call", "name": "read", "arguments": {"path": "x"}}),
            json!({"name": "read", "arguments": {"path": "x"}}),
            json!({"op": "tool.call", "name": "write", "arguments": {"path": "x", "content": "x"}}),
            json!({"op": "tool.call", "name": "bash_status", "arguments": {"taskId": "t"}}),
        ] {
            corr += 1;
            let (ty, reply) = wire.request(channel, corr, body.clone()).await;
            assert_eq!(ty, FrameType::Error, "{body}: {reply}");
            assert_eq!(reply["code"], "session_not_bound", "{body}: {reply}");
        }
    }
    // A carried route is not refused here: its tool calls continue to the
    // carrier gate, which refuses an unlisted tool with its own code.
    corr += 1;
    let (ty, reply) = wire
        .request(
            CARRIED,
            corr,
            json!({"op": "tool.call", "name": "write", "arguments": {"path": "x", "content": "x"}}),
        )
        .await;
    assert_eq!(ty, FrameType::Error, "{reply}");
    assert_eq!(reply["code"], "carrier_read_only", "{reply}");
    wire.stop().await;
}

#[tokio::test]
async fn occipital_legacy_and_management_binds_are_refused() {
    let _watcher_disabled = crate::test_env::without_project_watcher();
    let mut wire = Wire::start().await;
    let core = Principal::Reserved {
        module_id: "prefrontal-core".into(),
    };
    let mut channel = 10;
    for scope in [
        None,
        Some(stamp(occipital(), "occipital-own")),
        Some(stamp(core, "head-s")),
    ] {
        for (target, v1) in [
            (
                RouteTarget::ToolProvider {
                    module_id: "aft".into(),
                },
                false,
            ),
            (
                RouteTarget::ManagementSurface {
                    module_id: "aft".into(),
                },
                false,
            ),
            (
                RouteTarget::ManagementSurface {
                    module_id: "aft".into(),
                },
                true,
            ),
        ] {
            channel += 1;
            let reply = wire
                .bind(channel, target, occipital(), scope.clone(), v1)
                .await;
            assert_eq!(reply.header.ty, FrameType::Error);
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(body["code"], "route_refused", "{body}");
        }
    }
    // The same legacy bind by another principal is still accepted.
    channel += 1;
    let ack = wire
        .bind(
            channel,
            RouteTarget::ToolProvider {
                module_id: "aft".into(),
            },
            Principal::Reserved {
                module_id: "broca".into(),
            },
            None,
            false,
        )
        .await;
    assert!(
        matches!(
            serde_json::from_slice::<ModuleControlResponse>(&ack.body),
            Ok(ModuleControlResponse::RouteBindAck {})
        ),
        "{:?}",
        String::from_utf8_lossy(&ack.body)
    );
    wire.stop().await;
}
