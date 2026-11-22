//! Persisted-task fixture used only by supervised real-daemon notice tests.

use super::*;

pub struct RunnerNoticeRecoveryForTest {
    registry: crate::bash_background::BgTaskRegistry,
    task: String,
    root: ProjectRootId,
    route: std::sync::Mutex<Option<RouteIdentity>>,
}

impl RunnerNoticeRecoveryForTest {
    pub fn new(
        connection: &Path,
        storage: &Path,
        root: &Path,
        task: &str,
        module: &str,
        raw: &str,
        scope: subc_protocol::scope::ScopeStamp,
    ) -> Self {
        use crate::bash_background::persistence::{
            create_task_layout, write_task_at, PersistedTask,
        };
        let owner = crate::bash_background::TaskIdentity {
            principal: Some(format!("reserved:{module}")),
            harness: "runner".into(),
            session: task.into(),
        };
        let _identity = crate::bash_background::task_identity_scope(Some(owner));
        let paths = create_task_layout(storage, task, task).unwrap();
        let mut metadata = PersistedTask::starting(
            task.into(),
            task.into(),
            "echo recovered".into(),
            root.into(),
            Some(root.into()),
            None,
            true,
            false,
        );
        metadata.mark_terminal(
            crate::bash_background::BgTaskStatus::Completed,
            Some(0),
            None,
        );
        metadata.launching_raw_harness = Some(raw.into());
        metadata.scope = Some(scope);
        metadata.runner_completion_offers = 100;
        write_task_at(&paths, &metadata).unwrap();
        let registry = crate::bash_background::BgTaskRegistry::default();
        registry.replay_session(storage, task).unwrap();
        install(connection.into());
        Self {
            registry,
            task: task.into(),
            root: ProjectRootId::from_path(root).unwrap(),
            route: Default::default(),
        }
    }

    pub fn offer(&self) {
        offer_replayed(
            self.registry.clone(),
            &[(self.task.clone(), vec![self.task.clone()])],
        );
    }

    pub fn waiting_for_harness(&self) -> bool {
        scheduler()
            .unwrap()
            .entries
            .lock()
            .unwrap()
            .get(&format!("aft:bash:{}", self.task))
            .is_some_and(|entry| entry.retrying && entry.work.is_some())
            && self
                .registry
                .notice_metadata(&self.task)
                .unwrap()
                .runner_completion_offers
                == 100
    }

    pub fn delivered(&self) -> bool {
        self.registry
            .notice_metadata(&self.task)
            .unwrap()
            .completion_delivered
    }

    /// Feed an actual daemon bind into the same re-offer hook as the AFT loop.
    pub fn bound(&self, bind: &subc_client_rs::RouteBindRequest) {
        let principal_id = match &bind.principal {
            Some(Principal::Reserved { module_id }) => Some(format!("reserved:{module_id}")),
            _ => panic!("fixture expects a supervised runner carrier"),
        };
        let identity = RouteIdentity(Arc::new(RouteIdentityData {
            output_scope: Default::default(),
            root: self.root.clone(),
            project_root: self.root.as_path().into(),
            harness: bind.identity.harness.clone(),
            session: bind.identity.session.clone(),
            role: tool_provider::RouteRole::Legacy,
            trust: BindTrust::FirstParty,
            spawn_principal: AuthenticatedPrincipal::RouteBind {
                trust: crate::sandbox_spawn::PrincipalTrust::FirstParty,
                route_channel: 0,
                route_epoch: 0,
                project_root: self.root.as_path().into(),
                harness: bind.identity.harness.clone(),
                session_id: bind.identity.session.clone(),
                principal_id,
            },
            consumer_elicitation_capable: false,
            held_reply: false,
            held_update: false,
            call_keepalive: false,
            call_yield: false,
            tool_result_body: false,
            disabled_tools: Arc::default(),
            scope: bind.scope.clone(),
            call_mode: bind.call_mode.clone(),
            run_scope: Default::default(),
            made_tool_call: AtomicBool::new(false),
            run_owner: None,
        }));
        route_bound(self.registry.clone(), &identity);
        *self.route.lock().unwrap() = Some(identity);
    }
}

impl Drop for RunnerNoticeRecoveryForTest {
    fn drop(&mut self) {
        if let Some(route) = self.route.lock().unwrap().take() {
            route_unbound(&route);
        }
        self.registry.detach();
    }
}
