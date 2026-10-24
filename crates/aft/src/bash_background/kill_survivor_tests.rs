//! What a kill reports when members of the task's process group are still
//! running after the group SIGKILL.
//!
//! AFT cannot signal a process owned by another user (`killpg` gets EPERM for
//! it), so a root process started through `sudo` survives the kill. The
//! report must name such a survivor instead of describing a clean kill, list
//! nothing when the group is gone, and give a reason when it cannot tell.

use super::*;
use crate::bash_background::process::test_hooks;

const SESSION: &str = "session";

/// A process that ignores SIGTERM, so it outlives the kill whenever the group
/// SIGKILL does not reach it.
const TERM_IGNORING: &str = "trap '' TERM; touch ready; exec sleep 30";

fn registry() -> BgTaskRegistry {
    let registry = BgTaskRegistry::default();
    // Keep the watchdog away: each test, not a background pass, runs the kill.
    registry.inner.shutdown.store(true, Ordering::SeqCst);
    registry
}

fn spawn_for_kill(registry: &BgTaskRegistry, dir: &Path, command: &str) -> String {
    spawn_for_kill_with_env(registry, dir, command, HashMap::new())
}

fn spawn_for_kill_with_env(
    registry: &BgTaskRegistry,
    dir: &Path,
    command: &str,
    env: HashMap<String, String>,
) -> String {
    registry
        .spawn(
            SpawnPlan::Unsandboxed,
            command,
            SESSION.to_string(),
            dir.to_path_buf(),
            env,
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.to_path_buf(),
            10,
            true,
            false,
            Some(dir.to_path_buf()),
        )
        .unwrap()
}

fn wait_for_file(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn task_pgid(registry: &BgTaskRegistry, task_id: &str) -> i32 {
    registry
        .task_for_test(task_id)
        .unwrap()
        .lock_state()
        .unwrap()
        .metadata
        .pgid
        .unwrap()
}

fn completion_for(registry: &BgTaskRegistry, task_id: &str) -> BgCompletion {
    let completions = registry
        .pending_completions_for_session(SESSION)
        .into_iter()
        .filter(|completion| completion.task_id == task_id)
        .collect::<Vec<_>>();
    assert_eq!(completions.len(), 1, "{completions:?}");
    completions.into_iter().next().unwrap()
}

/// SIGKILLs the group when dropped, so a red assertion leaves no survivor.
struct ReapGroup(i32);

impl Drop for ReapGroup {
    fn drop(&mut self) {
        unsafe {
            libc::killpg(self.0, libc::SIGKILL);
        }
    }
}

fn current_user() -> String {
    let uid = unsafe { libc::getuid() };
    std::process::Command::new("id")
        .arg("-un")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| uid.to_string())
}

/// The kill's group SIGKILL reaches no member here, which is what the kernel
/// does for members owned by another user (EPERM). The TERM-ignoring `sleep`
/// is still running afterwards, and both the task and its completion notice
/// must say so with its pid, owner and command.
#[test]
fn timeout_kill_names_a_survivor_the_group_kill_could_not_reach() {
    let registry = registry();
    let dir = tempfile::tempdir().unwrap();
    let task_id = spawn_for_kill(&registry, dir.path(), TERM_IGNORING);
    wait_for_file(&dir.path().join("ready"), "the command did not start");
    let pgid = task_pgid(&registry, &task_id);
    let _reap = ReapGroup(pgid);
    test_hooks::deny_sigkill(pgid);

    registry.kill_for_timeout(&task_id, SESSION).unwrap();
    let snapshot = registry.status(&task_id, SESSION, None, None, 0).unwrap();

    assert_eq!(snapshot.info.status, BgTaskStatus::TimedOut);
    let members = snapshot
        .live_descendants
        .clone()
        .expect("a completed check of the group is a list, not null");
    assert_eq!(members.len(), 1, "{members:?}");
    let survivor = &members[0];
    assert_eq!(survivor.comm, "sleep");
    assert_eq!(survivor.user.as_deref(), Some(current_user().as_str()));
    assert_eq!(survivor.command.as_deref(), Some("sleep …"));
    assert_eq!(snapshot.live_descendants_unavailable, None);
    let expected = format!(
        "1 process survived the kill and is still running: pid {}, user {}, `sleep …`. AFT cannot signal processes owned by another user.",
        survivor.pid,
        current_user()
    );
    assert_eq!(
        snapshot.live_descendants_summary.as_deref(),
        Some(expected.as_str())
    );
    let completion = completion_for(&registry, &task_id);
    assert_eq!(
        completion.live_descendants_summary.as_deref(),
        Some(expected.as_str()),
        "the completion notice must name the survivor too"
    );
}

