pub const USAGE: &str = "\
AFT — Agent Foundational Toolkit

Usage: aft [<subcommand> [<args>...]]
       aft --subc <connection-file>

Normally started by a harness plugin, or by the CortexKit daemon with
--subc <connection-file>.

Info flags:
  --version, -V       Print the version.
  --formats           Print the highest storage format versions this build reads as JSON.
  --help, -h          Print this usage text.

Subcommands:
  setup               Show the feature plan or save your base choices.
  fix-config          Repair retired configuration keys and inactive caches.
  index               Build a one-shot snapshot of configured standing index roots.
  warmup              Warm search, semantic, and callgraph indexes for a repository.
  profile             Profile the running daemon's CPU, memory, or writes.
  backups purge       Preview or remove undo backups early.
  cache prune-legacy  Preview or remove index folders used before AFT 0.64.
  cache clean-old     Remove old caches after moving them to a separate device.
";

/// Only the first argument belongs to this guard; subcommands own their flags.
pub fn handle_first_arg(first_arg: Option<&str>) -> Option<i32> {
    let arg = first_arg?;
    match arg {
        "--help" | "-h" => {
            print!("{USAGE}");
            Some(0)
        }
        "--version" | "-V" | "--formats" | "--subc" => None,
        "setup" | "fix-config" | "index" | "grammar-smoke" | "backups" | "cache" | "profile"
        | "warmup" => None,
        _ if arg.starts_with("--subc=") => None,
        _ if !arg.starts_with('-') => {
            eprint!("aft: unknown command '{arg}'\n\n{USAGE}");
            Some(2)
        }
        _ => {
            eprint!("aft: unknown option '{arg}'\n\n{USAGE}");
            Some(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::handle_first_arg;

    #[test]
    fn recognized_commands_pass_through_to_their_dispatchers() {
        for command in [
            "setup",
            "fix-config",
            "index",
            "grammar-smoke",
            "backups",
            "cache",
            "profile",
            "warmup",
        ] {
            assert_eq!(handle_first_arg(Some(command)), None, "{command}");
        }
        assert_eq!(handle_first_arg(None), None);
    }
}
