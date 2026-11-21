use crate::bash_background::registry::test_support::{
    capture_display_terminal, display_bodies, set_completion_secrets, terminal_fixture,
};

fn task_display(pin: &str) -> crate::bash_background::frames::TaskDisplay {
    crate::bash_background::frames::TaskDisplay {
        identity: crate::bash_background::frames::Identity {
            runner: subc_protocol::Principal::Reserved {
                module_id: "broca".into(),
            },
            call_key: "display-terminal-race".into(),
            schema_pin: pin.into(),
        },
        capture_key: "display-terminal-race".into(),
        reply_produced: false,
        deferred: false,
        held_ms: None,
        attempted: false,
    }
}

#[test]
fn bash_tail_masks_secrets_and_tickets_before_and_after_removing_escapes() {
    crate::subc::lower_hooks::register();
    use crate::log_redact::{SecretValues, SECRET_PLACEHOLDER};
    use std::ffi::OsStr;
    const SECRET: &str = "unpredictable-long-credential-value";
    const TICKET: &str = "d34db33f89ab01cd7654ef2098ac1357";
    let (_dir, ctx, task) = terminal_fixture();
    super::default_kill_tests::assert_worker_wait_limit(&ctx);
    let (mut metadata, buffer) = {
        let state = task.lock_state().unwrap();
        (state.metadata.clone(), state.buffer.clone())
    };
    set_completion_secrets(
        &task,
        SecretValues::from_environment([(OsStr::new("CUSTOM_TOKEN"), OsStr::new(SECRET))]),
    );
    metadata.completion_ticket_digest = Some(blake3::hash(TICKET.as_bytes()).to_hex().to_string());
    let pin = crate::subc::tool_views::SHIPPED
        .keys()
        .find(|pin| crate::subc::tool_views::has_bash_tail(pin))
        .unwrap();
    metadata.display = Some(task_display(pin));
    std::fs::write(
        &task.paths.stdout,
        format!("plain {SECRET}\ncolored unpredictable-\x1b[31mlong-credential-value\x1b[0m\n"),
    )
    .unwrap();
    std::fs::write(&task.paths.stderr, format!("ticket {TICKET}\n")).unwrap();
    let bodies = display_bodies(ctx.bash_background(), &task, &metadata, &buffer);
    assert_eq!(
        bodies.tail,
        Some(serde_json::json!([
            {"text":format!("plain {SECRET_PLACEHOLDER}")},
            {"text":format!("colored {SECRET_PLACEHOLDER}")},
            {"text":format!("ticket {SECRET_PLACEHOLDER}"),"tone":"muted"},
        ]))
    );

    let old = pin.strip_suffix(":3").unwrap().to_string() + ":2";
    metadata.display = Some(task_display(&old));
    let legacy = display_bodies(ctx.bash_background(), &task, &metadata, &buffer);
    assert_eq!(legacy.detail, bodies.detail);
    assert!(legacy.tail.is_none());
}

#[test]
fn display_terminal_claim_uses_current_reply_state_not_a_stale_snapshot() {
    let (_dir, ctx, task) = terminal_fixture();
    super::default_kill_tests::assert_worker_wait_limit(&ctx);
    let pin = crate::subc::tool_views::SHIPPED
        .keys()
        .find(|pin| pin.starts_with("tp1:bash:"))
        .unwrap();
    let display = task_display(pin);
    let (stale, buffer) = {
        let mut state = task.lock_state().unwrap();
        state.metadata.display = Some(display);
        let stale = state.metadata.clone();
        let current = state.metadata.display.as_mut().unwrap();
        current.reply_produced = true;
        current.deferred = true;
        (stale, state.buffer.clone())
    };
    capture_display_terminal(ctx.bash_background(), &task, &stale, &buffer);
    assert!(
        task.lock_state()
            .unwrap()
            .metadata
            .display
            .as_ref()
            .unwrap()
            .attempted
    );
    let persisted = crate::bash_background::persistence::read_task(&task.paths.json).unwrap();
    assert!(persisted.display.as_ref().unwrap().attempted);
    capture_display_terminal(ctx.bash_background(), &task, &stale, &buffer);
    assert!(
        task.lock_state()
            .unwrap()
            .metadata
            .display
            .as_ref()
            .unwrap()
            .attempted
    );
}
