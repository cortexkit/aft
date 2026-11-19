//! Expire unattended file-access asks without racing a retry of the same call.
use super::{ConsentError, ConsentTransport, HeldCall, Hold, Prepared, SubcConsentTransport};
use crate::context::App;
use crate::db::access_asks::AskKey;
use crate::db::access_grants::ScopeKey;
use crate::db::TrackedConnection;
use rusqlite::{params, Connection};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

pub(super) const IDLE_MS: i64 = 30 * 60 * 1000;
const SWEEP_PERIOD: Duration = Duration::from_secs(60);
const SWEEP_BUDGET: Duration = Duration::from_secs(5);
const ROW_LIMIT: i64 = 32;

type Db = Arc<Mutex<TrackedConnection>>;

enum Lease {
    Waiting(usize),
    Withdrawing(watch::Sender<()>),
}

static LEASES: LazyLock<Mutex<HashMap<AskKey, Lease>>> = LazyLock::new(Mutex::default);

pub(super) struct Guard(AskKey);

impl Drop for Guard {
    fn drop(&mut self) {
        let mut leases = LEASES.lock().unwrap();
        match leases.get_mut(&self.0) {
            Some(Lease::Waiting(count)) if *count > 1 => *count -= 1,
            _ => {
                leases.remove(&self.0);
            }
        }
    }
}

pub(super) struct CallUse {
    guard: Option<Guard>,
    row: Option<(Db, String)>,
}

impl CallUse {
    pub(super) fn remember(&mut self, call: &HeldCall, prep: &Prepared) {
        self.row = call
            .executor
            .actor_context(&call.identity.root)
            .and_then(|ctx| ctx.db())
            .map(|db| (db, prep.call_key.clone()));
    }

    pub(super) async fn finish(mut self) {
        if let Some(done) = self.persist_last_use() {
            let _ = done.await;
        }
    }

    fn persist_last_use(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        let (db, call_key) = self.row.take()?;
        let guard = self.guard.take().unwrap();
        let now = wall_now();
        // Keep the sweep excluded until the last-use time is durable, even if
        // the waiting future was cancelled. Never take aft.db on the runtime.
        Some(tokio::task::spawn_blocking(move || {
            if let Ok(conn) = db.lock() {
                let _ = touch(&conn, &guard.0, &call_key, now);
            }
            drop(guard);
        }))
    }
}

impl Drop for CallUse {
    fn drop(&mut self) {
        self.persist_last_use();
    }
}

pub(super) async fn watch(key: AskKey) -> CallUse {
    loop {
        let mut released = match join(&key) {
            Ok(guard) => {
                return CallUse {
                    guard: Some(guard),
                    row: None,
                }
            }
            Err(released) => released,
        };
        // The sweep drops its sender only after completing the attempt. Read
        // aft.db after that point so a retry cannot await a withdrawn card.
        let _ = released.changed().await;
    }
}

fn join(key: &AskKey) -> Result<Guard, watch::Receiver<()>> {
    let mut leases = LEASES.lock().unwrap();
    match leases.entry(key.clone()).or_insert(Lease::Waiting(0)) {
        Lease::Waiting(count) => *count += 1,
        Lease::Withdrawing(released) => return Err(released.subscribe()),
    }
    Ok(Guard(key.clone()))
}

pub(super) async fn watch_reconciliation(db: &Db, prep: &Prepared) -> Option<Guard> {
    let guard = join(&prep.key).ok()?;
    let key = prep.key.clone();
    let call_key = prep.call_key.clone();
    // Recovery is not a retry of the tool. Don't re-publish an old row with
    // no elicitation id just because the scope bound before the first sweep.
    let cutoff = wall_now().saturating_sub(IDLE_MS);
    blocking(db, move |conn| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM access_asks WHERE owner=?1 AND scope_ref=?2
             AND epoch=?3 AND args_hash=?4 AND call_key=?5 AND created_at>=?6)",
            params![
                key.scope.owner,
                key.scope.scope_ref,
                key.scope.epoch.to_string(),
                key.args_hash,
                call_key,
                cutoff
            ],
            |row| row.get::<_, bool>(0),
        )
    })
    .await
    .ok()?
    .then_some(guard)
}

pub(super) async fn watch_parked(db: &Db, key: &AskKey, call_key: &str) -> Option<CallUse> {
    let waiting = watch(key.clone()).await;
    let key = key.clone();
    let call_key = call_key.to_owned();
    // The sweep may already have withdrawn the parked card. In that case the
    // run must not withdraw it again or remove a fresh row for these arguments.
    blocking(db, move |conn| {
        let row = crate::db::access_asks::get(conn, &key)?;
        Ok(row.is_some_and(|row| row.call_key.as_ref() == Some(&call_key)))
    })
    .await
    .ok()?
    .then_some(waiting)
}

