//! Project-scoped settings slots shared by the routes bound to a root.
use super::*;
use std::sync::Weak;
use tokio::task::JoinSet;

#[derive(Default)]
pub(super) struct Projects(parking_lot::Mutex<HashMap<String, Weak<Project>>>);

pub(in crate::subc) struct Project {
    pub(in crate::subc) id: String,
    pub(in crate::subc) settings: ApprovedUser,
    schema: Arc<Schema>,
    // Outermost None means no fetch has finished. Some(None) remembers an
    // unknown project until the daemon supplies a subsequent change hint.
    view: tokio::sync::Mutex<Option<Option<ConfigView>>>,
}

impl Projects {
    pub(super) fn slot(&self, id: &str, user: &ApprovedUser, schema: &Arc<Schema>) -> Arc<Project> {
        let mut slots = self.0.lock();
        if let Some(project) = slots.get(id).and_then(Weak::upgrade) {
            return project;
        }
        slots.retain(|_, slot| slot.strong_count() != 0);
        let project = Arc::new(Project {
            id: id.into(),
            settings: ApprovedUser::project(user),
            schema: schema.clone(),
            view: tokio::sync::Mutex::new(None),
        });
        slots.insert(id.into(), Arc::downgrade(&project));
        project
    }

    pub(super) fn existing(&self, id: &str) -> Option<Arc<Project>> {
        self.0.lock().get(id).and_then(Weak::upgrade)
    }
}

impl Project {
    pub(super) async fn fetch(
        &self,
        control: &readiness::DaemonControl,
        executor: &Arc<Executor>,
        first: bool,
    ) -> Result<(), &'static str> {
        let mut current = self.view.lock().await;
        if first && current.is_some() {
            return Ok(());
        }
        let mut input = ConfigGetRequest::new();
        input.project_id = Some(self.id.clone());
        let reply: ConfigGetReply = match request(control, &input).await {
            Ok(reply) => reply,
            Err("unknown_project") => {
                self.settings.use_user_view();
                *current = Some(None);
                reload_roots(executor, &self.settings).await;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let recovery = reply.pending.as_ref().and(reply.active.as_ref()).cloned();
        let next = project_view(reply, Some(&self.id))?;
        if current
            .as_ref()
            .and_then(Option::as_ref)
            .is_some_and(|old| next.generation < old.generation)
        {
            return Ok(());
        }
        match project_tier(&self.schema, &next) {
            Ok(tier) => {
                self.settings.replace(tier);
                reload_roots(executor, &self.settings).await;
                *current = Some(Some(next.clone()));
                report(control, &next, Ok(())).await
            }
            Err(error) => {
                report(control, &next, Err(error)).await?;
                if current.is_some() {
                    return Err(error);
                }
                if let Some(previous) = recovery {
                    let previous =
                        project_view(ConfigGetReply::new(Some(previous), None), Some(&self.id))?;
                    self.settings
                        .replace(project_tier(&self.schema, &previous)?);
                    *current = Some(Some(previous));
                } else {
                    *current = Some(None);
                }
                Ok(())
            }
        }
    }
}

fn project_tier(schema: &Schema, view: &ConfigView) -> Result<ConfigTier, &'static str> {
    // Before approvals are enabled, the daemon checks the schema but does not
    // ask AFT to review files. Check AFT's additional rules before using the view.
    let mut layers = Vec::new();
    for snapshot in &view.layers {
        let Some(bytes) = snapshot.bytes.as_deref() else {
            continue;
        };
        let layer = snapshot
            .layer
            .known_layer()
            .ok_or("invalid_settings_view")?;
        let value = review::parse_snapshot(bytes).ok_or("invalid_settings_view")?;
        if layer != subc_config::Layer::User && review::forbidden_override(&value).is_some() {
            return Err("invalid_settings_view");
        }
        layers.push(LayerValue::new(layer, value));
    }
    review::validate_harness_bounds(
        schema,
        &layers,
        &ValidationContext::new("aft", view.project_id.clone()),
    )
    .map_err(|_| "invalid_settings_view")?;
    let mut effective = view.effective.clone();
    prioritize_layers_over_harnesses(&mut effective, &view.sources);
    validate_user(&effective).map_err(|_| "invalid_settings_view")
}

fn prioritize_layers_over_harnesses(
    value: &mut Value,
    sources: &std::collections::BTreeMap<String, subc_config::Layer>,
) {
    fn prune(
        value: &mut Value,
        path: &str,
        harness: &str,
        sources: &std::collections::BTreeMap<String, subc_config::Layer>,
    ) -> bool {
        if let Some(fields) = value.as_object_mut() {
            fields.retain(|key, value| {
                prune(
                    value,
                    &format!("{path}/{}", review::pointer_segment(key)),
                    harness,
                    sources,
                )
            });
            !fields.is_empty()
        } else {
            review::source(sources, path) <= review::source(sources, &format!("{harness}{path}"))
        }
    }
    if let Some(harnesses) = value.get_mut("harnesses").and_then(Value::as_object_mut) {
        harnesses.retain(|name, value| {
            prune(
                value,
                "",
                &format!("/harnesses/{}", review::pointer_segment(name)),
                sources,
            )
        });
    }
}

