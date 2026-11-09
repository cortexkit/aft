//! Reconcile saved grants with daemon lifecycle state, away from tool calls.
use super::super::{readiness, scope_describe, DispatchPathMetrics, Executor};
use crate::db::{access_grants as store, TrackedConnection};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subc_protocol::{scope::ScopeStamp, session::ModuleControlResponseToModule};

const PER_TICK: usize = 4;
const INTERVAL: Duration = Duration::from_secs(5);
type Db = Arc<Mutex<TrackedConnection>>;

#[derive(Default, Serialize)]
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
    }
}

fn stamp(key: &store::ScopeKey) -> Result<ScopeStamp, serde_json::Error> {
    serde_json::from_value(serde_json::json!({
        "owner": serde_json::from_str::<serde_json::Value>(&key.owner)?,
        "ref": key.scope_ref, "scope_epoch": key.epoch,
        "kind": "worker", "owner_authorized": true,
    }))
}

async fn tick<F, Fut>(db: Db, cursor: &mut Option<store::ScopeKey>, describe: F) -> Stats
where
    F: Fn(ScopeStamp) -> Fut,
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
        if !replies.contains_key(&reference) {
            stats.described += 1;
            let reply = tokio::time::timeout(scope_describe::describe_timeout(), describe(request))
                .await
                .unwrap_or(Err(readiness::ControlError::Timeout));
            match &reply {
                Ok(reply) if scope_describe::describe_answer_ends_scope(reply) => stats.ended += 1,
                Ok(ModuleControlResponseToModule::ScopeDescribe {
                    status: subc_protocol::scope::ScopeStatus::Live,
                    ..
                }) => stats.live += 1,
                Ok(_) => stats.indeterminate += 1,
                Err(readiness::ControlError::Timeout) => stats.timeouts += 1,
                Err(_) => stats.errors += 1,
            }
            replies.insert(reference.clone(), reply);
        }
        if replies[&reference]
            .as_ref()
            .is_ok_and(scope_describe::describe_answer_ends_scope)
        {
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
pub(in crate::subc) async fn run(control: Arc<readiness::DaemonControl>, executor: Arc<Executor>) {
    let mut cursors = HashMap::new();
    let mut database = 0usize;
    loop {
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
                let stats = tick(
                    db.clone(),
                    cursors.entry(path.clone()).or_default(),
                    |stamp| {
                        let control = control.clone();
                        async move { control.describe(&stamp).await }
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

fn report(metrics: &DispatchPathMetrics, stats: Stats) {
    metrics
        .scope_descriptions
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .grant_sweep(stats);
}

#[cfg(test)]
mod tests;
