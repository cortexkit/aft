//! `aft fix-config`: the configuration half of `aft doctor --fix`.
//!
//! Repairs the files ordinary loading consumes for this invocation (the user
//! file and, when the invocation directory has one, the project file) by
//! rewriting retired keys to their canonical replacements. Each file is
//! repaired and written on its own; one failure never undoes another file's
//! repair. The per-file outcomes go to stdout as JSON, and the exit status is
//! non-zero when any file failed.

use std::ffi::OsString;
use std::io::{self, Write};

use aft::config_fix::{fix_files, fix_targets};

pub fn run(args: Vec<OsString>) -> i32 {
    if let Some(arg) = args.first() {
        eprintln!("usage: aft fix-config (unexpected argument {})", arg.to_string_lossy());
        return 2;
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            eprintln!("could not determine current directory: {error}");
            return 1;
        }
    };
    let user = aft::subc_config::cortexkit_user_config_path();
    let outcomes = fix_files(&fix_targets(user.as_deref(), &cwd));
    let failed = outcomes.iter().any(|outcome| outcome.status == "failed");
    let report = serde_json::json!({ "files": outcomes });
    let _ = writeln!(io::stdout().lock(), "{report}");
    i32::from(failed)
}
