//! Install the optional daemon services declared by lower layers before serving routes.

/// Install the tool callbacks shared by standalone and attached startup. The
/// edit reporter only captures details while a daemon display scope is active.
#[doc(hidden)]
pub fn register_tool_callbacks() {
    crate::commands::edit_match::register_edit_no_match_reporter(
        super::rich_ui::display::edit_no_match,
    );
}

pub(crate) fn register() {
    register_tool_callbacks();
    crate::reader_floor::register_cache_hooks();
    crate::commands::configure::register_runtime_hooks();
    crate::bash_background::frames::register(crate::bash_background::frames::Hooks {
        enabled: super::rich_ui::display::enabled,
        current_task: super::rich_ui::display::current_task,
        bash_reply: super::rich_ui::display::bash_reply,
        bash_reply_persisted: super::rich_ui::display::bash_reply_persisted,
        bash_terminal: super::rich_ui::display::bash_terminal,
        bash_tail: super::rich_ui::display::bash_tail,
        bash_detail_byte_limit: super::rich_ui::display::detail_byte_limit,
        recovered_bash_frame: super::recovered_bash_frame,
    });
    crate::edit::register_file_reporter(super::rich_ui::display::file);
    crate::db::call_ledger::register_held_result_projector(super::project_held_ledger_result);
    crate::commands::bash_watch::restart::register(super::call_yield::settle_watch);
    crate::late_result_bus::register(crate::late_result_bus::Hooks {
        take_due_read: super::rich_ui::display::take_due_read,
        mark_read_published: super::rich_ui::display::mark_read_published,
        publish_awaited: |publish| Box::pin(super::rich_ui::display::publish_awaited_notice(publish)),
        announce: |id, bytes, publish| {
            Box::pin(async move {
                use super::rich_ui::notices;
                if notices::is_notice_id(&id)
                    && (notices::WRITER_CAPABILITY.is_none()
                        || crate::late_result_bus::notice_display_reader()
                            .ok()
                            .flatten()
                            .is_none())
                {
                    return Err("notice_display_unavailable");
                }
                super::rich_ui::display::announce(id, bytes, publish).await
            })
        },
        bus_snapshot: super::tool_views::bus_snapshot,
    });
    crate::commands::register_external_search();
}

#[cfg(test)]
mod tests {
    use aft_engine::test_bridge::display_data::{configure, late_result_bus};

    #[test]
    fn display_hook_without_writer_never_polls_publish() {
        const CHILD: &str = "AFT_TEST_DISPLAY_HOOK_WRITERLESS_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "subc::lower_hooks::tests::display_hook_without_writer_never_polls_publish",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            return;
        }

        super::register();
        let hooks = late_result_bus::hooks().unwrap();
        let result = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(late_result_bus::display::announce(
                Some(hooks),
                "record:writerless".to_owned(),
                vec![1],
                Box::pin(std::future::poll_fn(|_| {
                    panic!("publish was polled without a writer")
                })),
            ));
        assert_eq!(result, Err("display_store_not_started"));
    }

    #[test]
    fn edge_registers_every_lower_layer_hook() {
        super::register();
        assert!(configure::registered());
        assert!(configure::database_runtime().is_some());
        assert!(configure::command_hooks().is_some());
        assert!(crate::cache_location::reader_floor_hooks_registered());
        assert!(aft_engine::test_bridge::bash_remote::bash_background::frames::registered());
        assert!(aft_engine::test_bridge::search_misc::edit::file_reporter_registered());
        assert!(crate::db::call_ledger::held_result_projector_registered());
        assert!(crate::tools_test_support::watch_restart_registered());
        assert!(late_result_bus::hooks().is_some());
    }

    #[test]
    fn held_ledger_projection_keeps_the_direct_edge_bytes() {
        super::register();
        let legacy = crate::db::call_ledger::RecordedFrame {
            ty: subc_protocol::FrameType::Response,
            body: br#"{"content":[{"type":"text","text":"terminal output"}],"isError":false}"#
                .to_vec(),
        };
        for structured in [false, true] {
            let old = crate::subc::project_held_ledger_result(&legacy, None, structured).unwrap();
            let new =
                crate::db::call_ledger::project_held_result(&legacy, None, structured).unwrap();
            assert_eq!(old.ty, new.ty);
            assert_eq!(old.body, new.body);
        }
    }

    #[test]
    fn every_process_entry_registers_floor_checks_before_storage() {
        let source = std::fs::read_to_string(
            std::path::Path::new(crate::test_support::manifest_dir()).join("../aft/src/main.rs"),
        )
        .expect("read facade main source");
        let main = source.split_once("fn main() {").unwrap().1;
        let registration = main
            .find("aft::reader_floor::register_cache_hooks();")
            .unwrap();
        for entry in [
            "if aft::gh_shim::is_shim_invocation_from_env()",
            "parse_subc_arg(std::env::args_os()",
            "cli::index::run(args)",
            "Config::default()",
        ] {
            assert!(
                registration < main.find(entry).unwrap(),
                "reader-floor registration must precede {entry}"
            );
        }
        let shim = include_str!("../../../aft-engine/src/gh_shim.rs")
            .split_once("pub fn run_from_env() -> i32 {")
            .unwrap()
            .1;
        assert!(shim
            .trim_start()
            .starts_with("crate::reader_floor::register_cache_hooks();"));
    }
}
#[cfg(test)]
#[path = "call_ledger_recovery_tests.rs"]
mod call_ledger_recovery_tests;

#[cfg(test)]
#[path = "context_database_tests.rs"]
mod context_database_tests;
