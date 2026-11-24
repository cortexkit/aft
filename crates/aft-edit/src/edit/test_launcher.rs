use super::tool_launcher::{
    run_prepared, ToolCommand, ToolEnvironment, ToolLauncher, ToolOutcome, ToolRefusal,
};
use std::process::Command;

pub(crate) struct Unconfined;

fn prepare(command: &ToolCommand) -> Result<Command, String> {
    #[cfg(windows)]
    let mut prepared =
        if crate::windows_command::is_batch_file(std::path::Path::new(&command.program)) {
            crate::windows_command::batch_command(
                std::path::Path::new(&command.program),
                &command.args,
            )
            .map_err(|error| error.to_string())?
        } else {
            effective_command(command)
        };
    #[cfg(not(windows))]
    let mut prepared = effective_command(command);

    if matches!(command.environment, ToolEnvironment::Clean) {
        prepared.env_clear();
        if let Some(path) = std::env::var_os("PATH") {
            prepared.env("PATH", path);
        }
    }
    Ok(prepared)
}

fn effective_command(command: &ToolCommand) -> Command {
    let mut prepared = crate::effective_path::new_command(&command.program).build();
    prepared.args(&command.args);
    prepared
}

impl ToolLauncher for Unconfined {
    fn check(&self, command: &ToolCommand) -> Result<(), ToolRefusal> {
        prepare(command).map(|_| ()).map_err(ToolRefusal)
    }

    fn run(&self, command: ToolCommand) -> ToolOutcome {
        match prepare(&command) {
            Ok(prepared) => run_prepared(prepared, command),
            Err(reason) => ToolOutcome::Refused(ToolRefusal(reason)),
        }
    }
}
