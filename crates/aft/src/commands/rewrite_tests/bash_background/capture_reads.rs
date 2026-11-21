mod tests {
    use crate::bash_background::registry::BgTaskRegistry;
    use crate::log_redact::SECRET_PLACEHOLDER;
    use aft_engine::test_bridge::bash_remote::registry_support::{
        display_bodies, filler, terminal_task, with_secret,
        DISPLAY_LINE_FINISH_BYTES as LINE_FINISH_BYTES,
    };
    use aft_engine::test_bridge::bash_remote::work_counts;
    use aft_engine::test_bridge::bash_remote::BgTask;

    fn display_detail(
        registry: &BgTaskRegistry,
        task: &std::sync::Arc<BgTask>,
    ) -> serde_json::Value {
        let (metadata, buffer) = {
            let state = task.lock_state().unwrap();
            (state.metadata.clone(), state.buffer.clone())
        };
        display_bodies(registry, task, &metadata, &buffer).detail
    }

    fn install_tail_display(metadata: &mut crate::bash_background::persistence::PersistedTask) {
        use crate::bash_background::frames::{Identity, TaskDisplay};

        crate::subc::lower_hooks::register();
        let pin = crate::subc::tool_views::SHIPPED
            .keys()
            .find(|pin| crate::subc::tool_views::has_bash_tail(pin))
            .unwrap();
        metadata.display = Some(TaskDisplay {
            identity: Identity {
                runner: subc_protocol::Principal::Reserved {
                    module_id: "broca".into(),
                },
                call_key: "capture-display".into(),
                schema_pin: pin.clone(),
            },
            capture_key: "capture-display".into(),
            reply_produced: false,
            deferred: false,
            held_ms: None,
            attempted: false,
        });
    }

    #[test]
    fn display_detail_reads_only_the_lines_its_limit_can_show() {
        let fixture = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let stdout = filler(64 * 1024 * 1024);
        let task = terminal_task(&registry, &fixture, &stdout, false);
        let limit = crate::subc::rich_ui::display::detail_byte_limit();
        let (mut metadata, buffer) = {
            let state = task.lock_state().unwrap();
            (state.metadata.clone(), state.buffer.clone())
        };
        install_tail_display(&mut metadata);

        work_counts::reset();
        let bodies = display_bodies(&registry, &task, &metadata, &buffer);
        let read = work_counts::get().artifact_bytes as u64;

        assert!(
            (limit..=limit + LINE_FINISH_BYTES).contains(&read),
            "the detail read {read} capture bytes to show at most {limit}"
        );
        assert!(bodies.tail.is_none(), "a detail prefix is not a tail");
        let detail = bodies.detail;
        let shown = detail["stdout"].as_str().unwrap();
        assert!(shown.len() as u64 >= limit);
        assert!(stdout.starts_with(shown) && shown.ends_with('\n'));
        assert_eq!(detail["truncated"], true);
        assert_eq!(detail["omittedBytes"], stdout.len() - shown.len());
    }

    #[test]
    fn display_detail_includes_tail_when_its_bounded_read_reaches_the_end() {
        let fixture = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let limit = crate::subc::rich_ui::display::detail_byte_limit() as usize;
        // The complete capture fits in the read's line-finishing allowance but
        // not in the display detail's byte budget.
        let stdout = format!("{}\nfirst\nsecond\nthird\nlast\n", filler(limit));
        let task = terminal_task(&registry, &fixture, &stdout, false);
        let (mut metadata, buffer) = {
            let state = task.lock_state().unwrap();
            (state.metadata.clone(), state.buffer.clone())
        };
        install_tail_display(&mut metadata);

        work_counts::reset();
        let bodies = display_bodies(&registry, &task, &metadata, &buffer);

        assert_eq!(work_counts::get().artifact_bytes as usize, stdout.len());
        assert_eq!(
            bodies.tail,
            Some(serde_json::json!([
                {"text": "second"},
                {"text": "third"},
                {"text": "last"}
            ]))
        );
    }

    #[test]
    fn display_detail_masks_a_secret_on_the_line_crossing_its_limit() {
        let fixture = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let limit = crate::subc::rich_ui::display::detail_byte_limit() as usize;
        let secret = "s3cr3t-display-value-0123456789";
        let mut stdout = filler(limit - 10);
        stdout.push_str(&format!("token {secret} end\n"));
        stdout.push_str(&filler(2 * LINE_FINISH_BYTES as usize));
        let task = terminal_task(&registry, &fixture, &stdout, false);
        with_secret(&task, secret);

        let detail = display_detail(&registry, &task);
        let shown = detail["stdout"].as_str().unwrap();
        assert!(!shown.contains(&secret[..8]), "secret prefix shown");
        assert!(shown.contains(SECRET_PLACEHOLDER));
        assert_eq!(detail["truncated"], true);
    }

    #[test]
    fn display_detail_reads_whole_when_a_json_secret_is_open_at_its_cut() {
        let fixture = tempfile::tempdir().unwrap();
        let registry = BgTaskRegistry::default();
        let limit = crate::subc::rich_ui::display::detail_byte_limit() as usize;
        let mut stdout = filler(limit - 10);
        // The value holds a raw newline just past the limit, so a cut there
        // would leave its first half unmasked.
        stdout.push_str("{\"password\": \"first-half-of-value\nsecond-half\"}\n");
        stdout.push_str(&filler(2 * LINE_FINISH_BYTES as usize));
        let task = terminal_task(&registry, &fixture, &stdout, false);

        let detail = display_detail(&registry, &task);
        let shown = detail["stdout"].as_str().unwrap();
        assert!(
            !shown.contains("first-half-of-value"),
            "open JSON secret shown"
        );
    }
}
