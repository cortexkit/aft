//! `aft cache prune-legacy [--yes]`: deletes legacy (pre-view) index sets
//! whose import into per-checkout views completed at least the retention
//! window ago. The work and its safeguards live in
//! `aft::migration::prune_legacy`; this file parses arguments and reports.

use std::ffi::OsString;
use std::io::{self, Write};
use std::time::SystemTime;

use aft::migration::prune_legacy::{
    prune_legacy, retention_days, PruneOptions, SystemCensus, OPERATOR_CONTRACT,
};

/// Exit code for an argument error.
const USAGE_EXIT: i32 = 2;

fn usage() -> String {
    format!(
        "Usage: aft cache prune-legacy [--yes]

Lists every legacy (pre-view) index set under the AFT storage root with its
bytes, and with --yes deletes the sets whose import into per-checkout views
or into the v3 stores completed at least {days} days ago. A set that a running
import or a v3 base build may still read is kept. Older v1 views and blob
stores and v2 family stores are listed as retained and never deleted. Without
--yes it is a dry run.

{OPERATOR_CONTRACT}

A run refuses when another aft or ck-aft process is running, when a process
that looks like AFT cannot be classified, or when the process list cannot be
read. With --yes, each eligible set is first renamed as a whole into
<storage>/.prune-legacy-<timestamp>/ on the same filesystem, the check runs
again, and only then is that directory deleted. A set that cannot be renamed
atomically is skipped, never deleted in place.

The storage root is AFT_STORAGE_DIR when set, otherwise the default.

Exit codes: 0 done or dry run, 2 usage error, 3 refused by the process
check, 4 a process appeared after sets were moved aside (they are kept and a
later run removes them), 5 a set was skipped.
",
        days = retention_days()
    )
}

/// Runs `aft cache <args>` and returns the process exit code.
pub fn run(args: Vec<OsString>) -> i32 {
    let mut args = args.into_iter();
    match args.next().as_deref().and_then(|arg| arg.to_str()) {
        Some("prune-legacy") => {}
        Some("--help" | "-h") | None => {
            print!("{}", usage());
            return 0;
        }
        Some(other) => {
            eprintln!("unknown cache command `{other}`\n\n{}", usage());
            return USAGE_EXIT;
        }
    }
    let mut yes = false;
    for arg in args {
        match arg.to_str() {
            Some("--yes" | "-y") => yes = true,
            Some("--help" | "-h") => {
                print!("{}", usage());
                return 0;
            }
            _ => {
                eprintln!(
                    "unexpected argument `{}`\n\n{}",
                    arg.to_string_lossy(),
                    usage()
                );
                return USAGE_EXIT;
            }
        }
    }
    let storage = aft::bash_background::storage_dir(None);
    if !storage.is_absolute() {
        eprintln!(
            "AFT_STORAGE_DIR must be absolute when set: {}",
            storage.display()
        );
        return USAGE_EXIT;
    }
    let options = PruneOptions {
        yes,
        now: SystemTime::now(),
    };
    let mut out = io::stdout().lock();
    match prune_legacy(&storage, &options, &SystemCensus, None, &mut out) {
        Ok(exit) => {
            let _ = out.flush();
            exit.code()
        }
        Err(error) => {
            let _ = out.flush();
            eprintln!("aft cache prune-legacy failed: {error}");
            1
        }
    }
}