/// `bash_kill` on a running task takes the same path as a timeout.
#[test]
fn bash_kill_names_a_survivor_the_group_kill_could_not_reach() {
    let registry = registry();
    let dir = tempfile::tempdir().unwrap();
    let task_id = spawn_for_kill(&registry, dir.path(), TERM_IGNORING);
    wait_for_file(&dir.path().join("ready"), "the command did not start");
    let pgid = task_pgid(&registry, &task_id);
    let _reap = ReapGroup(pgid);
    test_hooks::deny_sigkill(pgid);

    let snapshot = registry.kill(&task_id, SESSION).unwrap();

    assert_eq!(snapshot.info.status, BgTaskStatus::Killed);
    let summary = snapshot.live_descendants_summary.unwrap_or_default();
    assert!(
        summary.starts_with("1 process survived the kill and is still running: pid "),
        "{summary}"
    );
}

/// A command that finished on its own but left a TERM-ignoring process in
/// its group: `bash_kill` signals that survivor, and if it is still running
/// afterwards the task says it survived the kill.
#[test]
fn bash_kill_of_a_finished_task_names_what_survived_it() {
    let registry = registry();
    let dir = tempfile::tempdir().unwrap();
    let task_id = spawn_for_kill(
        &registry,
        dir.path(),
        "(trap '' TERM; touch ready; exec sleep 30) & while [ ! -f ready ]; do sleep 0.01; done",
    );
    wait_for_file(&dir.path().join("ready"), "the command did not start");
    let pgid = task_pgid(&registry, &task_id);
    let _reap = ReapGroup(pgid);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = registry.status(&task_id, SESSION, None, None, 0).unwrap();
        if snapshot.info.status.is_terminal()
            && snapshot
                .live_descendants
                .as_ref()
                .is_some_and(|members| !members.is_empty())
        {
            break;
        }
        assert!(Instant::now() < deadline, "the task did not finish");
        std::thread::sleep(Duration::from_millis(20));
    }
    test_hooks::deny_sigkill(pgid);

    let snapshot = registry.kill(&task_id, SESSION).unwrap();

    assert_eq!(snapshot.info.status, BgTaskStatus::Completed);
    let summary = snapshot.live_descendants_summary.unwrap_or_default();
    assert!(
        summary.starts_with("1 process survived the kill and is still running: pid "),
        "{summary}"
    );
}

/// The control case: the group SIGKILL reaches the TERM-ignoring process, the
/// group is empty afterwards, and the check reports an empty list, not null.
#[test]
fn timeout_kill_of_an_emptied_group_reports_an_empty_list() {
    let registry = registry();
    let dir = tempfile::tempdir().unwrap();
    let task_id = spawn_for_kill(&registry, dir.path(), TERM_IGNORING);
    wait_for_file(&dir.path().join("ready"), "the command did not start");
    let _reap = ReapGroup(task_pgid(&registry, &task_id));

    registry.kill_for_timeout(&task_id, SESSION).unwrap();
    let snapshot = registry.status(&task_id, SESSION, None, None, 0).unwrap();

    assert_eq!(snapshot.info.status, BgTaskStatus::TimedOut);
    assert_eq!(snapshot.live_descendants, Some(Vec::new()));
    assert_eq!(snapshot.live_descendants_unavailable, None);
    assert_eq!(snapshot.live_descendants_summary, None);
    let completion = completion_for(&registry, &task_id);
    assert_eq!(completion.live_descendants, Some(Vec::new()));
    assert_eq!(completion.live_descendants_unavailable, None);
}

