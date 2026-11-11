//! The launch contract used by formatters and syntax checkers.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::format::{FormatError, SubprocessOutcome};

pub trait ToolLauncher {
    /// Check policy even when a cached probe result means no process will run.
    fn check(&self, command: &ToolCommand) -> Result<(), ToolRefusal>;
    /// Apply the policy for this launch; callers need not call `check` first.
    fn run(&self, command: ToolCommand) -> ToolOutcome;
}

pub struct ToolRefusal(pub String);

pub enum ToolEnvironment {
    Inherited,
    Clean,
}

pub enum ToolStdin {
    Inherited,
    Null,
    Source {
        source: String,
        may_stop_reading: bool,
    },
}

pub enum ToolTimeout {
    /// Formatter deadlines start after spawning, not while preparing the policy.
    AfterSpawn(u32),
    /// Syntax parsers start their budget after launch-policy preparation.
    AfterPreparation(Duration),
    Deadline(Instant),
}

pub struct ToolCommand {
    pub tool: String,
    pub program: OsString,
    pub args: Vec<OsString>,
    pub cwd: Option<PathBuf>,
    pub stdin: ToolStdin,
    pub environment: ToolEnvironment,
    pub timeout: ToolTimeout,
}

impl ToolCommand {
    pub(crate) fn external(
        program: &str,
        args: &[&str],
        cwd: Option<&std::path::Path>,
        timeout_secs: u32,
    ) -> Self {
        Self {
            tool: program.into(),
            program: program.into(),
            args: args.iter().map(OsString::from).collect(),
            cwd: cwd.map(std::path::Path::to_path_buf),
            stdin: ToolStdin::Inherited,
            environment: ToolEnvironment::Inherited,
            timeout: ToolTimeout::AfterSpawn(timeout_secs),
        }
    }
}

pub enum ToolOutcome {
    Completed {
        status: ExitStatus,
        stdout: String,
        stderr: String,
        truncated: bool,
    },
    Refused(ToolRefusal),
    NotFound,
    Timeout {
        timeout_secs: u32,
    },
    Failed {
        stderr: String,
    },
}

impl ToolOutcome {
    pub(crate) fn from_result(result: Result<SubprocessOutcome, FormatError>) -> Self {
        match result {
            Ok(output) => Self::Completed {
                status: output.status,
                stdout: output.stdout,
                stderr: output.stderr,
                truncated: output.truncated,
            },
            Err(FormatError::NotFound { .. }) => Self::NotFound,
            Err(FormatError::Timeout { timeout_secs, .. }) => Self::Timeout { timeout_secs },
            Err(FormatError::Failed { stderr, .. }) => Self::Failed { stderr },
            Err(FormatError::UnsupportedLanguage) => unreachable!("a subprocess has a tool"),
        }
    }

    pub(crate) fn into_result(self, tool: &str) -> Result<SubprocessOutcome, FormatError> {
        match self {
            Self::Completed {
                status,
                stdout,
                stderr,
                truncated,
            } => Ok(SubprocessOutcome {
                status,
                stdout,
                stderr,
                truncated,
            }),
            Self::Refused(ToolRefusal(stderr)) | Self::Failed { stderr } => {
                Err(FormatError::Failed {
                    tool: tool.into(),
                    stderr,
                })
            }
            Self::NotFound => Err(FormatError::NotFound { tool: tool.into() }),
            Self::Timeout { timeout_secs } => Err(FormatError::Timeout {
                tool: tool.into(),
                timeout_secs,
            }),
        }
    }
}

/// Execute only after the caller has prepared the command under its launch policy.
pub(crate) fn run_prepared(mut prepared: Command, mut command: ToolCommand) -> ToolOutcome {
    if let ToolTimeout::AfterPreparation(duration) = command.timeout {
        command.timeout = ToolTimeout::Deadline(Instant::now() + duration);
    }
    let tool = command.tool;
    if let Some(cwd) = command.cwd {
        prepared.current_dir(cwd);
    }
    let result = match command.stdin {
        ToolStdin::Source {
            source,
            may_stop_reading,
        } => {
            let ToolTimeout::Deadline(deadline) = command.timeout else {
                unreachable!("source checks have an absolute deadline")
            };
            super::swift::run_command(prepared, &tool, &source, deadline, may_stop_reading)
        }
        ToolStdin::Null => {
            let ToolTimeout::Deadline(deadline) = command.timeout else {
                unreachable!("probes have an absolute deadline")
            };
            crate::format::capture_probe_command(prepared, &tool, deadline, || {
                crate::executor::current_job_cancelled()
            })
        }
        ToolStdin::Inherited => {
            prepared.stdout(Stdio::piped()).stderr(Stdio::piped());
            crate::format::isolate_in_process_group(&mut prepared);
            prepared
                .spawn()
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        FormatError::NotFound {
                            tool: tool.to_string(),
                        }
                    } else {
                        FormatError::Failed {
                            tool: tool.to_string(),
                            stderr: error.to_string(),
                        }
                    }
                })
                .and_then(|child| {
                    let deadline = match command.timeout {
                        ToolTimeout::AfterSpawn(seconds) => {
                            Instant::now() + Duration::from_secs(seconds as u64)
                        }
                        ToolTimeout::Deadline(deadline) => deadline,
                        ToolTimeout::AfterPreparation(_) => {
                            unreachable!("deadline resolved before spawn")
                        }
                    };
                    crate::format::wait_with_deadline(child, &tool, deadline, || false)
                })
        }
    };
    ToolOutcome::from_result(result)
}
