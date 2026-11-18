use super::*;

impl crate::edit::tool_launcher::ToolLauncher for ToolLaunchPolicy<'_> {
    fn check(
        &self,
        command: &crate::edit::tool_launcher::ToolCommand,
    ) -> Result<(), crate::edit::tool_launcher::ToolRefusal> {
        match command.environment {
            crate::edit::tool_launcher::ToolEnvironment::Inherited => {
                self.command(&command.program, &command.args)
            }
            crate::edit::tool_launcher::ToolEnvironment::Clean => {
                self.command_with_clean_environment(&command.program, &command.args)
            }
        }
        .map(|_| ())
        .map_err(crate::edit::tool_launcher::ToolRefusal)
    }

    fn run(
        &self,
        command: crate::edit::tool_launcher::ToolCommand,
    ) -> crate::edit::tool_launcher::ToolOutcome {
        use crate::edit::tool_launcher::{run_prepared, ToolEnvironment, ToolOutcome, ToolRefusal};
        let prepared = match command.environment {
            ToolEnvironment::Inherited => self.command(&command.program, &command.args),
            ToolEnvironment::Clean => {
                self.command_with_clean_environment(&command.program, &command.args)
            }
        };
        match prepared {
            Ok(prepared) => run_prepared(prepared, command),
            Err(reason) => ToolOutcome::Refused(ToolRefusal(reason)),
        }
    }
}