/// When the group cannot be listed, the task says it could not check and
/// why, instead of an empty list that would read as a clean kill.
#[test]
fn timeout_kill_whose_group_cannot_be_listed_reports_null_with_a_reason() {
    let registry = registry();
    let dir = tempfile::tempdir().unwrap();
    let task_id = spawn_for_kill(&registry, dir.path(), TERM_IGNORING);
    wait_for_file(&dir.path().join("ready"), "the command did not start");
    let pgid = task_pgid(&registry, &task_id);
    let _reap = ReapGroup(pgid);
    test_hooks::fail_listing(pgid);

    registry.kill_for_timeout(&task_id, SESSION).unwrap();
    let snapshot = registry.status(&task_id, SESSION, None, None, 0).unwrap();

    assert_eq!(snapshot.info.status, BgTaskStatus::TimedOut);
    assert_eq!(snapshot.live_descendants, None);
    assert_eq!(
        snapshot.live_descendants_unavailable,
        Some(LiveDescendantsUnavailable::ListingFailed)
    );
    let completion = completion_for(&registry, &task_id);
    assert_eq!(completion.live_descendants, None);
    assert_eq!(
        completion.live_descendants_unavailable,
        Some(LiveDescendantsUnavailable::ListingFailed)
    );
    let json = serde_json::to_value(&snapshot).unwrap();
    assert_eq!(json["live_descendants_unavailable"], "listing_failed");
}

