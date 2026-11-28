use crate::{config::Config, protocol::RawRequest};
use serde_json::json;
use std::os::unix::fs::FileTypeExt;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn fifo_mutations_refuse_without_a_writer() {
    const CHILD: &str = "AFT_TEST_FIFO_MUTATION_CHILD";
    if let Ok(action) = std::env::var(CHILD) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("pipe.txt");
        let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: c_path is a valid NUL-terminated pathname.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        let ctx = crate::test_context(
            Box::new(crate::parser::TreeSitterProvider::new()),
            Config {
                project_root: Some(temp.path().to_path_buf()),
                storage_dir: Some(temp.path().join("state")),
                format_on_edit: false,
                ..Config::default()
            },
        );
        let mut req = RawRequest {
            id: action.clone(),
            command: action.clone(),
            params: json!({"file": path, "content": "new\n", "match": "old", "replacement": "new"}),
            lsp_hints: None,
            session_id: None,
        };
        let reply = match action.as_str() {
            "write" => super::write::handle_write(&req, &ctx),
            "edit_match" => super::edit_match::handle_edit_match(&req, &ctx),
            "patch-delete" | "patch-update" => {
                req.command = "apply_patch".to_string();
                req.params = json!({"patch_text": if action == "patch-delete" {
                    "*** Begin Patch\n*** Delete File: pipe.txt\n*** End Patch"
                } else {
                    "*** Begin Patch\n*** Update File: pipe.txt\n@@\n-old\n+new\n*** End Patch"
                }});
                super::apply_patch::handle_apply_patch(&req, &ctx)
            }
            _ => panic!("unexpected test action {action}"),
        };
        assert!(!reply.success, "{reply:?}");
        assert!(
            reply.data.to_string().contains("not a regular file"),
            "{reply:?}"
        );
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_fifo());
        return;
    }

    // Isolate each call so a regression that blocks in open can be killed
    // rather than hanging the suite or supplying a writer to the FIFO.
    let mut failures = Vec::new();
    for action in ["write", "edit_match", "patch-delete", "patch-update"] {
        let mut child = Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "commands::fifo_mutation_tests::fifo_mutations_refuse_without_a_writer",
                "--nocapture",
            ])
            .env(CHILD, action);
        match aft_engine::test_bridge::search_misc::capture_probe_command_for_test(
            child,
            action,
            Instant::now() + Duration::from_secs(5),
        ) {
            Ok((status, stdout, _)) if status.success() && stdout.contains("1 passed") => {}
            Ok((_, stdout, stderr)) => failures.push(format!("{action}: {stdout}\n{stderr}")),
            Err(error) => failures.push(format!("{action}: {error}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