pub(super) async fn reload_roots(executor: &Arc<Executor>, settings: &ApprovedUser) {
    let executor = executor.clone();
    let settings = settings.clone();
    let jobs = tokio::task::spawn_blocking(move || {
        let mut jobs = Vec::new();
        for (root, ctx) in executor.actor_entries() {
            crate::config_live::reload_daemon_settings(&ctx, &settings);
            if !ctx.subc_unbound_quiesced()
                && crate::config_live::managed_index_update(&ctx, &settings).is_some()
            {
                let settings = settings.clone();
                let id = "subc-settings-indexes".to_string();
                jobs.push(executor.submit_maintenance_async(
                    root,
                    Lane::MaintenanceCommit,
                    id.clone(),
                    Box::new(move |ctx| {
                        crate::commands::configure::apply_managed_indexes(ctx, &settings);
                        Response::success(id, Value::Null)
                    }),
                ));
            }
        }
        jobs
    })
    .await
    .unwrap_or_default();
    for job in jobs {
        let _ = job.await;
    }
}

// The frame loop must remain available to resolve the daemon's response while
// bind admission is waiting on its project's first view.
pub(in crate::subc) type ProjectBind = Result<Arc<Project>, &'static str>;

#[derive(Default)]
pub(in crate::subc) struct BindWaits {
    jobs: JoinSet<(DecodedFrame, ProjectBind)>,
    routes: HashMap<RouteChannel, tokio::task::AbortHandle>,
}

impl BindWaits {
    pub(in crate::subc) fn waiting(&self) -> bool {
        !self.jobs.is_empty()
    }

    pub(in crate::subc) fn cancel(&mut self, route: RouteChannel) {
        if let Some(job) = self.routes.remove(&route) {
            job.abort();
        }
    }

    pub(in crate::subc) async fn next(&mut self) -> (DecodedFrame, ProjectBind) {
        loop {
            if let Some(outcome) = self.jobs.join_next_with_id().await {
                let id = match &outcome {
                    Ok((id, _)) => *id,
                    Err(error) => error.id(),
                };
                if let Some(route) = self
                    .routes
                    .iter()
                    .find_map(|(route, job)| (job.id() == id).then_some(*route))
                {
                    self.routes.remove(&route);
                }
                if let Ok((_, result)) = outcome {
                    return result;
                }
            }
            if self.jobs.is_empty() {
                std::future::pending::<()>().await;
            }
        }
    }

    pub(in crate::subc) async fn defer(
        &mut self,
        frame: DecodedFrame,
        session: &Session,
        control: &Arc<readiness::DaemonControl>,
        executor: &Arc<Executor>,
        writer: &WriterSender,
        metrics: &DispatchPathMetrics,
    ) -> Result<Option<DecodedFrame>, SubcError> {
        let incoming = &frame.frame;
        if incoming.header.ty != FrameType::Request || incoming.header.channel != 0 {
            return Ok(Some(frame));
        }
        let Ok((
            ModuleControlRequest::RouteBind {
                route_channel,
                epoch,
                scope: Some(scope),
                target: RouteTarget::ToolProvider { .. },
                ..
            },
            _,
        )) = remote_calls::decode_control(&incoming.body)
        else {
            return Ok(Some(frame));
        };
        let Some(id) = scope.project_id.as_deref() else {
            return Ok(Some(frame));
        };
        let route = route_key(route_channel, epoch);
        if self.routes.contains_key(&route) || self.jobs.len() >= WRITER_QUEUE_CAPACITY {
            send_route_bind_error(
                writer,
                incoming,
                "actor_not_ready",
                a0_egress::BIND_BUSY_MESSAGE,
                metrics,
            )
            .await?;
            return Ok(None);
        }
        let project = session.projects.slot(id, &session.user, &session.schema);
        let control = control.clone();
        let executor = executor.clone();
        let job = self.jobs.spawn(async move {
            let result = project
                .fetch(&control, &executor, true)
                .await
                .map(|()| project);
            (frame, result)
        });
        self.routes.insert(route, job);
        Ok(None)
    }
}

pub(super) async fn refresh_changed(
    project: Arc<Project>,
    control: &readiness::DaemonControl,
    executor: &Arc<Executor>,
) {
    if let Err(reason) = project.fetch(control, executor, false).await {
        log::warn!("project settings refresh refused: {reason}");
    }
}
