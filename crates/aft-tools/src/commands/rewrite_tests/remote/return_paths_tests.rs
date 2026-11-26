use super::*;

#[tokio::test]
async fn runon_return_paths_can_copy_back_outside_cwd_but_inside_the_checkout() {
    const CAPTURE: &str = ".cortexkit/internal-docs/research/x/captures/a.png";
    const REQUESTED: &[&str] = &[
        "../.cortexkit/internal-docs/research/x/captures/a.png",
        "dist/absent.txt",
    ];
    let daemon = aft_engine::test_bridge::bash_remote::remote_wire::daemon_with_capabilities(
        Script::ReturnVectorReporting {
            vector: "failed-missing",
            paths: REQUESTED,
            changed: &[CAPTURE, "rune/dist/absent.txt", "rune/other.txt"],
        },
        RETURN_CLAIMS,
        1,
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ctx = restarted_context(dir.path());
    fs::create_dir(dir.path().join("rune")).unwrap();
    let mut call = return_call(REQUESTED);
    call["workdir"] = serde_json::json!("rune");
    let (reply, _) = orchestrated_reply(&ctx, &daemon, call).await;
    assert!(reply.success, "{reply:?}");
    assert_eq!(
        exec_runs(&daemon)[0]["return_paths"],
        serde_json::json!(REQUESTED)
    );
    assert_eq!(fs::read(dir.path().join(CAPTURE)).unwrap(), b"hello");
    let output = reply.data["output"].as_str().unwrap();
    assert!(
        output.contains(
            "These files changed on the server and were NOT copied back:\n  \
             rune/dist/absent.txt\n  rune/other.txt"
        ),
        "{output}"
    );
    assert!(!output.contains("untracked files"), "{output}");
    assert!(
        !output.lines().any(|line| line.trim() == CAPTURE),
        "{output}"
    );
}
