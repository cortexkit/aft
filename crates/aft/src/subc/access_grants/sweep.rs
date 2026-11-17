//! Reconcile saved grants with daemon lifecycle state, away from tool calls.
use super::super::{readiness, scope_describe, DispatchPathMetrics, Executor};
use crate::db::{access_grants as store, TrackedConnection};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subc_protocol::{scope::ScopeStamp, session::ModuleControlResponseToModule};

const PER_TICK: usize = 4;
const INTERVAL: Duration = Duration::from_secs(5);
type Db = Arc<Mutex<TrackedConnection>>;

#[derive(Default)]
pub(in crate::subc) struct Stats {
    pub ticks: u64,
    pub described: u64,
    pub ended: u64,
    pub deleted_grants: u64,
    pub live: u64,
    pub indeterminate: u64,
    pub errors: u64,
    pub timeouts: u64,
    pub database_errors: u64,
    pub last_pass_at_ms: i64,
}

impl Stats {
    pub(in crate::subc) fn add(&mut self, other: Self) {
        self.ticks += other.ticks;
        self.described += other.described;
        self.ended += other.ended;
        self.deleted_grants += other.deleted_grants;
        self.live += other.live;
        self.indeterminate += other.indeterminate;
        self.errors += other.errors;
        self.timeouts += other.timeouts;
        self.database_errors += other.database_errors;
        self.last_pass_at_ms = other.last_pass_at_ms;
    }
}

pub(in crate::subc) fn stamp(key: &store::ScopeKey) -> Result<ScopeStamp, serde_json::Error> {
    serde_json::from_value(serde_json::json!({
        "owner": serde_json::from_str::<serde_json::Value>(&key.owner)?,
        "ref": key.scope_ref, "scope_epoch": key.epoch,
        "kind": "worker", "owner_authorized": true,
    }))
}

#[cfg(test)]
async fn tick<F, Fut>(db: Db, cursor: &mut Option<store::ScopeKey>, describe: F) -> Stats
where
    F: Fn(ScopeStamp) -> Fut,
    Fut: std::future::Future<
        Output = Result<ModuleControlResponseToModule, readiness::ControlError>,
    >,
{
    tick_unwatched(db, cursor, describe, |_| false).await
}