fn begin_withdrawal(key: &AskKey) -> Option<Guard> {
    let mut leases = LEASES.lock().unwrap();
    if leases.contains_key(key) {
        return None;
    }
    leases.insert(key.clone(), Lease::Withdrawing(watch::channel(()).0));
    Some(Guard(key.clone()))
}

pub(super) fn wall_now() -> i64 {
    crate::db::call_ledger::now_ms()
}

// No reader needs the original creation time. Keep the existing column name
// and schema so an older AFT can open the same database after a rollback.
pub(super) fn touch(
    conn: &Connection,
    key: &AskKey,
    call_key: &str,
    now: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE access_asks SET created_at=MAX(created_at,?6)
         WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND args_hash=?4 AND call_key=?5",
        params![
            key.scope.owner,
            key.scope.scope_ref,
            key.scope.epoch.to_string(),
            key.args_hash,
            call_key,
            now
        ],
    )?;
    Ok(())
}

#[derive(Clone)]
struct Candidate {
    rowid: i64,
    key: AskKey,
    call_key: String,
    elicitation: Option<String>,
    last_use: i64,
}

fn candidates(
    conn: &Connection,
    cursor: i64,
    cutoff: i64,
    limit: i64,
) -> rusqlite::Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT rowid,owner,scope_ref,epoch,args_hash,call_key,elicitation_id,created_at
         FROM access_asks WHERE rowid>?1 AND created_at<?2 AND call_key IS NOT NULL
         AND json_extract(pending,'$.mode') IN ('read','write') ORDER BY rowid LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![cursor, cutoff, limit], |row| {
        let epoch: String = row.get(3)?;
        let epoch = epoch.parse().map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        Ok(Candidate {
            rowid: row.get(0)?,
            key: AskKey {
                scope: ScopeKey {
                    owner: row.get(1)?,
                    scope_ref: row.get(2)?,
                    epoch,
                },
                args_hash: row.get(4)?,
            },
            call_key: row.get(5)?,
            elicitation: row.get(6)?,
            last_use: row.get(7)?,
        })
    })?;
    rows.collect()
}

impl Candidate {
    fn still_current(&self, conn: &Connection) -> rusqlite::Result<bool> {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM access_asks WHERE rowid=?1 AND call_key=?2
             AND created_at=?3 AND elicitation_id IS ?4)",
            params![self.rowid, self.call_key, self.last_use, self.elicitation],
            |row| row.get(0),
        )
    }
}

/// Remove only this card, not a replacement another dispatch already opened.
pub(super) fn discard_card(
    conn: &Connection,
    key: &AskKey,
    call_key: &str,
) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM access_asks WHERE owner=?1 AND scope_ref=?2 AND epoch=?3 AND args_hash=?4 AND call_key=?5",
        params![key.scope.owner, key.scope.scope_ref, key.scope.epoch.to_string(), key.args_hash, call_key],
    )?;
    Ok(())
}

pub(super) async fn refile_withdrawn(call: &HeldCall, prep: &Prepared) -> Hold {
    // Cingulate can close the card without AFT receiving the reply, or AFT can
    // exit before deleting the row. A replay must ask again rather than treat
    // that withdrawal as the person's answer.
    let key = prep.key.clone();
    let call_key = prep.call_key.clone();
    match super::with_db(call, move |conn, _| discard_card(conn, &key, &call_key)).await {
        Ok(()) => Hold::Refile(prep.grant.clone()),
        Err(error) => Hold::Answer(super::unavailable_answer(call, &prep.grant, &error)),
    }
}