/// A pseudoterminal task's group is checked after AFT kills it, which it was
/// not before: a TERM-ignoring survivor is named, not left unreported.
#[test]
fn pty_kill_names_a_survivor_the_group_kill_could_not_reach() {
    let registry = registry();
    let dir = tempfile::tempdir().unwrap();
    let task_id = registry
        .spawn_pty(
            SpawnPlan::Unsandboxed,
            "/bin/sh -c \"(trap '' TERM HUP; touch ready; exec sleep 30) & wait\"",
            SESSION.to_string(),
            dir.path().to_path_buf(),
            HashMap::new(),
            crate::bash_background::HardKill::After(Duration::from_secs(30)),
            dir.path().to_path_buf(),
            10,
            true,
            false,
            Some(dir.path().to_path_buf()),
            24,
            80,
        )
        .unwrap();
    wait_for_file(&dir.path().join("ready"), "the command did not start");
    let pgid = task_pgid(&registry, &task_id);
    let _reap = ReapGroup(pgid);
    test_hooks::deny_sigkill(pgid);

    registry.kill(&task_id, SESSION).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let snapshot = loop {
        let snapshot = registry.status(&task_id, SESSION, None, None, 0).unwrap();
        if snapshot.info.status.is_terminal() && snapshot.live_descendants.is_some() {
            break snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "the killed PTY task was never checked: {:?} {:?}",
            snapshot.info.status,
            snapshot.live_descendants_unavailable
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let summary = snapshot.live_descendants_summary.unwrap_or_default();
    assert!(
        summary.starts_with("1 process survived the kill and is still running: pid "),
        "{summary}"
    );
}

/// A survivor's arguments can hold a credential: the expanded value of a
/// secret-named variable, or one read from a file that no pattern can
/// recognise. Neither may reach the task record, the status reply or the
/// completion notice; the survivor is still reported, by its executable name.
#[test]
fn a_survivors_arguments_never_reach_the_record_status_or_notice() {
    const ENV_SECRET: &str = "env-secret-7f3a9c41d2";
    const LITERAL_SECRET: &str = "literal-secret-5b8e2d60aa";
    let registry = registry();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pw"), LITERAL_SECRET).unwrap();
    let task_id = spawn_for_kill_with_env(
        &registry,
        dir.path(),
        "exec sh -c 'trap \"\" TERM; touch ready; while :; do sleep 1; done' worker --token \"$DEPLOY_TOKEN\" --password=\"$(cat pw)\"",
        HashMap::from([("DEPLOY_TOKEN".to_string(), ENV_SECRET.to_string())]),
    );
    wait_for_file(&dir.path().join("ready"), "the command did not start");
    let pgid = task_pgid(&registry, &task_id);
    let _reap = ReapGroup(pgid);
    test_hooks::deny_sigkill(pgid);

    registry.kill_for_timeout(&task_id, SESSION).unwrap();
    let snapshot = registry.status(&task_id, SESSION, None, None, 0).unwrap();
    let completion = completion_for(&registry, &task_id);
    let record =
        std::fs::read_to_string(&registry.task_for_test(&task_id).unwrap().paths.json).unwrap();

    let members = snapshot
        .live_descendants
        .clone()
        .expect("the group was checked");
    assert!(
        members.iter().any(|member| member.comm == "sh"),
        "the survivor must still be reported: {members:?}"
    );
    let surfaces = [
        ("task record", record),
        ("status", serde_json::to_string(&snapshot).unwrap()),
        ("completion", serde_json::to_string(&completion).unwrap()),
        (
            "summary",
            snapshot
                .live_descendants_summary
                .clone()
                .unwrap_or_default(),
        ),
    ];
    for (surface, text) in &surfaces {
        for secret in [ENV_SECRET, LITERAL_SECRET] {
            assert!(
                !text.contains(secret),
                "{surface} carries a survivor's argument {secret}: {text}"
            );
        }
    }
    let summary = snapshot.live_descendants_summary.unwrap_or_default();
    assert!(summary.contains(", `sh …`"), "{summary}");
}

#[test]
fn kill_survivors_line_names_the_first_three_and_counts_the_rest() {
    let member = |pid: u32, user: Option<&str>, command: Option<&str>| LiveDescendant {
        pid,
        comm: "sleep".into(),
        argv0: "sleep".into(),
        user: user.map(str::to_string),
        command: command.map(str::to_string),
    };
    assert_eq!(
        kill_survivors_line(&[member(3084, Some("root"), Some("sleep 30"))], 0),
        "1 process survived the kill and is still running: pid 3084, user root, `sleep 30`. AFT cannot signal processes owned by another user."
    );
    let members = [
        member(1, Some("root"), Some("a")),
        member(2, None, None),
        member(3, Some("root"), Some("c")),
        member(4, Some("root"), Some("d")),
    ];
    assert_eq!(
        kill_survivors_line(&members, 2),
        "6 processes survived the kill and are still running: pid 1, user root, `a`; pid 2, user unknown, `sleep`; pid 3, user root, `c`; +3 more. AFT cannot signal processes owned by another user."
    );
}

/// A real root-owned survivor started through `sudo`. The test needs a way to
/// run a command as root without a password, named by `AFT_TEST_ROOT_PREFIX`
/// (for example `sudo -n`), and is skipped without it: the build server runs
/// jobs with "no new privileges", so sudo cannot work there.
#[test]
fn timeout_kill_reports_a_root_owned_survivor_through_sudo() {
    let Some(prefix) = std::env::var("AFT_TEST_ROOT_PREFIX")
        .ok()
        .filter(|prefix| !prefix.trim().is_empty())
    else {
        eprintln!("skipped: AFT_TEST_ROOT_PREFIX is not set");
        return;
    };
    let registry = registry();
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("marker");
    let ready = dir.path().join("ready");
    let command = format!(
        "{prefix} bash -c \"trap 'echo TRAP_RAN >> {marker}' EXIT; touch {ready}; sleep 30\"",
        marker = marker.display(),
        ready = ready.display(),
    );
    let task_id = spawn_for_kill(&registry, dir.path(), &command);
    wait_for_file(&ready, "the root command did not start");
    let pgid = task_pgid(&registry, &task_id);

    registry.kill_for_timeout(&task_id, SESSION).unwrap();
    let snapshot = registry.status(&task_id, SESSION, None, None, 0).unwrap();
    let completion = completion_for(&registry, &task_id);
    let survivors_after = live_process_group_members(pgid);

    // Try to kill the root processes before asserting, so a red run does not
    // leave them behind.
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{prefix} kill -KILL -- -{pgid}"))
        .status();

    eprintln!(
        "status={:?} exit={:?} live_descendants={:?} summary={:?} still_running={:?}",
        snapshot.info.status,
        snapshot.exit_code,
        snapshot.live_descendants,
        snapshot.live_descendants_summary,
        survivors_after,
    );
    assert!(
        survivors_after
            .as_ref()
            .is_ok_and(|(members, _)| !members.is_empty()),
        "the repro needs a member that survives the kill"
    );
    assert_eq!(snapshot.info.status, BgTaskStatus::TimedOut);
    let members = snapshot
        .live_descendants
        .as_ref()
        .expect("a completed check of the group must not report null");
    let sleep = members
        .iter()
        .find(|member| member.comm == "sleep")
        .expect("the surviving root sleep was not reported");
    assert_eq!(sleep.user.as_deref(), Some("root"));
    assert_eq!(sleep.command.as_deref(), Some("sleep …"));
    let summary = snapshot.live_descendants_summary.unwrap_or_default();
    assert!(
        summary.contains(" survived the kill and ")
            && summary.contains(&format!("pid {}, user root, `sleep …`", sleep.pid))
            && summary.ends_with("AFT cannot signal processes owned by another user."),
        "{summary}"
    );
    assert_eq!(
        completion.live_descendants_summary.as_deref(),
        Some(summary.as_str())
    );
}
