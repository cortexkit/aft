//! The final text budget is shared by every transport, not imposed by the host.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;

use crate::context::AppContext;
use crate::protocol::Response;

pub const TEXT_LIMIT: usize = 50 * 1024;
pub const SAVED_FILE_LIMIT: usize = 64 * 1024 * 1024;
pub const CUT_TRAILER: &str = "[output cut at 50 KB of <total> KB; full result saved to <path>; read it with read and a line range]";

struct Artifact {
    session: String,
    file: Mutex<File>,
}

#[derive(Default)]
pub(crate) struct OutputFiles {
    files: Mutex<HashMap<PathBuf, Artifact>>,
}

impl OutputFiles {
    fn save(&self, ctx: &AppContext, session: &str, text: &str) -> io::Result<PathBuf> {
        if text.len() > SAVED_FILE_LIMIT {
            return Err(io::Error::other(
                "full tool result exceeds the 64 MiB saved-file limit",
            ));
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = ctx.storage_dir();
        let dir = root
            .join(
                ctx.harness_opt()
                    .map_or("standalone", |h| h.storage_segment()),
            )
            .join("tool-results")
            .join(crate::backup::hash_session(session));
        crate::production_storage::refuse_write(&dir)?;
        crate::private_storage::open_dir(&root, &dir)?;
        let dir = dir.canonicalize()?;
        let mut files = self.files.lock();
        loop {
            let path = dir.join(format!(
                "{}-{}-{}.txt",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let mut file = match crate::private_storage::options()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            };
            if let Err(error) = file.write_all(text.as_bytes()) {
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(error);
            }
            let path = crate::access_policy::policy_path(&path);
            files.insert(
                path.clone(),
                Artifact {
                    session: session.to_owned(),
                    file: Mutex::new(file),
                },
            );
            return Ok(path);
        }
    }

    pub(crate) fn owner(&self, path: &Path) -> Option<String> {
        let path = crate::access_policy::policy_path(path);
        self.files
            .lock()
            .get(&path)
            .map(|artifact| artifact.session.clone())
    }

    pub(crate) fn read(&self, session: &str, path: &Path) -> Option<io::Result<Vec<u8>>> {
        let path = crate::access_policy::policy_path(path);
        let files = self.files.lock();
        let artifact = files.get(&path)?;
        Some((|| {
            if artifact.session != session {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "tool result belongs to another session",
                ));
            }
            let mut file = artifact.file.lock();
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if file.metadata()?.nlink() != 1 {
                    return Err(io::Error::other("tool result has multiple hard links"));
                }
            }
            file.seek(SeekFrom::Start(0))?;
            let mut bytes = Vec::new();
            (&mut *file)
                .take(SAVED_FILE_LIMIT as u64 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > SAVED_FILE_LIMIT {
                return Err(io::Error::other("tool result exceeds saved-file limit"));
            }
            Ok(bytes)
        })())
    }

