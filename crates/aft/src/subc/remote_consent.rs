//! Authorize preflighted remote calls without borrowing authority from a local bind.
use super::*;
use crate::db::remote_grant_rows::{self as store, Key, Mode};
use std::collections::VecDeque;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const AWAIT_TIMEOUT: Duration = Duration::from_secs(120);

type Db = Arc<StdMutex<crate::db::TrackedConnection>>;
type StampKey = (String, String, u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::subc) enum Authorization {
    Once,
    Saved(String),
    WorkerRun,
}

pub(in crate::subc) type Decision = Result<(Preflight, Authorization), ErrorBody>;

pub(super) fn denied() -> ErrorBody {
    ErrorBody::new(
        "remote_access_denied",
        "Denied by the person on this computer.",
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Folder {
    key: Key,
    path: PathBuf,
}

impl Folder {
    fn path_text(&self) -> &str {
        // The path comes from an Entorhinal JSON root record.
        self.path.to_str().expect("registered root is UTF-8")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Group {
    folder: Folder,
    mode: Mode,
    worker: Option<StampKey>,
    can_ask: bool,
}

fn display(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

fn reason(agent: &str, peer: &str, folder: &str, mode: Mode) -> String {
    let action = match mode {
        Mode::Read => "read files in",
        Mode::ReadWrite => "read and edit files in",
    };
    let mut names = [agent, peer, folder].map(|name| name.chars().collect::<Vec<_>>());
    let sentence_len = format!(" on  wants to {action} .").len();
    let mut excess = (sentence_len + names.iter().map(Vec::len).sum::<usize>()).saturating_sub(160);
    for name in &mut names {
        let cut = excess.min(name.len().saturating_sub(1));
        if cut != 0 {
            name.truncate(name.len() - cut);
            *name
                .last_mut()
                .expect("shortened name still has one character") = '…';
            excess -= cut;
        }
    }
    let [agent, peer, folder] = names.map(|name| name.into_iter().collect::<String>());
    format!("{agent} on {peer} wants to {action} {folder}.")
}

struct Card {
    open: consent::ConsentCall,
    label: String,
    project: FederatedProject,
}

impl Card {
    fn prepare(call: &Preflight, can_ask: bool) -> (Group, Self) {
        let (project, caller) =
            admit_scope(Some(&call.scope)).expect("preflight admitted the scope");
        let facts = call
            .scope
            .attributes
            .federated
            .as_ref()
            .expect("admitted computer");
        let claim = facts.remote_claim.as_ref().expect("admitted claim");
        let (agent_name, worker) = match claim {
            RemoteClaim::Agent { agent_name, .. } | RemoteClaim::Run { agent_name, .. } => {
                (agent_name, None)
            }
            RemoteClaim::Worker { agent_name, .. } => {
                (agent_name, Some(scope_describe::stamp_key(&call.scope)))
            }
            _ => unreachable!("preflight admitted the claim"),
        };
        let agent = display(agent_name.as_deref().unwrap_or(caller));
        let peer = display(&facts.peer_name);
        let group = Group {
            folder: Folder {
                key: Key {
                    pairing_id: facts.pairing_id.clone(),
                    caller_id: caller.into(),
                    project_id: project.project_id.clone(),
                    root_kind: match project.root_key.kind {
                        RootKeyKind::Remote => "remote",
                        RootKeyKind::Label => "label",
                    },
                    root_key: project.root_key.key.clone(),
                },
                path: call.folder.clone(),
            },
            mode: call.mode,
            worker,
            can_ask,
        };
        let path = group.folder.path_text();
        let folder = display(
            call.folder
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(path),
        );
        let pattern = format!("{}/**", path.trim_end_matches(std::path::MAIN_SEPARATOR));
        let open = consent::ConsentCall {
            scope: consent::selector(&call.scope),
            project_root: call.folder.clone(),
            session: call.session.clone(),
            method: "consent.request",
            params: consent::request_params(
                &consent::new_call_key(),
                &call.tool,
                if call.mode == Mode::Read {
                    "read"
                } else {
                    "write"
                },
                path,
                &reason(&agent, &peer, &folder, call.mode),
                &call.args_hash,
                &pattern,
                None,
            ),
            timeout: REQUEST_TIMEOUT,
        };
        (
            group,
            Self {
                open,
                label: format!("{agent} on {peer}"),
                project: project.clone(),
            },
        )
    }

    fn rpc(&self, method: &'static str, params: Value, timeout: Duration) -> consent::ConsentCall {
        consent::ConsentCall {
            method,
            params,
            timeout,
            ..self.open.clone()
        }
    }

    async fn answer(
        &self,
        transport: &dyn consent::ConsentTransport,
        cancel: &CancellationToken,
    ) -> Result<(bool, String), ErrorBody> {
        let reply = transport
            .call(self.open.clone())
            .await
            .map_err(|_| admission_refusal())?;
        let id = consent::decode_elicitation_id(&reply).map_err(|_| admission_refusal())?;
        loop {
            let await_call = transport.call(self.rpc(
                "consent.await",
                json!({"elicitation_id": id, "timeout_ms": AWAIT_TIMEOUT.as_millis() as u64}),
                AWAIT_TIMEOUT + REQUEST_TIMEOUT,
            ));
            let record = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    let _ = transport.call(self.rpc("consent.withdraw", json!({"elicitation_id": id}), REQUEST_TIMEOUT)).await;
                    return Err(denied());
                }
                record = await_call => record.map_err(|_| admission_refusal())?,
            };
            match consent::decode_record(&record).map_err(|_| admission_refusal())? {
                consent::Settlement::Pending => continue,
                consent::Settlement::Allow {
                    scope,
                    by_user: true,
                } => return Ok((scope, id)),
                _ => return Err(denied()),
            }
        }
    }
}

struct Waiter {
    call: Preflight,
    receiver: oneshot::Sender<Decision>,
    lifetime: scope_describe::remote::Lifetime,
}

impl Waiter {
    fn finish(self, result: Result<Authorization, ErrorBody>) {
        let _ = self
            .receiver
            .send(result.map(|authorization| (self.call, authorization)));
    }
}

struct Pending {
    waiters: VecDeque<Waiter>,
    generation: u64,
    asking_scope: StampKey,
    cancel: CancellationToken,
}

enum Outcome {
    Saved(String),
    Approved {
        scope: bool,
        id: String,
        label: String,
        current_root: PathBuf,
    },
}

pub(in crate::subc) struct Completion {
    group: Group,
    generation: u64,
    result: Result<Outcome, ErrorBody>,
}

/// One instance serves the module loop, not a single remote channel.
/// The loop owns queue order, scope ends and the final live-bind check. Card
/// I/O and the fresh registry lookup run on separate tasks.
pub(in crate::subc) struct Authorizer {
    db: Option<Db>,
    connection_file: PathBuf,
    transport: Option<Arc<dyn consent::ConsentTransport>>,
    pending: HashMap<Group, Pending>,
    worker_grants: HashMap<(Folder, StampKey), (Mode, scope_describe::remote::Lifetime)>,
    tasks: tokio::task::JoinSet<Completion>,
    generation: u64,
}

impl Authorizer {
    pub(in crate::subc) fn new(db: Option<Db>, connection_file: PathBuf) -> Self {
        Self {
            db,
            connection_file,
            transport: consent::transport(),
            pending: HashMap::new(),
            worker_grants: HashMap::new(),
            tasks: tokio::task::JoinSet::new(),
            generation: 0,
        }
    }

    #[cfg(test)]
    pub(in crate::subc) fn request(&mut self, call: Preflight) -> oneshot::Receiver<Decision> {
        self.request_with_lifetime(call, true, scope_describe::remote::Lifetime::default())
    }

    pub(in crate::subc) fn request_with_lifetime(
        &mut self,
        call: Preflight,
        can_ask: bool,
        lifetime: scope_describe::remote::Lifetime,
    ) -> oneshot::Receiver<Decision> {
        let (receiver, result) = oneshot::channel();
        let (group, card) = Card::prepare(&call, can_ask);
        let waiter = Waiter {
            call,
            receiver,
            lifetime,
        };
        if group.worker.is_some() && !can_ask {
            waiter.finish(Err(admission_refusal()));
        } else if group.worker.as_ref().is_some_and(|scope| {
            self.worker_grants
                .get(&(group.folder.clone(), scope.clone()))
                .is_some_and(|(mode, _)| mode.covers(group.mode))
        }) {
            waiter.finish(Ok(Authorization::WorkerRun));
        } else if let Some(pending) = self.pending.get_mut(&group) {
            pending.waiters.push_back(waiter);
        } else {
            self.start(group, card, VecDeque::from([waiter]));
        }
        result
    }

    pub(in crate::subc) fn set_db(&mut self, db: Db) {
        self.db = Some(db);
    }

    fn start(&mut self, group: Group, card: Card, waiters: VecDeque<Waiter>) {
        self.generation += 1;
        let generation = self.generation;
        let cancel = CancellationToken::new();
        let db = self.db.clone();
        let transport = self.transport.clone();
        let connection_file = self.connection_file.clone();
        #[cfg(test)]
        let fixture = TEST_REGISTRY.try_with(Arc::clone).ok();
        let asking_scope = scope_describe::stamp_key(&waiters[0].call.scope);
        self.pending.insert(
            group.clone(),
            Pending {
                waiters,
                generation,
                asking_scope,
                cancel: cancel.clone(),
            },
        );
        self.tasks.spawn(async move {
            let result = async {
                if group.worker.is_none() {
                    let folder = group.folder.clone();
                    let mode = group.mode;
                    let db = db.ok_or_else(admission_refusal)?;
                    let grant = tokio::task::spawn_blocking(move || {
                        let conn = db.lock().map_err(|_| admission_refusal())?;
                        store::find(&conn, &folder.key, folder.path_text(), mode)
                            .map_err(|_| admission_refusal())
                    })
                    .await
                    .map_err(|_| admission_refusal())??;
                    if let Some(id) = grant {
                        return Ok(Outcome::Saved(id));
                    }
                }
                if !group.can_ask {
                    return Err(admission_refusal());
                }
                let transport = transport.ok_or_else(admission_refusal)?;
                if cancel.is_cancelled() {
                    return Err(denied());
                }
                let (scope, id) = card.answer(&*transport, &cancel).await?;
                let lookup = enumerate(
                    #[cfg(test)]
                    fixture,
                    connection_file,
                );
                let registry = tokio::time::timeout(LOOKUP_TIMEOUT, lookup)
                    .await
                    .map_err(|_| admission_refusal())?
                    .map_err(|_| admission_refusal())?;
                let current_root = registered_root(&card.project, &registry)?;
                Ok(Outcome::Approved {
                    scope,
                    id,
                    label: card.label,
                    current_root,
                })
            }
            .await;
            Completion {
                group,
                generation,
                result,
            }
        });
    }

    pub(in crate::subc) async fn next(&mut self) -> Completion {
        loop {
            match self.tasks.join_next().await {
                Some(Ok(completion)) => return completion,
                Some(Err(error)) => log::warn!("remote consent task failed: {error}"),
                None => std::future::pending::<()>().await,
            }
        }
    }

    pub(in crate::subc) fn finish(
        &mut self,
        completion: Completion,
        root_channels: &HashMap<ProjectRootId, HashSet<RouteChannel>>,
        routes: &HashMap<RouteChannel, RouteIdentity>,
    ) {
        let Completion {
            group,
            generation,
            result,
        } = completion;
        if self
            .pending
            .get(&group)
            .is_none_or(|pending| pending.generation != generation)
        {
            return;
        }
        let mut waiters = self
            .pending
            .remove(&group)
            .expect("matching generation")
            .waiters;
        if waiters.iter().all(|waiter| waiter.receiver.is_closed()) {
            return;
        }
        let result = result.and_then(|outcome| {
            if matches!(&outcome, Outcome::Approved { current_root, .. } if *current_root != group.folder.path) {
                return Err(not_registered());
            }
            locally_bound_root(&group.folder.path, root_channels, routes)?;
            match outcome {
                Outcome::Saved(id) => Ok(Authorization::Saved(id)),
                Outcome::Approved { scope, id, label, .. } => {
                    if let Some(worker) = &group.worker {
                        let (mode, _) = self.worker_grants.entry((group.folder.clone(), worker.clone())).or_insert_with(|| {
                            (group.mode, waiters[0].lifetime.clone())
                        });
                        if !mode.covers(group.mode) {
                            *mode = group.mode;
                        }
                        Ok(Authorization::WorkerRun)
                    } else if scope {
                        let db = self.db.as_ref().ok_or_else(admission_refusal)?;
                        let conn = db.lock().map_err(|_| admission_refusal())?;
                        let id = store::save(&conn, &group.folder.key, group.folder.path_text(), group.mode, &label, &id)
                            .map_err(|_| admission_refusal())?;
                        Ok(Authorization::Saved(id))
                    } else {
                        Ok(Authorization::Once)
                    }
                }
            }
        });
        if result
            .as_ref()
            .is_ok_and(|authority| *authority == Authorization::Once)
        {
            // The first waiter owns the displayed card, even if it closed
            // while the person's answer was in flight.
            waiters.pop_front().expect("card owner").finish(result);
            waiters.retain(|waiter| !waiter.receiver.is_closed());
            if let Some(next) = waiters.front() {
                let (_, card) = Card::prepare(&next.call, group.can_ask);
                self.start(group, card, waiters);
            }
        } else {
            for waiter in waiters {
                waiter.finish(result.clone());
            }
        }
    }

    /// A confirmed end retires the cards opened by that stamp, as well as
    /// any worker approvals and followers from the ended scope.
    pub(in crate::subc) fn end_scope(&mut self, pairing: &str, stamp: &ScopeStamp) {
        let ended = scope_describe::stamp_key(stamp);
        self.worker_grants
            .retain(|(folder, scope), _| folder.key.pairing_id != pairing || *scope != ended);
        self.pending.retain(|group, pending| {
            if group.folder.key.pairing_id != pairing {
                return true;
            }
            let asking_scope_ended = pending.asking_scope == ended;
            let mut kept = VecDeque::new();
            for waiter in pending.waiters.drain(..) {
                if asking_scope_ended || scope_describe::stamp_key(&waiter.call.scope) == ended {
                    waiter.finish(Err(denied()));
                } else {
                    kept.push_back(waiter);
                }
            }
            pending.waiters = kept;
            if pending.waiters.is_empty() {
                pending.cancel.cancel();
                return false;
            }
            true
        });
    }
}

#[cfg(test)]
#[path = "remote_consent_tests.rs"]
mod tests;
