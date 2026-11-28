use super::*;

fn shim_state(base: &Path) -> PathBuf {
    base.join("xdg/state/cortexkit/aft/gh-shim")
}

pub(super) struct PolicyRootPeer {
    answer: bool,
    advertise: bool,
    wait_after_worker: bool,
    reads: usize,
}

impl PolicyRootPeer {
    pub(super) fn new(base: &Path) -> Self {
        let seeded_list = shim_state(base).join("gh-routing-manifest.json").exists();
        let answer = seeded_list || base.join("answer-policy-root").exists();
        Self {
            answer,
            advertise: answer || base.join("hold-policy-root").exists(),
            wait_after_worker: seeded_list && !base.join("defer-policy-root").exists(),
            reads: 0,
        }
    }

    pub(super) fn advertised_ops(&self) -> Vec<String> {
        if self.advertise {
            vec!["policy_root.get".into()]
        } else {
            vec![]
        }
    }

    pub(super) fn expects_pin_after_worker_bind(&self) -> bool {
        self.wait_after_worker
    }

    async fn handle(&mut self, stream: &mut TcpStream, frame: &Frame) -> bool {
        if frame.header.ty != FrameType::Request
            || frame.header.channel != 0
            || serde_json::from_slice::<Value>(&frame.body).ok().as_ref()
                != Some(&json!({"op":"policy_root.get"}))
        {
            return false;
        }
        self.reads += 1;
        if self.answer {
            let reply = Frame::build(
                FrameType::Response,
                frame.header.flags,
                0,
                0,
                frame.header.corr,
                serde_json::to_vec(&worker_gh_read::policy_root_reply()).unwrap(),
            )
            .unwrap();
            write_frame(stream, &reply).await.unwrap();
        }
        true
    }
}

impl Jail {
    pub(super) async fn next_non_policy_frame(&mut self) -> Frame {
        loop {
            let frame = next_frame(&mut self.stream).await;
            if !self.policy_root.handle(&mut self.stream, &frame).await {
                return frame;
            }
        }
    }

    pub(super) async fn wait_for_fixture_pin(&mut self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.policy_root.reads == 0 {
                let frame = next_frame(&mut self.stream).await;
                if !self.policy_root.handle(&mut self.stream, &frame).await {
                    self.pending_frames.push_back(frame);
                }
            }
            while !shim_state(&self.base).join("trust-root.json").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("AFT did not fetch and publish the fixture daemon's pin");
    }
}

#[tokio::test]
async fn gh_shim_attached_all_off_does_not_poll_or_create_policy_state() {
    let mut jail = Jail::start_with_layout_and_env(
        None,
        None,
        true,
        "strict",
        false,
        false,
        |base, command| {
            std::fs::write(
                base.join("xdg/config/cortexkit/aft.jsonc"),
                r#"{"github":{"shim":false},"git":{"co_author":"off"}}"#,
            )
            .unwrap();
            std::fs::write(base.join("answer-policy-root"), b"").unwrap();
            command
                .env_remove("AFT_GH_SHIM_STATE_DIR")
                .env("SUBC_MODULE_ID", "aft")
                .env("SUBC_LAUNCH_NONCE", "gh-shim-off-fixture");
        },
    )
    .await;
    jail.bind_grant_head().await;
    let reply = jail.tool_reply_on_channel(2, "bash", json!({"description":"Check disabled routing", "command":"test -z \"${AFT_GH_SHIM_ENABLED+x}${AFT_GH_SHIM_TRUST_ANCHOR+x}\" && printf SHIM_OFF"})).await;
    let text = model_text(&reply);
    assert!(text.contains("SHIM_OFF"), "{text}");
    let _ = tokio::time::timeout(Duration::from_millis(250), async {
        loop {
            let frame = jail.next_non_policy_frame().await;
            jail.pending_frames.push_back(frame);
        }
    })
    .await;
    assert_eq!(
        jail.policy_root.reads, 0,
        "an attached all-off AFT must not read policy_root.get"
    );
    assert_eq!(jail.read_holder.lock().unwrap().manifest_dials, 0);
    assert!(
        !shim_state(&jail.base).exists(),
        "an all-off attachment created policy state"
    );
}

#[tokio::test]
async fn gh_shim_attached_head_without_first_pin_refuses_before_upstream() {
    let mut jail = Jail::start_with_layout_and_env(
        None,
        None,
        true,
        "strict",
        false,
        false,
        |base, command| {
            use std::os::unix::fs::PermissionsExt;
            let config = base.join("xdg/config/cortexkit/aft.jsonc");
            std::fs::write(
                config,
                r#"{"github":{"shim":true},"git":{"co_author":"off"}}"#,
            )
            .unwrap();
            std::fs::write(base.join("hold-policy-root"), b"").unwrap();
            let gh = base.join("bin/gh");
            std::fs::write(
                &gh,
                format!(
                    "#!/bin/sh\nprintf 'hit' > {}\nexit 73\n",
                    quote(&base.join("upstream-record"))
                ),
            )
            .unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            let path = std::env::join_paths(
                std::iter::once(base.join("bin"))
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap();
            command
                .env("PATH", path)
                .env_remove("SUBC_CONNECTION_FILE")
                .env("SUBC_MODULE_ID", "aft")
                .env("SUBC_LAUNCH_NONCE", "gh-shim-cold-fixture");
        },
    )
    .await;
    jail.bind_grant_head().await;
    let reply = jail.tool_reply_on_channel(2, "bash", json!({"description":"Check refusal before the first pin", "command":"printf 'marker=%s anchor=%s\\n' \"$AFT_GH_SHIM_ENABLED\" \"${AFT_GH_SHIM_TRUST_ANCHOR-unset}\"; gh issue comment 1 --body test; printf 'exit=%s\\n' \"$?\""})).await;
    let text = model_text(&reply);
    assert!(text.contains("marker=1 anchor=unset"), "{text}");
    assert!(
        text.contains("exit=86") && text.contains("no usable pinned policy root"),
        "{text}"
    );
    assert!(!jail.base.join("upstream-record").exists());
}

#[tokio::test]
async fn gh_shim_attached_refresh_pins_when_an_enabled_harness_overrides_a_top_level_off() {
    let mut jail = Jail::start_with_layout_and_env(
        None,
        None,
        true,
        "strict",
        false,
        false,
        |base, command| {
            worker_gh_read::configure(base, command);
            std::fs::write(base.join("defer-policy-root"), b"").unwrap();
            let config = base.join("xdg/config/cortexkit/aft.jsonc");
            let mut value: Value =
                serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
            value["github"] = json!({"shim":false});
            value["harnesses"] = json!({"opencode":{"github":{"shim":true}}});
            std::fs::write(config, serde_json::to_vec(&value).unwrap()).unwrap();
        },
    )
    .await;
    assert_eq!(jail.policy_root.reads, 0, "the default harness is disabled");
    jail.bind_head_for_harness(jail.base.clone(), "opencode")
        .await;
    jail.wait_for_fixture_pin().await;
    let reply = jail.tool_reply_on_channel(2, "bash", json!({"description":"Check the harness policy pin", "command":"test \"$AFT_GH_SHIM_ENABLED\" = 1 && test -n \"$AFT_GH_SHIM_TRUST_ANCHOR\" && printf 'HARNESS_PINNED'"})).await;
    let text = model_text(&reply);
    assert!(text.contains("HARNESS_PINNED"), "{text}");
}
