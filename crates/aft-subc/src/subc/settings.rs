//! Connection-local config.gate delivery. Only the legacy, non-negotiated
//! path may read the user settings file. In gated mode, its path is sent to
//! the daemon in `config.declare`; only `config.get` supplies user settings.

use super::{readiness, *};
use crate::{
    config_resolve::{ConfigTier, RawAftConfig},
    subc_config::ApprovedUser,
};
use subc_client_rs::config::*;

mod projects;
mod review;
mod startup;
#[cfg(test)]
mod tests;
pub(super) use projects::{BindWaits, Project, ProjectBind};
pub(super) use startup::{
    initialize_storage, initialize_task_cleanup, legacy_catalog, negotiate, wait_for_first_view,
    LegacySettings,
};

const CONFIG_TIMEOUT: Duration = Duration::from_secs(5);
const WAITING_HEALTH: &str =
    "AFT is not ready because its settings are unavailable from Subconscious.";
const PENDING_HEALTH: &str = "AFT is using its last approved settings while a settings update awaits approval in Subconscious.";

fn declaration(path: Option<&Path>) -> Option<(ConfigDeclareRequest, Schema)> {
    let schema = declaration_schema()?;
    let validated = Schema::new(schema.clone()).ok()?;
    Some((
        ConfigDeclareRequest::new(schema, path?.to_str()?, vec![]),
        validated,
    ))
}

fn declaration_schema() -> Option<Value> {
    #[cfg(all(feature = "test-hooks", debug_assertions))]
    if let Some(path) = std::env::var_os("AFT_TEST_SETTINGS_SCHEMA") {
        return serde_json::from_slice(&std::fs::read(path).ok()?).ok();
    }
    Some(crate::config_live::schema::settings_declaration())
}

async fn request<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    control: &readiness::DaemonControl,
    body: &T,
) -> Result<R, &'static str> {
    match tokio::time::timeout(CONFIG_TIMEOUT, control.config_request(body)).await {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(readiness::ControlError::Refused { code, .. })) if code == "config_gated" => {
            Err("no_approved_config")
        }
        Ok(Err(readiness::ControlError::Refused { code, .. })) if code == "unknown_project" => {
            Err("unknown_project")
        }
        Ok(Err(readiness::ControlError::Refused { code, .. })) => {
            log::warn!("daemon refused settings control request ({code})");
            Err("settings_request_failed")
        }
        _ => Err("settings_request_failed"),
    }
}

fn view_from_reply(reply: ConfigGetReply) -> Result<ConfigView, &'static str> {
    project_view(reply, None)
}

fn project_view(reply: ConfigGetReply, project: Option<&str>) -> Result<ConfigView, &'static str> {
    let catchup = reply.declaration_pending.is_some();
    let pending = reply.pending.is_some();
    if catchup && pending {
        return Err("invalid_settings_view");
    }
    let mut view = reply.pending.or(reply.active).ok_or("no_approved_config")?;
    if view.project_id.as_deref() != project
        || view
            .layers
            .iter()
            .any(|layer| layer.layer.known_layer().is_none())
        || (pending && (view.status != ConfigStatus::Pending || view.attempt.is_none()))
        || (!pending && !matches!(view.status, ConfigStatus::Unstamped | ConfigStatus::Active))
        || (catchup && view.status != ConfigStatus::Active)
    {
        return Err("invalid_settings_view");
    }
    view.declaration_pending = reply.declaration_pending;
    Ok(view)
}

fn validate_user(value: &Value) -> Result<ConfigTier, String> {
    use serde::de::IntoDeserializer;
    let input = value.clone().into_deserializer();
    let _: RawAftConfig = serde_path_to_error::deserialize(input).map_err(|error| {
        error
            .path()
            .iter()
            .map(|segment| {
                use serde_path_to_error::Segment;
                let name = match segment {
                    Segment::Map { key } => key.to_owned(),
                    Segment::Seq { index } => index.to_string(),
                    _ => return String::new(),
                };
                format!("/{}", name.replace('~', "~0").replace('/', "~1"))
            })
            .collect::<String>()
    })?;
    let tier = ConfigTier {
        tier: "user".into(),
        source: "Subconscious settings".into(),
        doc: value.to_string(),
    };
    if value.pointer("/bash/env").is_some()
        && crate::config_resolve::strict_tier_error(&tier, None).is_some()
    {
        return Err("/bash/env".into());
    }
    if let Some(shell) = value.get("shell") {
        let shell: Option<crate::config::ShellConfig> =
            serde_json::from_value(shell.clone()).map_err(|_| "/shell")?;
        if shell
            .and_then(|shell| shell.env_provider)
            .is_some_and(|provider| provider.validate().is_err())
        {
            return Err("/shell/env_provider".into());
        }
    }
    if value.pointer("/semantic/backend").and_then(Value::as_str) == Some("synapse")
        && value
            .pointer("/semantic/model")
            .and_then(Value::as_str)
            .is_none_or(|model| model.trim().is_empty())
    {
        return Err("/semantic/model".into());
    }
    Ok(tier)
}

fn resolve_user(view: &ConfigView) -> Result<(ConfigTier, crate::config::Config), &'static str> {
    let tier = validate_user(&view.effective).map_err(|_| "invalid_settings_view")?;
    let resolved = crate::config_resolve::resolve_config(std::slice::from_ref(&tier));
    if !resolved.errors.is_empty() || resolved.config.cache_config_unknown.is_some() {
        return Err("invalid_settings_view");
    }
    Ok((tier, resolved.config))
}

