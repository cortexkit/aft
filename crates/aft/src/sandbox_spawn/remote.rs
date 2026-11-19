//! Remote shells inherit neither the normal shell profile nor its environment.

use crate::{context::AppContext, run_tool_call::ToolCallResult};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

pub(crate) struct Output {
    text: String,
    code: i32,
    pub(crate) timed_out: bool,
}

impl Output {
    pub(crate) fn result(self, id: &str, line: Option<String>) -> ToolCallResult {
        let text = match line.as_deref() {
            Some(line) if self.text.is_empty() => line.to_owned(),
            Some(line) => format!("{}\n{line}", self.text),
            None => self.text,
        };
        let data =
            serde_json::json!({"output":text,"exitCode":self.code,"timedOut":self.timed_out});
        let response = if self.code == 0 && !self.timed_out && line.is_none() {
            crate::protocol::Response::success(id, data)
        } else {
            let (code, message) = match line.as_deref() {
                Some(message) if self.timed_out => ("timeout", message),
                Some(message) => ("permission_denied", message),
                None if self.timed_out => ("timeout", "Command timed out."),
                None => ("command_failed", "Command exited unsuccessfully."),
            };
            crate::protocol::Response::error_with_data(id, code, message, data)
        };
        ToolCallResult { text, response }
    }
}

#[cfg(target_os = "linux")]
#[path = "remote_unix.rs"]
mod unix;
#[cfg(target_os = "linux")]
pub(crate) use unix::prepare;

#[cfg(all(target_os = "linux", feature = "test-hooks"))]
pub async fn run_for_test(
    ctx: &AppContext,
    folder: &Path,
    launcher: &Path,
    command: String,
    timeout: Duration,
) -> Result<ToolCallResult, String> {
    unix::prepare_with_launcher(
        ctx,
        folder,
        folder.to_path_buf(),
        command,
        launcher.to_path_buf(),
    )?
    .start()?
    .wait(timeout, CancellationToken::new())
    .await
    .map(|output| output.result("test", None))
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn prepare(_: &AppContext, _: &Path, _: PathBuf, _: String) -> Result<Start, String> {
    Err("no remote shell sandbox backend".into())
}

#[cfg(not(target_os = "linux"))]
pub(crate) struct Start;
#[cfg(not(target_os = "linux"))]
impl Start {
    pub(crate) fn start(self) -> Result<Running, String> {
        Err("no remote shell sandbox backend".into())
    }
}
#[cfg(not(target_os = "linux"))]
pub(crate) struct Running;
#[cfg(not(target_os = "linux"))]
impl Running {
    pub(crate) async fn wait(self, _: Duration, _: CancellationToken) -> Result<Output, String> {
        Err("no remote shell sandbox backend".into())
    }
}
