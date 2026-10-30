use crate::bash_background::watches::WatchPattern;
use crate::context::AppContext;
use crate::protocol::{RawRequest, Response};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize)]
struct BashNotifyParams {
    task_id: String,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    regex: Option<String>,
    #[serde(default = "default_once")]
    once: bool,
    #[serde(default)]
    deliver: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BashUnnotifyParams {
    task_id: String,
    watch_id: String,
}

fn default_once() -> bool {
    true
}

pub fn handle(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashNotifyParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_notify: invalid params: {e}"),
            );
        }
    };

    if params
        .deliver
        .as_deref()
        .is_some_and(|value| value != "each_line")
    {
        return Response::error(
            &req.id,
            "invalid_request",
            "bash_watch: deliver must be each_line",
        );
    }
    let pattern = if let Some(regex) = params.regex.as_deref() {
        match WatchPattern::regex(regex) {
            Ok(pattern) => pattern,
            Err(e) => {
                return Response::error(
                    &req.id,
                    "invalid_request",
                    format!("bash_notify: invalid regex: {e}"),
                );
            }
        }
    } else if let Some(pattern) = params.pattern {
        WatchPattern::Substring(pattern)
    } else {
        return Response::error(
            &req.id,
            "invalid_request",
            "bash_notify: missing pattern or regex",
        );
    };

    // Task ownership is immutable. Check it before registration can mark the
    // owner's delivery session live or consume its watch quota.
    let registration = if ctx
        .bash_background()
        .observed_status(&params.task_id, req.session(), 0)
        .is_some()
    {
        ctx.bash_background().register_watch_delivery(
            params.task_id.clone(),
            pattern,
            params.once,
            params.deliver.is_some(),
        )
    } else {
        Err("task_not_found")
    };
    match registration {
        Ok(watch_id) => Response::success(&req.id, json!({ "watch_id": watch_id })),
        Err("too_many_watches") => Response::error(
            &req.id,
            "too_many_watches",
            format!("Too many active watches on task {} (max 8)", params.task_id),
        ),
        Err("task_not_found") => {
            let unadopted = ctx.config().project_root.as_deref().and_then(|root| {
                ctx.bash_background()
                    .unadopted_task_message(&params.task_id, req.session(), root)
            });
            match unadopted {
                Some(message) => Response::error(&req.id, "task_not_adopted", message),
                None => Response::error(&req.id, "invalid_request", "task_not_found"),
            }
        }
        Err(error) => Response::error(&req.id, "invalid_request", error),
    }
}

pub fn handle_unnotify(req: &RawRequest, ctx: &AppContext) -> Response {
    let raw_params = req
        .params
        .get("params")
        .cloned()
        .unwrap_or_else(|| req.params.clone());
    let params = match serde_json::from_value::<BashUnnotifyParams>(raw_params) {
        Ok(params) => params,
        Err(e) => {
            return Response::error(
                &req.id,
                "invalid_request",
                format!("bash_unnotify: invalid params: {e}"),
            );
        }
    };
    ctx.bash_background()
        .unregister_watch(&params.task_id, &params.watch_id);
    Response::success(&req.id, json!({ "unregistered": true }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn notify_rejects_other_session_without_consuming_owner_quota() {
        let project = tempfile::tempdir().unwrap();
        let storage = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config {
            project_root: Some(project.path().into()),
            storage_dir: Some(storage.path().into()),
            ..crate::config::Config::default()
        };
        config.sandbox.profile.head = crate::config::SandboxMode::Off;
        let ctx = AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
        let spawn = serde_json::from_value(json!({
            "id": "notify-owner-task",
            "command": "bash",
            "session_id": "owner",
            "params": { "command": "sleep 30" },
        }))
        .unwrap();
        let launched = crate::commands::bash::handle(&spawn, &ctx);
        assert!(launched.success, "{:?}", launched.data);
        let task_id = launched.data["task_id"].as_str().unwrap().to_string();
        struct Cleanup<'a>(&'a AppContext, String);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.bash_background().kill(&self.1, "owner");
            }
        }
        let _cleanup = Cleanup(&ctx, task_id.clone());
        let request = |session: &str| {
            serde_json::from_value(json!({
                "id": "notify-session-test",
                "command": "bash_notify",
                "session_id": session,
                "params": { "task_id": task_id, "pattern": "never-emitted" },
            }))
            .unwrap()
        };

        for _ in 0..8 {
            let denied = handle(&request("other"), &ctx);
            assert!(
                !denied.success,
                "another session registered a watch: {:?}",
                denied.data
            );
            assert_eq!(denied.data["message"], "task_not_found");
        }
        for _ in 0..8 {
            let allowed = handle(&request("owner"), &ctx);
            assert!(
                allowed.success,
                "owner quota was consumed: {:?}",
                allowed.data
            );
            assert!(allowed.data["watch_id"].as_str().is_some());
        }
        assert_eq!(
            handle(&request("owner"), &ctx).data["code"],
            "too_many_watches"
        );
    }
}
