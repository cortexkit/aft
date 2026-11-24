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

impl<'ctx> ToolLaunchPolicy<'ctx> {
    /// Direct tools carry only their formatter-and-checker launch policy across
    /// dispatch. General sandbox scopes must not cover dispatch: it can start
    /// shared LSP servers.
    #[doc(hidden)]
    pub fn with_current<R>(
        policy: Option<std::rc::Rc<RequestToolLaunch>>,
        run: impl FnOnce() -> R,
    ) -> R {
        struct Restore(Option<std::rc::Rc<RequestToolLaunch>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                CURRENT_TOOL_POLICY.with(|slot| {
                    slot.replace(self.0.take());
                });
            }
        }
        let previous = CURRENT_TOOL_POLICY.with(|slot| slot.replace(policy));
        let _restore = Restore(previous);
        run()
    }
}
