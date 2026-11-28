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
    pub(crate) fn command(&self, program: &OsStr, args: &[OsString]) -> Result<Command, String> {
        self.command_with_base_environment(program, args, false)
    }

    /// Bash syntax checks must not read daemon-provided shell startup hooks.
    /// Clear inherited state before installing any policy-required environment.
    pub(crate) fn command_with_clean_environment(
        &self,
        program: &OsStr,
        args: &[OsString],
    ) -> Result<Command, String> {
        self.command_with_base_environment(program, args, true)
    }

    fn command_with_base_environment(
        &self,
        program: &OsStr,
        args: &[OsString],
        clean: bool,
    ) -> Result<Command, String> {
        let launch = self.launch();
        // Host grants bind one approved shell command, never an edit's tools.
        if matches!(launch.plan.policy(), SpawnPlan::Host { .. }) {
            return Err(
                "sandbox_unavailable: host escalation cannot run a formatter or checker".into(),
            );
        }
        if let SpawnPlan::Refused { code, message, .. } = launch.plan.policy() {
            return Err(format!("{code}: {message}"));
        }
        let (program, args, _profile_handle) =
            command_argv_for_plan(&launch.plan, program, args, Path::new(""), None)?;
        #[cfg(windows)]
        let mut command = if crate::windows_command::is_batch_file(Path::new(&program)) {
            crate::windows_command::batch_command(Path::new(&program), &args)
                .map_err(|error| error.to_string())?
        } else {
            let mut command = crate::effective_path::new_command(program).build();
            command.args(args);
            command
        };
        #[cfg(not(windows))]
        let mut command = {
            let mut command = crate::effective_path::new_command(program).build();
            command.args(args);
            command
        };
        if clean {
            command.env_clear();
            if let Some(path) = std::env::var_os("PATH") {
                command.env("PATH", path);
            }
        } else if matches!(launch.plan.policy(), SpawnPlan::Unsandboxed) {
            // Apply the shell child's removals without injecting shell request overrides.
            crate::agent_child_env::apply_to_command(&mut command, &HashMap::new());
        }
        apply_sandbox_environment(&launch.plan, &mut command, &launch.environment);
        #[cfg(unix)]
        if !matches!(launch.plan.policy(), SpawnPlan::Unsandboxed) {
            disclaim_command_for_plan(&launch.plan, &mut command, &launch.environment)?;
        }
        Ok(command)
    }

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
