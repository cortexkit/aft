#[test]
fn bash_changed_files_off_does_no_probes_or_watcher_registration() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.path().to_path_buf());
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    assert_eq!(ctx.config().bash.changed_files, BashChangedFiles::Off);
    assert!(bash_changed_files::start(&ctx).is_none());
    assert!(ctx.watcher_counters().bash_changes.get().is_none());
}

#[test]
fn bash_changed_files_watcher_slot_keeps_the_capture_journal_after_one_context_drops() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().canonicalize().unwrap();
    let make_context = || {
        let mut config = crate::config::Config::default();
        config.project_root = Some(project.clone());
        config.bash.changed_files = BashChangedFiles::List;
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config)
    };
    let first = make_context();
    let second = make_context();
    let capture = bash_changed_files::start(&first).unwrap();
    let counters = second.index_watcher_counters();
    let second_journal = second.watcher_counters().bash_changes.get().unwrap();
    assert!(Arc::ptr_eq(capture.journal_for_test(), &second_journal));

    drop(first);
    let sink = counters.bash_changes().get().unwrap();
    let matcher = Arc::new(std::sync::RwLock::new(None));
    let event = notify::Event::new(notify::EventKind::Create(CreateKind::File))
        .add_path(project.join("after-first-context.txt"));
    sink.event_for_root(&event, &project, &matcher);
    sink.gap();
    let report = capture.report_for_test();
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.files[0].path, "after-first-context.txt");
    assert!(report.text.contains(OVERFLOW_TEXT));
}

#[test]
fn bash_changed_files_flush_inherits_reply_budget_before_spawn_and_clears_on_handoff() {
    let root = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.path().into());
    config.bash.changed_files = BashChangedFiles::List;
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let deadline = Instant::now() + Duration::from_millis(20);
    let budget = BudgetGuard::new(deadline);
    let receipt = Arc::new(crate::bash_background::SpawnReceipt::new(
        Instant::now() + Duration::from_secs(30),
    ));
    receipt.bind_changed_files_deadline(budget.shared());
    let capture = crate::bash_background::with_spawn_receipt(receipt.clone(), || {
        bash_changed_files::start(&ctx).unwrap()
    });
    assert_eq!(capture.deadline_for_test(), Some(deadline));
    receipt.limit_changed_files_deadline(deadline + Duration::from_secs(1));
    assert_eq!(capture.deadline_for_test(), Some(deadline));
    drop(budget);
    assert!(capture.deadline_for_test().is_none());
    receipt.limit_changed_files_deadline(deadline);
    assert!(
        capture.deadline_for_test().is_none(),
        "handoff cannot revive a caller budget"
    );
    let late = crate::bash_background::with_spawn_receipt(receipt, || {
        bash_changed_files::start(&ctx).unwrap()
    });
    assert!(late.deadline_for_test().is_none());
}

#[test]
fn bash_changed_files_diff_inherits_shell_launch_read_denies() {
    let root = repo();
    let storage = tempfile::tempdir().unwrap();
    let secret = root.path().join("environment-secret.txt");
    std::fs::write(&secret, "LAUNCH_PROFILE_SECRET_MUST_NOT_APPEAR\n").unwrap();
    let mut config = crate::config::Config::default();
    config.project_root = Some(root.path().into());
    config.storage_dir = Some(storage.path().into());
    config.bash.changed_files = BashChangedFiles::Diff;
    let ctx =
        crate::context::AppContext::new(Box::new(crate::parser::TreeSitterProvider::new()), config);
    let mut profile = crate::sandbox_profile::SandboxProfile::build(
        vec![root.path().into()],
        vec![],
        vec![],
        vec![],
        vec![secret.clone()],
        vec![],
        vec![],
        root.path().into(),
    )
    .unwrap();
    profile.deny_prefixes = vec![root.path().join("credential-prefix")];
    let profile = profile.canonicalize_for_launch().unwrap();
    let plan = crate::sandbox_spawn::SpawnPlan::Launcher {
        profile,
        launcher_path: PathBuf::from("unused-test-launcher"),
        task_launcher: None,
    };
    let capture = bash_changed_files::start_for_spawn(&ctx, "read-floor-test", &plan).unwrap();
    std::fs::write(
        root.path().join("credential-prefix.toml"),
        "PREFIX_SECRET_MUST_NOT_APPEAR\n",
    )
    .unwrap();
    for name in ["environment-secret.txt", "credential-prefix.toml"] {
        event(
            capture.journal_for_test(),
            capture.root_for_test(),
            name,
            notify::EventKind::Create(CreateKind::File),
        );
    }
    let report = capture.report_for_test();
    assert_eq!(report.files.len(), 2);
    assert!(
        report
            .files
            .iter()
            .all(|file| file.diff.is_none() && file.reason.as_deref() == Some(NOT_SHOWN_TEXT)),
        "{report:?}"
    );
    assert!(!serde_json::to_string(report)
        .unwrap()
        .contains("SECRET_MUST_NOT_APPEAR"));
}
use crate::bash_changed_files::{self, BudgetGuard};
use crate::config::BashChangedFiles;
use aft_shell::bash_changed_files::test_fixture::{event, repo, NOT_SHOWN_TEXT, OVERFLOW_TEXT};
use aft_shell::test_bridge::SpawnReceiptTestExt as _;
use notify::event::{CreateKind, ModifyKind};
use std::path::PathBuf;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
