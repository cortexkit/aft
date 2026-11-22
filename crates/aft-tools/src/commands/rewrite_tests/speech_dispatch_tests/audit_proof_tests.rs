use super::*;

// These assertions describe the required authority boundary. They intentionally
// fail on implementation d69a90a3d0; they are evidence, not fixes.
#[tokio::test]
async fn bound_background_must_not_follow_a_rotated_bot_handle() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(false).await;
    fixture.plex.state.lock().unwrap().handle = "app-rotated".into();

    let answer = fixture.post(true, "audit-bound-handle").await;
    let state = fixture.plex.state.lock().unwrap();
    assert!(
        state.posts.is_empty(),
        "a background command captured app-original, but posted after rotation to app-rotated: {:?}",
        answer.data
    );
}

#[tokio::test]
async fn bound_codemode_must_require_the_program_to_remain_live() {
    let _lock = crate::test_env::process_env_lock();
    let fixture = Fixture::new(true).await;
    let run = fixture.run.as_ref().unwrap();
    fixture
        .plex
        .state
        .lock()
        .unwrap()
        .ended
        .insert(ScopeKey::from_stamp(run));

    let answer = fixture.post(true, "audit-bound-ended-program").await;
    let state = fixture.plex.state.lock().unwrap();
    assert!(
        state.posts.is_empty(),
        "the provider knows the program ended, but the still-bound head write omitted its guard: {:?}",
        answer.data
    );
}
