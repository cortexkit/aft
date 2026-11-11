//! Install the optional daemon services declared by lower layers before serving routes.

pub(crate) fn register() {
    crate::commands::configure::register_runtime_hooks();
    crate::bash_background::frames::register(crate::bash_background::frames::Hooks {
        enabled: super::rich_ui::display::enabled,
        current_task: super::rich_ui::display::current_task,
        bash_reply: super::rich_ui::display::bash_reply,
        bash_terminal: super::rich_ui::display::bash_terminal,
        recovered_bash_frame: super::recovered_bash_frame,
    });
    crate::edit::register_file_reporter(super::rich_ui::display::file);
    crate::db::call_ledger::register_held_result_projector(super::project_held_ledger_result);
    crate::commands::bash_watch::restart::register(super::call_yield::settle_watch);
}

#[cfg(test)]
mod tests {
    #[test]
    fn edge_registers_every_lower_layer_hook() {
        super::register();
        assert!(crate::runtime_drain::configure::registered());
        assert!(crate::runtime_drain::configure::database_runtime().is_some());
        assert!(crate::bash_background::frames::registered());
        assert!(crate::edit::file_reporter_registered());
        assert!(crate::db::call_ledger::held_result_projector_registered());
        assert!(crate::commands::bash_watch::restart::registered());
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
}