async fn report(
    control: &readiness::DaemonControl,
    view: &ConfigView,
    result: Result<(), &'static str>,
) -> Result<(), &'static str> {
    if let Some(attempt) = view
        .attempt
        .filter(|_| view.status == ConfigStatus::Pending || view.declaration_pending.is_some())
    {
        let (result, reasons) = match result {
            Ok(()) => (ConfigApplyResult::Ack, vec![]),
            Err(reason) => (ConfigApplyResult::Nack, vec![reason.into()]),
        };
        let mut input = ConfigAppliedRequest::new(view.generation, attempt, result, reasons);
        input.project_id.clone_from(&view.project_id);
        let _: ConfigAppliedReply = request(control, &input).await?;
    }
    Ok(())
}

pub(super) struct Session {
    schema: Arc<Schema>,
    user: ApprovedUser,
    view: tokio::sync::Mutex<ConfigView>,
    hint: Notify,
    user_changed: AtomicBool,
    project_hints: parking_lot::Mutex<HashSet<String>>,
    projects: projects::Projects,
    pending: AtomicBool,
    ready: AtomicBool,
}

impl Session {
    fn new(schema: Arc<Schema>, user: ConfigTier, view: ConfigView) -> Self {
        Self {
            schema,
            user: ApprovedUser::new(user),
            pending: AtomicBool::new(view.declaration_pending.is_some()),
            view: tokio::sync::Mutex::new(view),
            hint: Notify::new(),
            user_changed: AtomicBool::new(false),
            project_hints: Default::default(),
            projects: Default::default(),
            ready: AtomicBool::new(false),
        }
    }

    pub(super) fn user(&self) -> &ApprovedUser {
        &self.user
    }
    pub(super) fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    pub(super) fn mark_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }
    pub(super) fn pending(&self) -> bool {
        self.pending.load(Ordering::Acquire)
    }

    fn notify_user_changed(&self) {
        self.user_changed.store(true, Ordering::Release);
        self.hint.notify_one();
    }

    pub(super) async fn run(
        &self,
        control: Arc<readiness::DaemonControl>,
        executor: Arc<Executor>,
    ) {
        loop {
            self.hint.notified().await;
            if self.user_changed.swap(false, Ordering::AcqRel) {
                if let Err(reason) = self.refresh(&control, &executor).await {
                    log::warn!("settings refresh refused: {reason}");
                }
            }
            let projects = std::mem::take(&mut *self.project_hints.lock());
            for id in projects {
                if let Some(project) = self.projects.existing(&id) {
                    projects::refresh_changed(project, &control, &executor).await;
                }
            }
        }
    }

    async fn refresh(
        &self,
        control: &readiness::DaemonControl,
        executor: &Arc<Executor>,
    ) -> Result<(), &'static str> {
        let next = view_from_reply(request(control, &ConfigGetRequest::new()).await?)?;
        let mut current = self.view.lock().await;
        // On unstamped installs, effective values can change at generation zero.
        if next.generation < current.generation {
            return Ok(());
        }
        let user = self.user.clone();
        let candidate = next.clone();
        let result = tokio::task::spawn_blocking(move || {
            let (tier, config) = resolve_user(&candidate)?;
            let old = crate::config_resolve::resolve_config(&[user.snapshot()]).config;
            if crate::config_live::apply_live_config(&old, &config, &old)
                .deferred
                .into_iter()
                .any(crate::config_live::schema::settings_restart_required)
            {
                return Err("settings_require_restart");
            }
            aft_store::sync::set_macos_full_sync(config.durability.macos_full_sync);
            crate::memory::profile::publish(config.memory);
            user.replace(tier);
            Ok(())
        })
        .await
        .unwrap_or(Err("settings_apply_failed"));
        if result.is_ok() {
            projects::reload_roots(executor, &self.user).await;
            self.pending
                .store(next.declaration_pending.is_some(), Ordering::Release);
            *current = next.clone();
        }
        report(control, &next, result).await?;
        result
    }

    pub(super) async fn intercept(
        &self,
        frame: &Frame,
        writer: &WriterSender,
        metrics: &DispatchPathMetrics,
    ) -> Result<bool, SubcError> {
        if let Some(change) = changed(frame) {
            if let Some(project) = change.project_id {
                if self.projects.existing(&project).is_some() {
                    self.project_hints.lock().insert(project);
                    self.hint.notify_one();
                }
            } else {
                self.notify_user_changed();
            }
            return Ok(true);
        }
        if let Some(frame) = review::response(&self.schema, frame).await? {
            send_frame(writer, metrics, frame).await?;
            return Ok(true);
        }
        if !self.ready() && frame.header.ty == FrameType::Request {
            if let Some(reply) =
                startup::pre_ready_reply(frame, None, "settings_catalog_starting").await?
            {
                send_frame(writer, metrics, reply).await?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

pub(super) fn note_health(
    report: &mut subc_protocol::session::HealthReport,
    session: Option<&Session>,
) {
    if session.is_some_and(Session::pending) {
        report.detail = Some(PENDING_HEALTH.into());
        if let Some(metrics) = report.metrics.as_mut().and_then(Value::as_object_mut) {
            metrics.insert("settings".into(), json!({"declaration_pending": true}));
        }
    }
}

fn is_changed(frame: &Frame) -> bool {
    changed(frame).is_some_and(|hint| hint.project_id.is_none())
}

fn changed(frame: &Frame) -> Option<ConfigChanged> {
    if frame.header.ty != FrameType::Push
        || frame.header.channel != 0
        || frame.header.epoch != 0
        || frame.header.corr != 0
    {
        return None;
    }
    serde_json::from_slice(&frame.body).ok()
}