async fn tick_unwatched<F, Fut, C>(
    db: Db,
    cursor: &mut Option<store::ScopeKey>,
    describe: F,
    covered: C,
) -> Stats
where
    F: Fn(ScopeStamp) -> Fut,
    C: Fn(&ScopeStamp) -> bool,
    Fut: std::future::Future<
        Output = Result<ModuleControlResponseToModule, readiness::ControlError>,
    >,
{
    let mut stats = Stats {
        ticks: 1,
        ..Stats::default()
    };
    let after = cursor.clone();
    let read_db = db.clone();
    let batch = tokio::task::spawn_blocking(move || {
        let conn = read_db
            .lock()
            .map_err(|_| "grant database lock poisoned".to_string())?;
        store::saved_scope_batch(&conn, after.as_ref(), PER_TICK).map_err(|error| error.to_string())
    })
    .await;
    let keys = match batch {
        Ok(Ok(keys)) => keys,
        _ => {
            stats.database_errors += 1;
            return stats;
        }
    };
    // Empty pages wrap on the next tick; live scopes cannot starve later rows.
    *cursor = keys.last().cloned();
    let mut replies = HashMap::new();
    for key in keys {
        let request = match stamp(&key) {
            Ok(stamp) => stamp,
            Err(_) => {
                stats.errors += 1;
                continue;
            }
        };
        let reference = (key.owner.clone(), key.scope_ref.clone());
        if covered(&request) {
            continue;
        }
        if !replies.contains_key(&reference) {
            stats.described += 1;
            let reply = tokio::time::timeout(
                scope_describe::describe_timeout(),
                describe(request.clone()),
            )
            .await
            .unwrap_or(Err(readiness::ControlError::Timeout));
            let mut ends = reply.as_ref().is_ok_and(|reply| {
                scope_describe::describe_answer_ends_scope(reply)
                    || crate::prefrontal::retired(&request, reply)
            });
            if ends {
                if let Some(other) = crate::prefrontal::counterpart(&request) {
                    let answer = tokio::time::timeout(
                        scope_describe::describe_timeout(),
                        describe(other.clone()),
                    )
                    .await
                    .unwrap_or(Err(readiness::ControlError::Timeout));
                    let (absent, outcome) =
                        match crate::prefrontal::continuation(&other, answer.as_ref().ok()) {
                            crate::prefrontal::Continuation::Live(_) => (false, "continuation"),
                            crate::prefrontal::Continuation::Hold => (false, "hold"),
                            crate::prefrontal::Continuation::Absent => (true, "absent_end"),
                        };
                    crate::prefrontal::log_continuation(&request, &other, outcome);
                    ends = absent;
                }
            } else if let Some(other) = crate::prefrontal::counterpart(&request) {
                crate::prefrontal::log_continuation(
                    &request,
                    &other,
                    crate::prefrontal::retained_outcome(reply.as_ref().ok()),
                );
            }
            match &reply {
                Ok(_) if ends => stats.ended += 1,
                Ok(ModuleControlResponseToModule::ScopeDescribe {
                    status: subc_protocol::scope::ScopeStatus::Live,
                    ..
                }) => stats.live += 1,
                Ok(_) => stats.indeterminate += 1,
                Err(readiness::ControlError::Timeout) => stats.timeouts += 1,
                Err(_) => stats.errors += 1,
            }
            replies.insert(reference.clone(), ends);
        }
        if replies[&reference] {
            let db = db.clone();
            match tokio::task::spawn_blocking(move || {
                let conn = db
                    .lock()
                    .map_err(|_| "grant database lock poisoned".to_string())?;
                store::end_saved_scope(&conn, &key).map_err(|error| error.to_string())
            })
            .await
            {
                Ok(Ok(deleted)) => stats.deleted_grants += deleted as u64,
                _ => stats.database_errors += 1,
            }
        }
    }
    stats
}

/// Owned by the connection's scope worker: cancellation stops the sweep too.
pub(in crate::subc) async fn run(
    control: Arc<readiness::DaemonControl>,
    executor: Arc<Executor>,
    mut covered: tokio::sync::watch::Receiver<super::super::scope_watch::Coverage>,
) {
    let mut cursors = HashMap::new();
    let mut database = 0usize;
    loop {
        while covered.borrow_and_update().all {
            if covered.changed().await.is_err() {
                return;
            }
        }
        let executor = executor.clone();
        let dbs = tokio::task::spawn_blocking(move || {
            let mut dbs = BTreeMap::new();
            for ctx in executor.actor_contexts() {
                if let Some(db) = ctx.db() {
                    let path = db
                        .lock()
                        .ok()
                        .map(|conn| conn.path().unwrap_or("").to_string());
                    if let Some(path) = path {
                        dbs.entry(path).or_insert(db);
                    }
                }
            }
            dbs.into_iter().collect::<Vec<_>>()
        })
        .await;
        match dbs {
            Ok(dbs) if !dbs.is_empty() => {
                let (path, db) = &dbs[database % dbs.len()];
                database = database.wrapping_add(1);
                let stats = tick_unwatched(
                    db.clone(),
                    cursors.entry(path.clone()).or_default(),
                    |stamp| {
                        let control = control.clone();
                        async move { control.describe(&stamp).await }
                    },
                    |stamp| {
                        // A snapshot may arrive while the database batch is
                        // being read. Check coverage at the actual RPC boundary.
                        let latest = covered.borrow();
                        latest.all || latest.keys.contains(&scope_describe::stamp_key(stamp))
                    },
                )
                .await;
                report(&control.metrics, stats);
            }
            Err(_) => report(
                &control.metrics,
                Stats {
                    database_errors: 1,
                    ..Stats::default()
                },
            ),
            _ => {}
        }
        tokio::time::sleep(INTERVAL).await;
    }
}

fn report(metrics: &DispatchPathMetrics, mut stats: Stats) {
    stats.last_pass_at_ms = crate::db::access_grant_rows::now_ms();
    metrics
        .scope_descriptions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .grant_sweep(stats);
}

#[cfg(test)]
mod tests;
