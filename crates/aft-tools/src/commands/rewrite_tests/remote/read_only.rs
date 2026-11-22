#[allow(unused_imports)]
use super::*;

#[test]
fn runon_read_only_note_requires_lock_failure_and_in_checkout_eperm() {
    for (locked, code, error) in [
        (None, 1, "../src/x.rs: Operation not permitted\n"),
        (Some(false), 1, "../src/x.rs: Operation not permitted\n"),
        (Some(true), 0, "../src/x.rs: Operation not permitted\n"),
        (
            Some(true),
            1,
            "'$ROOT/../outside.rs': Operation not permitted\n",
        ),
        (Some(true), 1, "../../outside.rs: Operation not permitted\n"),
        (Some(true), 1, "../src/x.rs: Permission denied\n"),
        (Some(true), 1, "failed without an error path\n"),
    ] {
        let (foreground, completion, disk) =
            read_only_result_case(locked, Outcome::Exit { code }, error);
        for text in [foreground, completion, disk] {
            assert!(
                !text.contains("note: the build server keeps tracked files"),
                "{text}"
            );
        }
    }
}
