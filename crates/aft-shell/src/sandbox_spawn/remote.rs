//! Remote shells inherit neither the normal shell profile nor its environment.

use super::SandboxHost;
use crate::tool_call_result::ToolCallResult;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

#[doc(hidden)]
pub struct Output {
    text: String,
    code: i32,
    pub timed_out: bool,
}

impl Output {
    #[doc(hidden)]
    pub fn result(self, id: &str, line: Option<String>) -> ToolCallResult {
        let line = line
            .as_deref()
            .or(self.timed_out.then_some("[command timed out]"));
        let text = match line {
            Some(line) if self.text.is_empty() => line.to_owned(),
            Some(line) => format!("{}\n{line}", self.text),
            None => self.text,
        };
        let data =
            serde_json::json!({"output":text,"exitCode":self.code,"timedOut":self.timed_out});
        let response = if self.code == 0 && !self.timed_out && line.is_none() {
            crate::protocol::Response::success(id, data)
        } else {
            let (code, message) = match line {
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
#[doc(hidden)]
pub use unix::prepare;
#[cfg(all(target_os = "linux", any(test, feature = "test-support")))]
pub(crate) use unix::prepare_with_launcher;
#[cfg(all(target_os = "linux", feature = "test-support"))]
pub(crate) use unix::Start as StartForTest;

#[cfg(all(target_os = "linux", feature = "test-hooks"))]
pub async fn run_for_test(
    ctx: &dyn SandboxHost,
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
    .start(timeout)?
    .wait(CancellationToken::new())
    .await
    .map(|output| output.result("test", None))
}

#[cfg(not(target_os = "linux"))]
#[doc(hidden)]
pub fn prepare(_: &dyn SandboxHost, _: &Path, _: PathBuf, _: String) -> Result<Start, String> {
    Err("no remote shell sandbox backend".into())
}

#[cfg(not(target_os = "linux"))]
#[doc(hidden)]
pub struct Start;
#[cfg(not(target_os = "linux"))]
impl Start {
    pub fn start(self, _: Duration) -> Result<Running, String> {
        Err("no remote shell sandbox backend".into())
    }
}
#[cfg(not(target_os = "linux"))]
#[doc(hidden)]
pub struct Running;
#[cfg(not(target_os = "linux"))]
impl Running {
    pub async fn wait(self, _: CancellationToken) -> Result<Output, String> {
        Err("no remote shell sandbox backend".into())
    }
}