    pub(crate) fn cleanup_session(&self, session: &str) {
        let mut files = self.files.lock();
        let paths: Vec<_> = files
            .iter()
            .filter(|(_, artifact)| artifact.session == session)
            .map(|(path, _)| path.clone())
            .collect();
        for path in paths {
            // Windows requires the reader handle to close before unlinking.
            drop(files.remove(&path));
            let _ = std::fs::remove_file(&path);
            if let Some(dir) = path.parent() {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }
}

/// Called after reminders and the status bar, so the host never cuts off recovery text.
pub(crate) fn cap(
    ctx: &AppContext,
    session: &str,
    tool: &str,
    response: &Response,
    text: &mut String,
) {
    if text.len() <= TEXT_LIMIT {
        return;
    }
    let total = text.len();
    let existing = matches!(tool, "bash" | "bash_watch" | "bash_status")
        .then(|| response.data.get("output_path").and_then(serde_json::Value::as_str))
        .flatten()
        .map(PathBuf::from)
        .filter(|path| ctx.bash_background().is_session_owned_artifact_path(session, path))
        // A task capture is reusable only if it contains the entire rendered reply,
        // not just stdout without stderr, formatting or appended reminders.
        .filter(|path| {
            std::fs::metadata(path).is_ok_and(|meta| meta.len() == total as u64)
                && ctx.bash_background().read_artifact_path(session, path)
                    .is_some_and(|result| result.is_ok_and(|bytes| bytes == text.as_bytes()))
        });
    let path = existing
        .map(Ok)
        .unwrap_or_else(|| ctx.output_files().save(ctx, session, text));
    let trailer = match path {
        Ok(path) => CUT_TRAILER
            .replace("<total>", &format!("{:.1}", total as f64 / 1024.0))
            .replace("<path>", &path.display().to_string()),
        Err(error) => {
            log::error!("could not save {tool} result for {session}: {error}");
            *text = format!(
                "Tool result unavailable: could not save the full {total}-byte result: {error}"
            );
            return;
        }
    };
    // A list's pagination contract remains visible even if its rows exceed
    // the transport budget. Save the original text, including these lines.
    let envelopes: Vec<_> = text
        .lines()
        .filter(|line| line.starts_with("shown ") && line.contains(" · narrow:"))
        .collect();
    let envelope = envelopes.join("\n");
    let envelope_bytes = if envelope.is_empty() {
        0
    } else {
        envelope.len() + 1
    };
    if trailer.len() + envelope_bytes + 1 > TEXT_LIMIT {
        // Pathological batches can contain more trailers than fit in a reply.
        // They remain intact in the saved result rather than hiding recovery.
        render_cut(text, &trailer, "");
    } else {
        render_cut(text, &trailer, &envelope);
    }
}

fn render_cut(text: &mut String, trailer: &str, envelope: &str) {
    let envelope_bytes = if envelope.is_empty() {
        0
    } else {
        envelope.len() + 1
    };
    let mut end = TEXT_LIMIT.saturating_sub(trailer.len() + envelope_bytes + 1);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if let Some(newline) = text[..end].rfind('\n') {
        end = newline;
    }
    text.truncate(end);
    if !envelope.is_empty() {
        text.push('\n');
        text.push_str(envelope);
    }
    text.push('\n');
    text.push_str(trailer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn context(root: &Path, storage: &Path) -> AppContext {
        let ctx = AppContext::new(
            Box::new(crate::parser::TreeSitterProvider::new()),
            crate::config::Config {
                project_root: Some(root.to_path_buf()),
                storage_dir: Some(storage.to_path_buf()),
                restrict_to_project_root: true,
                ..Default::default()
            },
        )
        .with_test_checkpoint_store();
        ctx.set_harness(crate::harness::Harness::Opencode);
        ctx
    }

    fn saved_path(text: &str) -> PathBuf {
        PathBuf::from(
            text.rsplit_once("; full result saved to ")
                .unwrap()
                .1
                .strip_suffix("; read it with read and a line range]")
                .unwrap(),
        )
    }

    fn check(tool: &str, data: Value) {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let response = Response::success("cap", data.clone());
        let format =
            crate::subc_format::FormatContext::from_tool_call(tool, &json!({}), temp.path());
        let original = crate::subc_format::format_response_with_context(tool, &response, &format);
        assert!(
            original.len() > TEXT_LIMIT,
            "fixture did not exceed budget for {tool}"
        );
        let mut text = original.clone();
        cap(&ctx, "owner", tool, &response, &mut text);
        let path = saved_path(&text);
        assert!(path.is_absolute());
        assert!(path.starts_with(ctx.harness_dir().join("tool-results")));
        assert!(text.len() <= 51_200, "{tool}: {}", text.len());
        let trailer = format!("[output cut at 50 KB of {:.1} KB; full result saved to {}; read it with read and a line range]", original.len() as f64 / 1024.0, path.display());
        assert!(text.ends_with(&trailer), "{tool}: incorrect trailer");
        assert_eq!(std::fs::read(&path).unwrap(), original.as_bytes());
        assert_eq!(response.data, data, "structured content must remain uncut");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn tool_output_cap_zoom_batch() {
        let symbols: Vec<_> = (0..300)
            .map(|i| {
                json!({
                    "name": format!("symbol_{i}"),
                    "response": {"success":true, "name":format!("symbol_{i}"), "kind":"function",
                        "range":{"start_line":1,"end_line":10}, "content":"line();\n".repeat(100)}
                })
            })
            .collect();
        check("zoom", json!({"symbols":symbols}));
    }

    #[test]
    fn tool_output_cap_outline() {
        check(
            "outline",
            json!({"text":"  function outline_symbol() 1:20\n".repeat(6000)}),
        );
    }

    #[test]
    fn tool_output_cap_conflicts() {
        check(
            "conflicts",
            json!({"text":"1: <<<<<<< HEAD\n2: ours\n3: =======\n4: theirs\n5: >>>>>>> branch\n".repeat(3000)}),
        );
    }

    #[test]
    fn tool_output_cap_ast_search() {
        let matches: Vec<_> = (0..4000).map(|i| json!({"file":"src/main.ts", "line":i+1, "text":"console.log(long_variable_name);"})).collect();
        check(
            "ast_search",
            json!({"matches":matches, "total_matches":4000,"files_with_matches":1,"files_searched":1,"complete":true}),
        );
    }

    #[test]
    fn tool_output_cap_bash_status() {
        check(
            "bash_status",
            json!({"task_id":"bash-0123456789abcdef", "mode":"pipes", "status":"completed", "exit_code":0, "output_preview":"status output\n".repeat(12000)}),
        );
    }

    #[test]
    fn tool_output_cap_bash_and_watch_two_megabyte_line() {
        let output = "x".repeat(2 * 1024 * 1024);
        check(
            "bash",
            json!({"output":format!("Task bash-0123456789abcdef: completed\n{output}")}),
        );
        check(
            "bash_watch",
            json!({"output":format!("Task bash-0123456789abcdef: completed\nWaited 1ms; task exited.\n{output}"), "output_preview":output}),
        );
    }

    #[test]
    fn tool_output_cap_small_results_are_byte_identical() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        for original in ["", "already formatted\n", &"界".repeat(17066)] {
            let mut text = original.to_owned();
            cap(
                &ctx,
                "owner",
                "zoom",
                &Response::success("id", json!({})),
                &mut text,
            );
            assert_eq!(text, original);
        }
        assert!(ctx.output_files().files.lock().is_empty());
    }

    #[test]
    fn tool_output_cap_utf8_and_line_boundaries_and_list_envelope() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let response = Response::success("id", json!({}));
        for original in ["🦀界".repeat(20000), "a complete line 🦀\n".repeat(10000)] {
            let mut text = original.clone();
            cap(&ctx, "owner", "zoom", &response, &mut text);
            let prefix = text.split_once("\n[output cut").unwrap().0;
            assert!(original.starts_with(prefix));
            assert!(!prefix.contains('\u{fffd}'));
            if original.contains('\n') {
                assert!(prefix.ends_with("🦀"));
            } else {
                assert!(text.len() >= TEXT_LIMIT - 6);
            }
        }
        let envelope = "shown 100 of 200 symbols (cap) · narrow: symbols";
        let mut text = format!("{}\n{envelope}", "row\n".repeat(20000));
        cap(&ctx, "owner", "zoom", &response, &mut text);
        assert!(text.contains(&format!("\n{envelope}\n[output cut")));
        assert!(text.len() <= TEXT_LIMIT);
    }

    #[test]
    fn tool_output_cap_owner_reads_range_while_confined_foreign_session_refused() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let ctx = context(&root, &temp.path().join("store"));
        let original = format!("first\nsecond\n{}", "later\n".repeat(20000));
        let mut text = original.clone();
        cap(
            &ctx,
            "owner",
            "zoom",
            &Response::success("id", json!({})),
            &mut text,
        );
        let path = saved_path(&text);
        let request = |session: &str| crate::protocol::RawRequest {
            id: "read-cap".into(),
            command: "read".into(),
            lsp_hints: None,
            session_id: Some(session.into()),
            params: json!({"file":path, "start_line":2,"end_line":2}),
        };
        let response = crate::context::confinement::with_root(&ctx, "read-cap", || {
            Ok(crate::commands::read::handle(&request("owner"), &ctx))
        })
        .unwrap();
        assert!(response.success, "{response:?}");
        assert_eq!(response.data["content"], "2: second\n");
        let foreign = crate::commands::read::handle(&request("foreign"), &ctx);
        assert!(!foreign.success, "{foreign:?}");
        // No root restriction must not turn another session's result into a public file.
        ctx.with_force_restrict("read-cap", || {
            assert!(ctx
                .validate_read_path("read-cap", "foreign", &path)
                .is_err());
        });
        ctx.output_files().cleanup_session("foreign");
        assert!(path.exists());
        ctx.output_files().cleanup_session("owner");
        assert!(!path.exists());
    }

    #[test]
    fn tool_output_cap_saved_file_limit_refuses_instead_of_claiming_full_result() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let mut text = "x".repeat(SAVED_FILE_LIMIT + 1);
        cap(
            &ctx,
            "owner",
            "zoom",
            &Response::success("id", json!({})),
            &mut text,
        );
        assert!(text.len() <= TEXT_LIMIT);
        assert!(text.contains("64 MiB saved-file limit"));
        assert!(!text.contains("full result saved"));
        assert!(ctx.output_files().files.lock().is_empty());
    }

    #[test]
    fn tool_output_cap_golden() {
        let temp = tempfile::tempdir().unwrap();
        let ctx = context(temp.path(), &temp.path().join("store"));
        let mut text = format!("header\n{}", "x".repeat(65536 - 7));
        cap(
            &ctx,
            "owner",
            "conflicts",
            &Response::success("id", json!({})),
            &mut text,
        );
        let path = saved_path(&text);
        let normalized = text.replace(&path.display().to_string(), "<path>");
        assert_eq!(
            normalized,
            include_str!("../tests/fixtures/tool-result-cap.txt").trim_end()
        );
    }
}
