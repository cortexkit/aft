
#[test]
fn worker_only_edits_with_tier2_enabled_do_not_refresh_the_callgraph() {
    use crate::inspect::tier2_scheduler::{
        TIER2_REFRESH_COLD_CACHE_DELAY, TIER2_REFRESH_DEBOUNCE, TIER2_REFRESH_MIN_INTERVAL,
    };
    use crate::inspect::Tier2TriggerReason;

    let quiet = Duration::from_millis(100);
    let fixture = PublicationFixture::new(quiet, quiet);
    let ctx = fixture.context();
    ctx.set_worker_only_routes(true);
    assert!(ctx.config().inspect.enabled);
    let configured_at = Instant::now()
        .checked_sub(TIER2_REFRESH_COLD_CACHE_DELAY + Duration::from_secs(1))
        .unwrap();
    ctx.reset_tier2_refresh_scheduler_at(configured_at);
    let now = Instant::now();
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(now, 0),
        Some(Tier2TriggerReason::ConfigureWarm)
    );
    wait_until(|| {
        crate::runtime_drain::drain_inspect_events(&ctx);
        ctx.status_bar_count_values().todos == Some(0)
    });
    let before = fixture.refreshes();
    let path = fixture.root().join("src/lib.rs");
    std::fs::write(
        &path,
        "// TODO: keep the count current\npub fn original() {}\n",
    )
    .unwrap();
    fixture.note_paths(std::slice::from_ref(&path));
    ctx.add_pending_tier2_paths([path]);
    let edited_at = now + TIER2_REFRESH_MIN_INTERVAL;
    assert_eq!(ctx.tick_tier2_refresh_scheduler_at(edited_at, 1), None);
    assert_eq!(
        ctx.tick_tier2_refresh_scheduler_at(edited_at + TIER2_REFRESH_DEBOUNCE, 0),
        Some(Tier2TriggerReason::Debounce)
    );
    wait_until(|| {
        crate::runtime_drain::drain_inspect_events(&ctx);
        ctx.status_bar_count_values().todos == Some(1)
    });
    std::thread::sleep(quiet * 3);
    assert_eq!(fixture.refreshes(), before);
    assert!(fixture.pending());
}
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aft_index::data_layer::runtime::{
    composed_checks as checks, publication_fixture::PublicationFixture as IndexFixture,
};

use crate::config::Config;
use crate::context::{AppContext, Executor};
use crate::executor::{Lane, RootMaintenanceExecutor};
use crate::index_publication_fixture::{compose_context, PublicationFixture};
use crate::path_identity::ProjectRootId;

fn context() -> AppContext {
    AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        Config::default(),
    )
}

#[test]
fn memory_live_change_applies_to_next_cold_build_admission() {
    let ctx = context();
    checks::memory_live_change_applies_to_next_cold_build_admission(
        &|config| {
            ctx.set_config(config);
        },
        &|published, candidate| {
            let live = crate::config_live::apply_live_config(published, candidate, published);
            assert_eq!(live.applied, vec!["memory"]);
            assert!(live.deferred.is_empty());
            live.next
        },
        &crate::memory::profile::publish,
    );
}

#[cfg(target_os = "linux")]
#[test]
fn binding_many_roots_shares_waiting_threads_and_eviction_removes_deadlines() {
    checks::binding_many_roots_shares_waiting_threads_and_eviction_removes_deadlines(&|| {
        let manager = Arc::new(crate::inspect::InspectManager::new());
        manager.note_dead_code_request(Instant::now());
        Arc::new(move || manager.dead_code_timer_registration_for_test())
    });
}

impl checks::PublicationRouting for AppContext {
    fn set_worker_only_routes(&self, worker_only: bool) {
        self.set_worker_only_routes(worker_only);
    }

    fn set_standing_artifact_exempt(&self, exempt: bool) {
        self.set_standing_artifact_exempt(exempt);
    }
}

fn routing_context(fixture: &IndexFixture) -> Box<dyn checks::PublicationRouting> {
    Box::new(compose_context(fixture, AppContext::new))
}

#[test]
fn a_head_joining_worker_only_routes_arms_pending_changes_and_leaving_cancels_them() {
    checks::a_head_joining_worker_only_routes_arms_pending_changes_and_leaving_cancels_them(
        &routing_context,
    );
}

#[test]
fn an_admitted_refresh_finishes_after_routes_become_worker_only_without_rearming_quiet() {
    checks::an_admitted_refresh_finishes_after_routes_become_worker_only_without_rearming_quiet(
        &routing_context,
    );
}

#[test]
fn standing_roots_keep_quiet_refresh_with_worker_only_routes() {
    checks::standing_roots_keep_quiet_refresh_with_worker_only_routes(&routing_context);
}

#[test]
fn worker_only_publication_thresholds_do_not_schedule_follow_up_refreshes() {
    checks::worker_only_publication_thresholds_do_not_schedule_follow_up_refreshes(
        &routing_context,
    );
}

#[test]
fn worker_only_rescans_refresh_only_when_a_bind_requests_them() {
    checks::worker_only_rescans_refresh_only_when_a_bind_requests_them(&routing_context);
}

fn actor(
    root: &Path,
) -> (
    Arc<AppContext>,
    Executor,
    ProjectRootId,
    RootMaintenanceExecutor,
) {
    let ctx = Arc::new(context());
    ctx.mark_subc_bound();
    let executor = Executor::new();
    let root_id = ProjectRootId::from_path(root).unwrap();
    assert!(executor.register_actor(root_id.clone(), Arc::clone(&ctx)));
    let maintenance = ctx
        .callgraph_executor_for_test()
        .expect("registering the actor installs its executor");
    (ctx, executor, root_id, maintenance)
}

#[test]
fn admitted_work_runs_off_the_executor_and_blocks_no_other_work() {
    checks::admitted_work_runs_off_the_executor_and_blocks_no_other_work(|root| {
        let (_ctx, executor, root_id, maintenance) = actor(root);
        let read = move || {
            executor.submit(
                root_id.clone(),
                Lane::PureRead,
                "read".to_string(),
                Box::new(|_| crate::protocol::Response::success("read", serde_json::Value::Null)),
            )
        };
        (maintenance, Box::new(read))
    });
}

#[test]
fn an_unbound_root_builds_nothing_until_it_is_bound() {
    checks::an_unbound_root_builds_nothing_until_it_is_bound(|root| {
        let (ctx, executor, _, maintenance) = actor(root);
        let set_bound = move |bound| {
            let _keep_actor_alive = &executor;
            if bound {
                ctx.mark_subc_bound();
            } else {
                ctx.mark_subc_unbound();
            }
        };
        (maintenance, Box::new(set_bound))
    });
}

fn wait_until(ready: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !ready() {
        assert!(Instant::now() < deadline, "background work did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}