async fn blocking<T: Send + 'static>(
    db: &Db,
    f: impl FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
) -> Result<T, String> {
    let db = Arc::clone(db);
    tokio::task::spawn_blocking(move || {
        let conn = db.lock().map_err(|_| "aft.db mutex poisoned")?;
        f(&conn).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Default)]
pub(super) struct Sweep {
    // Page by rowid so live or failing asks at the front cannot starve the
    // remainder. Wrap at the end to retry rows that were skipped or failed.
    cursor: i64,
    failed: HashSet<String>,
}

impl Sweep {
    pub(super) async fn run(
        &mut self,
        db: &Db,
        transport: Option<&dyn ConsentTransport>,
        limit: i64,
        budget: Duration,
        wall_ms: i64,
        now: impl Fn() -> Instant,
    ) -> Result<(), String> {
        let deadline = now() + budget;
        let cutoff = wall_ms.saturating_sub(IDLE_MS);
        let cursor = self.cursor;
        let rows = blocking(db, move |conn| candidates(conn, cursor, cutoff, limit)).await?;
        let end = (rows.len() as i64) < limit;
        for row in rows {
            if now() >= deadline {
                return Ok(());
            }
            let Some(_guard) = begin_withdrawal(&row.key) else {
                self.cursor = row.rowid;
                continue;
            };
            // A waiter could have refreshed or replaced this candidate after
            // the SELECT but before the sweep acquired its exclusive lease.
            let candidate = row.clone();
            if !blocking(db, move |conn| candidate.still_current(conn)).await? {
                self.cursor = row.rowid;
                continue;
            }
            let remaining = deadline.saturating_duration_since(now());
            if remaining.is_zero() {
                return Ok(());
            }
            self.cursor = row.rowid;
            if let Some(id) = &row.elicitation {
                let timeout = remaining.min(super::UNARY_TIMEOUT);
                let result = match transport {
                    Some(transport) => {
                        tokio::time::timeout(timeout, transport.withdraw_idle(id.clone(), timeout))
                            .await
                            .map_err(|_| "withdrawal timed out".to_owned())
                            .and_then(|reply| {
                                let settled = reply
                                    .and_then(|record| super::decode_record(&record))
                                    .map_err(|e| format!("{e:?}"))?;
                                if settled == super::Settlement::Pending {
                                    return Err("Cingulate returned an open card".into());
                                }
                                Ok(())
                            })
                    }
                    None => Err("Cingulate transport is unavailable".into()),
                };
                if let Err(error) = result {
                    if self.failed.insert(row.call_key.clone()) {
                        log::warn!("idle consent.withdraw failed for {id}: {error}");
                    }
                    continue;
                }
            }
            self.failed.remove(&row.call_key);
            blocking(db, move |conn| discard_card(conn, &row.key, &row.call_key)).await?;
        }
        if end {
            self.cursor = 0;
        }
        Ok(())
    }
}

pub(in crate::subc) fn start(app: Arc<App>, path: PathBuf) -> watch::Sender<()> {
    let (running, mut shutdown) = watch::channel(());
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SWEEP_PERIOD);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut sweep = Sweep::default();
        let mut db = None;
        loop {
            // Finish any withdrawal already sent before stopping the worker.
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                _ = interval.tick() => {},
            }
            if db.is_none() {
                let app = Arc::clone(&app);
                let path = path.clone();
                db = match tokio::task::spawn_blocking(move || app.open_db(&path)).await {
                    Ok(Ok(db)) => Some(db),
                    Ok(Err(error)) => {
                        log::warn!("idle consent sweep could not open aft.db: {error}");
                        None
                    }
                    Err(error) => {
                        log::warn!("idle consent sweep database worker failed: {error}");
                        None
                    }
                };
            }
            if let Some(db) = &db {
                let transport = super::transport();
                if let Err(error) = sweep
                    .run(
                        db,
                        transport.as_deref(),
                        ROW_LIMIT,
                        SWEEP_BUDGET,
                        wall_now(),
                        Instant::now,
                    )
                    .await
                {
                    log::warn!("idle consent sweep failed: {error}");
                }
            }
        }
    });
    running
}

pub(super) async fn withdraw_on_own_route(
    transport: Arc<SubcConsentTransport>,
    id: String,
    timeout: Duration,
) -> Result<serde_json::Value, ConsentError> {
    use subc_client_rs::{CallError, CallOptions};
    use subc_protocol::RouteTarget;
    let body = super::encode("consent.withdraw", &json!({"elicitation_id": id}))?;
    let consumer = transport.consumer().await?;
    // The authenticated AFT principal owns the card. Unlike a new request,
    // withdrawal needs no live asker scope or original bind session.
    let identity = subc_protocol::BindIdentity::new("aft-consent", "idle-file-access");
    let route_identity = transport.route_identity(super::ConsentRouteKey {
        harness: identity.harness.clone(),
        session: identity.session.clone(),
        project_root: PathBuf::new(),
        scope_owner: String::new(),
        scope_ref: String::new(),
        scope_epoch: None,
    });
    let gate = route_identity.state.gate.lock().await;
    let options = || CallOptions {
        timeout,
        route_retry_deadline: timeout,
        ..CallOptions::default()
    };
    let route = consumer
        .open_route(
            RouteTarget::ManagementSurface {
                module_id: super::CINGULATE_MODULE_ID.into(),
            },
            identity,
            options(),
        )
        .await
        .map_err(|error| ConsentError::Unavailable(error.to_string()))?;
    *route_identity
        .state
        .leases
        .lock()
        .unwrap()
        .entry(route)
        .or_default() += 1;
    drop(gate);
    let _lease = super::ConsentRoute {
        consumer: Arc::clone(&consumer),
        route: Some((route, route_identity)),
        runtime: tokio::runtime::Handle::current(),
    };
    match consumer.request(&route, body, options()).await {
        Ok(bytes) => super::decode_result(&bytes),
        Err(CallError::Module(body)) => Err(ConsentError::Refused {
            code: body.code,
            message: body.message,
        }),
        Err(error) => {
            transport.forget(&consumer).await;
            Err(ConsentError::Unavailable(error.to_string()))
        }
    }
}
