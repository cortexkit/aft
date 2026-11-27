use super::*;

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
    let reply = jail.tool_reply_on_channel(2, "bash", json!({"command":"printf 'marker=%s anchor=%s\\n' \"$AFT_GH_SHIM_ENABLED\" \"${AFT_GH_SHIM_TRUST_ANCHOR-unset}\"; gh issue comment 1 --body test; printf 'exit=%s\\n' \"$?\""})).await;
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
            let config = base.join("xdg/config/cortexkit/aft.jsonc");
            let mut value: Value =
                serde_json::from_slice(&std::fs::read(&config).unwrap()).unwrap();
            value["github"] = json!({"shim":false});
            value["harnesses"] = json!({"opencode":{"github":{"shim":true}}});
            std::fs::write(config, serde_json::to_vec(&value).unwrap()).unwrap();
        },
    )
    .await;
    jail.bind_head_for_harness(jail.base.clone(), "opencode")
        .await;
    let reply = jail.tool_reply_on_channel(2, "bash", json!({"command":"test \"$AFT_GH_SHIM_ENABLED\" = 1 && test -n \"$AFT_GH_SHIM_TRUST_ANCHOR\" && printf 'HARNESS_PINNED'"})).await;
    let text = model_text(&reply);
    assert!(text.contains("HARNESS_PINNED"), "{text}");
}
