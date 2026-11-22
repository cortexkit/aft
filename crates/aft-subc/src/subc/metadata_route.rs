use super::*;

pub(super) fn log_management_refusal(
    identity: &ManagementIdentity,
    frame: &Frame,
    error: &ErrorBody,
) {
    let body = serde_json::from_slice::<Value>(&frame.body).ok();
    let operation = body.as_ref().and_then(|body| {
        body.get("method")
            .or_else(|| body.get("op"))
            .or_else(|| body.get("name"))
            .and_then(Value::as_str)
    });
    let admission = match &identity.admission {
        ManagementAdmission::FirstParty => "FirstParty",
        ManagementAdmission::FlowOnly => "FlowOnly",
        ManagementAdmission::PermissionsOnly => "PermissionsOnly",
        ManagementAdmission::ShellsOnly => "ShellsOnly",
        ManagementAdmission::DisplayOnly => "DisplayOnly",
        ManagementAdmission::Occipital => "Occipital",
        ManagementAdmission::RemoteTools(_) => "RemoteTools",
        ManagementAdmission::ProjectFree { .. } => "ProjectFree",
    };
    log::warn!(
        "subc management request refused: operation={operation:?} admission={admission} channel={} epoch={} code={}",
        frame.header.channel,
        frame.header.epoch,
        error.code,
    );
}

/// Metadata needs no project. Only rootless provider routes also retain the
/// custodian feed; Occipital's explicit management route has no custody access.
pub(super) async fn handle_metadata_route_request(
    tx: &WriterSender,
    frame: &Frame,
    metrics: &Arc<DispatchPathMetrics>,
    identity: &ManagementIdentity,
    shared_app: &Arc<App>,
    storage_dir: &Path,
) -> Result<(), SubcError> {
    let mut body = serde_json::from_slice::<Value>(&frame.body).unwrap_or(Value::Null);
    let operation = body
        .get("op")
        .or_else(|| body.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let arguments = if body.get("op").is_some() {
        body.as_object_mut().unwrap().remove("op");
        body
    } else {
        body.get("arguments").cloned().unwrap_or_else(|| json!({}))
    };
    if let Some(op @ ("late_results" | "late_results.ack")) = operation.as_deref() {
        if let ManagementAdmission::ProjectFree {
            model_only,
            held_updates,
            ..
        } = &identity.admission
        {
            tool_provider::project_free_custody(
                tx,
                frame,
                metrics,
                shared_app.clone(),
                storage_dir.join("aft.db"),
                op,
                arguments,
                principal_label(&identity.principal),
                *model_only,
                *held_updates,
            );
            return Ok(());
        }
    }
    let result = match operation.as_deref() {
        Some("role.describe") => Ok(tool_provider::describe()),
        Some("tool.views") => tool_views::answer(arguments),
        Some("tool.catalog") => {
            tool_provider::validate_catalog_request(arguments).and_then(|(request, preset)| {
                tool_provider::catalog_for_request(
                    request,
                    preset,
                    &[],
                    false,
                    false,
                    false,
                    crate::config::DEFAULT_BASH_WORKER_WAIT_MAX_MS,
                )
            })
        }
        _ => match &identity.admission {
            ManagementAdmission::ProjectFree {
                workdir,
                legacy_root,
                ..
            } => Err(project_folder_refusal(
                PROJECT_REQUIRED_CODE,
                workdir.as_deref(),
                legacy_root.as_deref(),
                route_key(frame.header.channel, frame.header.epoch),
                frame.header.corr,
            )),
            _ => Err(ErrorBody::new(
                "route_not_admitted",
                "not a metadata operation",
            )),
        },
    };
    match result {
        Err(error) => {
            log_management_refusal(identity, frame, &error);
            send_provider_error(tx, metrics, frame, error).await
        }
        Ok(answer) => {
            let response = Frame::build_with_version(
                frame.header.ver,
                FrameType::Response,
                frame.header.flags,
                frame.header.channel,
                frame.header.epoch,
                frame.header.corr,
                serde_json::to_vec(&answer).map_err(SubcError::Json)?,
            )
            .map_err(SubcError::FrameBuild)?;
            send_reliable_writer_frame(tx, metrics, response, "provider metadata").await
        }
    }
}
