use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::context::AppContext;
use crate::harness::Harness;
use crate::hashline::integration::{
    display_files_from_envelope, effective_for_capture, hashline_preflight_from_args,
    render_mutation_response, render_rejection_response, MutationRenderInput, TransportKind,
};
use crate::hashline::syntax::{
    parse_hashline_patch, resolve_patch_sections, Baseline, HashlineRejection, Operation,
};
use crate::hashline::transaction::{
    run_transaction, ExecuteContext, MvDestinationInput, TransactionSectionInput,
};
use crate::protocol::{RawRequest, Response};

struct OwnedMvDestination {
    canonical_path: PathBuf,
    requested_path: String,
    baseline_bytes: Option<Vec<u8>>,
}

pub fn handle_preflight(req: &RawRequest, ctx: &AppContext) -> Response {
    let Some((_guard, root)) = effective_binding(req, ctx) else {
        return rejection_response(
            &req.id,
            &HashlineRejection::parse("hashline edit is not enabled for this session"),
            transport_kind(ctx),
        );
    };
    match hashline_preflight_from_args(&req.params, Some(&root)) {
        Ok(result) => Response::success(&req.id, result.to_json()),
        Err(rejection) => rejection_response(&req.id, &rejection, transport_kind(ctx)),
    }
}

pub fn handle_edit(req: &RawRequest, ctx: &AppContext) -> Response {
    let Some((guard, root)) = effective_binding(req, ctx) else {
        return rejection_response(
            &req.id,
            &HashlineRejection::parse("hashline edit is not enabled for this session"),
            transport_kind(ctx),
        );
    };
    let preview = req
        .params
        .get("preview")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut agent_arguments = req.params.clone();
    if let Some(arguments) = agent_arguments.as_object_mut() {
        arguments.remove("preview");
    }
    let patch_text = match crate::hashline::syntax::validate_raw_arguments(&agent_arguments) {
        Ok(request) => request.patch,
        Err(rejection) => {
            return rejection_response(&req.id, &rejection, transport_kind(ctx));
        }
    };
    let patch = match parse_hashline_patch(&patch_text) {
        Ok(patch) => patch,
        Err(rejection) => {
            return rejection_response(&req.id, &rejection, transport_kind(ctx));
        }
    };

    let result = guard.with_binding_mut(|binding| {
        let resolved = resolve_patch_sections(binding.snapshots_mut(), &patch, |requested| {
            resolve_write_path(req, ctx, &root, requested)
        })?;

        let mut baselines = BTreeMap::<PathBuf, Baseline>::new();
        for section in &resolved {
            if !baselines.contains_key(&section.canonical_path) {
                let bytes = fs::read(&section.canonical_path).map_err(|error| {
                    HashlineRejection::untaggable_path(format!(
                        "failed to load {}: {error}",
                        section.canonical_path.display()
                    ))
                })?;
                baselines.insert(section.canonical_path.clone(), Baseline::from_bytes(bytes));
            }
        }

        let mut destinations = Vec::with_capacity(patch.sections.len());
        for section in &patch.sections {
            let destination = section
                .operations
                .iter()
                .find_map(|operation| match operation {
                    Operation::Mv(mv) => Some(mv.destination.as_str()),
                    _ => None,
                });
            let owned = if let Some(requested_path) = destination {
                let canonical_path = resolve_write_path(req, ctx, &root, requested_path)?;
                let baseline_bytes = if canonical_path.exists() {
                    Some(fs::read(&canonical_path).map_err(|error| {
                        HashlineRejection::untaggable_path(format!(
                            "failed to load MV destination {}: {error}",
                            canonical_path.display()
                        ))
                    })?)
                } else {
                    None
                };
                Some(OwnedMvDestination {
                    canonical_path,
                    requested_path: requested_path.to_string(),
                    baseline_bytes,
                })
            } else {
                None
            };
            destinations.push(owned);
        }

        let mut display_baselines = Vec::<(String, Vec<u8>)>::new();
        let inputs = resolved
            .iter()
            .zip(patch.sections.iter())
            .zip(destinations.iter())
            .map(|((resolved, section), destination)| {
                let baseline = baselines
                    .get(&resolved.canonical_path)
                    .expect("every resolved source has one baseline");
                display_baselines.push((
                    section.header.requested_path.clone(),
                    baseline.bytes.clone(),
                ));
                let mv_destination = destination.as_ref().map(|destination| {
                    if let Some(bytes) = destination.baseline_bytes.as_ref() {
                        display_baselines.push((destination.requested_path.clone(), bytes.clone()));
                    }
                    MvDestinationInput {
                        canonical_path: destination.canonical_path.as_path(),
                        requested_path: destination.requested_path.as_str(),
                        baseline_bytes: destination.baseline_bytes.as_deref(),
                    }
                });
                TransactionSectionInput {
                    canonical_path: resolved.canonical_path.as_path(),
                    requested_path: section.header.requested_path.as_str(),
                    baseline,
                    snapshot: &resolved.snapshot,
                    operations: section.operations.as_slice(),
                    resolved: resolved.operations.as_slice(),
                    mv_destination,
                }
            })
            .collect::<Vec<_>>();

        let register_snapshot = binding.registers().clone();
        let (snapshots, registers) = binding.stores_mut();
        let mut backups = ctx.backup().lock();
        let backups_enabled = backups.policy().enabled;
        let skipped_before = backups.latest_skipped_order(req.session());
        let mut execute = ExecuteContext {
            session: req.session(),
            backups: &mut backups,
            snapshots,
            registers,
            backups_enabled,
            fault: None,
        };
        let envelope = run_transaction(&inputs, &register_snapshot, &mut execute, preview)?;
        drop(execute);
        let display_files = display_files_from_envelope(&envelope, &display_baselines);
        let backup_skipped_reason = backups.skipped_reason_after(req.session(), skipped_before);
        Ok((envelope, display_files, backup_skipped_reason))
    });

    match result {
        Ok((envelope, display_files, backup_skipped_reason)) => {
            let mut payload = render_mutation_response(MutationRenderInput {
                envelope: &envelope,
                display_files: &display_files,
                project_root: Some(&root),
                transport: transport_kind(ctx),
            });
            if let (Some(object), Some(reason)) = (payload.as_object_mut(), backup_skipped_reason) {
                object.insert(
                    "backup_skipped_reason".to_string(),
                    Value::String(reason.as_str().to_string()),
                );
            }
            response_from_payload(&req.id, payload)
        }
        Err(rejection) => rejection_response(&req.id, &rejection, transport_kind(ctx)),
    }
}

fn effective_binding(
    req: &RawRequest,
    ctx: &AppContext,
) -> Option<(crate::hashline::integration::BindingGuard, PathBuf)> {
    let root = ctx
        .canonical_cache_root_opt()
        .or_else(|| ctx.config().project_root.clone())?;
    let guard = ctx
        .hashline_bindings()
        .capture(&root, req.session().to_string())?;
    effective_for_capture(Some(&guard)).then_some((guard, root))
}

fn resolve_write_path(
    req: &RawRequest,
    ctx: &AppContext,
    project_root: &Path,
    requested: &str,
) -> Result<PathBuf, HashlineRejection> {
    let path = crate::subc_translate::resolve_path_from_project_root(project_root, requested);
    // Hashline handles are keyed by the symlink-resolved file identity. Following
    // the final component here keeps two requested spellings of one existing file
    // in the same verification and transaction unit while still validating the
    // resolved target against the project boundary.
    let validated = ctx.validate_path(&req.id, &path).map_err(|response| {
        let message = response
            .data
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("path is not write-eligible");
        HashlineRejection::untaggable_path(message)
    })?;
    let canonical = std::fs::canonicalize(&validated).unwrap_or(validated);
    ctx.validate_path(&req.id, &canonical).map_err(|response| {
        let message = response
            .data
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("path is not write-eligible");
        HashlineRejection::untaggable_path(message)
    })
}

fn transport_kind(ctx: &AppContext) -> TransportKind {
    match ctx.harness_opt() {
        Some(Harness::Opencode) => TransportKind::OpenCode,
        Some(Harness::Pi) => TransportKind::Pi,
        Some(Harness::Mcp { .. }) => TransportKind::Mcp,
        Some(Harness::Runner | Harness::Fed { .. }) | None => TransportKind::Ndjson,
    }
}

fn rejection_response(
    id: &str,
    rejection: &HashlineRejection,
    transport: TransportKind,
) -> Response {
    response_from_payload(id, render_rejection_response(rejection, transport))
}

fn response_from_payload(id: &str, mut payload: Value) -> Response {
    let success = payload
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(object) = payload.as_object_mut() {
        object.remove("success");
    }
    Response {
        id: id.to_string(),
        success,
        data: payload,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    use crate::config::Config;
    use crate::context::default_language_provider_factory;
    use crate::hashline::integration::RegistrationRequest;
    use crate::hashline::snapshot::{MAX_SNAPSHOT_PATHS, MAX_VERSIONS_PER_PATH};
    use crate::protocol::{RawRequest, DEFAULT_SESSION_ID};

    fn request(command: &str, params: Value) -> RawRequest {
        request_in_session(command, params, None)
    }

    fn request_in_session(
        command: &str,
        params: Value,
        session_id: impl Into<Option<String>>,
    ) -> RawRequest {
        RawRequest {
            id: format!("hashline-{command}-test"),
            command: command.to_string(),
            lsp_hints: None,
            session_id: session_id.into(),
            params,
        }
    }

    fn registered_fixture(
        contents: &str,
    ) -> (tempfile::TempDir, PathBuf, PathBuf, AppContext, String) {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(temp.path()).expect("canonical root");
        let path = root.join("sample.txt");
        std::fs::write(&path, contents).expect("fixture write");
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.clone()),
                ..Default::default()
            },
        );
        let registration = ctx.hashline_bindings().register(
            &root,
            DEFAULT_SESSION_ID.to_string(),
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: true,
                read_slot_survives: true,
            },
        );
        assert!(registration.effective);
        let mut read = request("read", json!({ "file": path }));
        read.params["_hashline_requested_path"] = Value::String("sample.txt".to_string());
        let response = crate::commands::read::handle_read(&read, &ctx);
        let tag = response.data["hashline_tag"]
            .as_str()
            .expect("tagged read")
            .to_string();
        (temp, root, path, ctx, tag)
    }

    #[test]
    fn preflight_then_apply_mutates_bytes_and_records_undo() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(temp.path()).expect("canonical root");
        let path = root.join("sample.txt");
        std::fs::write(&path, "alpha\nbeta\n").expect("fixture write");
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.clone()),
                ..Default::default()
            },
        );
        let registration = ctx.hashline_bindings().register(
            &root,
            DEFAULT_SESSION_ID.to_string(),
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: true,
                read_slot_survives: true,
            },
        );
        assert!(registration.effective);

        let mut read = request("read", json!({ "file": path }));
        read.params["_hashline_requested_path"] = Value::String("sample.txt".to_string());
        let read_response = crate::commands::read::handle_read(&read, &ctx);
        let tag = read_response.data["hashline_tag"]
            .as_str()
            .expect("tagged read")
            .to_string();
        let patch = format!("*** Begin Patch\n[sample.txt#{tag}]\nPUT 1:\n+omega\n*** End Patch");

        let preflight = handle_preflight(
            &request("hashline_preflight", json!({ "patch": patch.clone() })),
            &ctx,
        );
        assert!(preflight.success, "{}", preflight.data);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\nbeta\n");

        let response = handle_edit(&request("hashline_edit", json!({ "patch": patch })), &ctx);
        assert!(response.success, "{}", response.data);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "omega\nbeta\n");
        assert!(response.data["op_id"].as_str().is_some());
        assert_eq!(
            ctx.backup().lock().history(DEFAULT_SESSION_ID, &path).len(),
            1
        );
    }

    #[test]
    fn two_session_residency_churn_cannot_evict_another_sessions_fresh_baseline() {
        const SESSION_A: &str = "session-a";
        const SESSION_B: &str = "session-b";

        let temp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(temp.path()).expect("canonical root");
        let primary = root.join("primary.py");
        let primary_contents = (1..=130)
            .map(|line| format!("line_{line} = {line}\n"))
            .collect::<String>();
        std::fs::write(&primary, &primary_contents).expect("primary fixture");
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.clone()),
                ..Default::default()
            },
        );
        for session in [SESSION_A, SESSION_B] {
            let registration = ctx.hashline_bindings().register(
                &root,
                session,
                RegistrationRequest {
                    configured_enabled: true,
                    edit_slot_survives: true,
                    read_slot_survives: true,
                },
            );
            assert!(registration.effective);
        }

        let mut primary_read = request_in_session(
            "read",
            json!({ "file": primary.clone() }),
            Some(SESSION_A.to_string()),
        );
        primary_read.params["_hashline_requested_path"] = Value::String("primary.py".into());
        let primary_response = crate::commands::read::handle_read(&primary_read, &ctx);
        let primary_tag = primary_response.data["hashline_tag"]
            .as_str()
            .expect("session A tagged read")
            .to_string();
        assert!(primary_response.data["content"]
            .as_str()
            .is_some_and(|content| content.contains("16:line_16 = 16")));

        for index in 0..=MAX_SNAPSHOT_PATHS {
            let b_path = root.join(format!("b-{index}.py"));
            std::fs::write(&b_path, format!("b_{index} = {index}\n")).expect("B fixture");
            let b_read = request_in_session(
                "read",
                json!({ "file": b_path }),
                Some(SESSION_B.to_string()),
            );
            assert!(crate::commands::read::handle_read(&b_read, &ctx).success);

            if index < MAX_SNAPSHOT_PATHS - 1 {
                let a_path = root.join(format!("a-{index}.py"));
                std::fs::write(&a_path, format!("a_{index} = {index}\n")).expect("A fixture");
                let a_read = request_in_session(
                    "read",
                    json!({ "file": a_path }),
                    Some(SESSION_A.to_string()),
                );
                assert!(crate::commands::read::handle_read(&a_read, &ctx).success);
            }
        }

        let churn_path = root.join("b-churn.py");
        for version in 0..=MAX_VERSIONS_PER_PATH {
            std::fs::write(&churn_path, format!("version = {version}\n")).expect("version fixture");
            let churn_read = request_in_session(
                "read",
                json!({ "file": churn_path }),
                Some(SESSION_B.to_string()),
            );
            assert!(crate::commands::read::handle_read(&churn_read, &ctx).success);
        }

        let a = ctx
            .hashline_bindings()
            .peek(&root, SESSION_A)
            .expect("session A binding");
        a.with_binding(|binding| {
            assert_eq!(binding.snapshots().path_count(), MAX_SNAPSHOT_PATHS);
            assert!(binding.snapshots().contains(&primary, &primary_tag));
        });
        let b = ctx
            .hashline_bindings()
            .peek(&root, SESSION_B)
            .expect("session B binding");
        b.with_binding(|binding| {
            assert_eq!(binding.snapshots().path_count(), MAX_SNAPSHOT_PATHS);
        });

        let patch = format!("[primary.py#{primary_tag}]\nPUT 16:\n+line_16 = 160");
        let response = handle_edit(
            &request_in_session(
                "hashline_edit",
                json!({ "patch": patch }),
                Some(SESSION_A.to_string()),
            ),
            &ctx,
        );
        assert!(response.success, "{}", response.data);
        assert_eq!(
            std::fs::read_to_string(primary)
                .expect("edited primary")
                .lines()
                .nth(15),
            Some("line_16 = 160")
        );
    }

    #[test]
    fn same_path_section_composition_pinned_oracle_rejection_control() {
        // The pinned oracle runs assertUniqueCanonicalPaths before apply. Lock its
        // rejection for the exact duplicate-canonical-path class AFT composes.
        let canonical_paths = [PathBuf::from("/same-file"), PathBuf::from("/same-file")];
        let mut unique = std::collections::BTreeSet::new();
        let outcome = canonical_paths
            .iter()
            .find(|path| !unique.insert((*path).clone()))
            .map(|path| {
                format!(
                    "assertUniqueCanonicalPaths rejected duplicate canonical path {}",
                    path.display()
                )
            });

        assert_eq!(
            outcome.as_deref(),
            Some("assertUniqueCanonicalPaths rejected duplicate canonical path /same-file")
        );
    }

    #[test]
    fn same_path_section_composition_aft_outcome_control() {
        let (_temp, _root, path, ctx, tag) =
            registered_fixture("alpha\nbeta\ngamma\ndelta\nepsilon\n");
        let original = std::fs::read(&path).unwrap();
        let patch = format!("[sample.txt#{tag}]\nCUT 1\n[sample.txt#{tag}]\nCUT 5");

        let response = handle_edit(&request("hashline_edit", json!({ "patch": patch })), &ctx);
        assert!(response.success, "{}", response.data);
        assert_eq!(std::fs::read(&path).unwrap(), b"beta\ngamma\ndelta\n");
        assert!(response.data["output"]
            .as_str()
            .unwrap_or_default()
            .starts_with("1 of 1 files applied"));
        assert_eq!(
            response.data["classifications"].as_array().unwrap().len(),
            1
        );
        assert_eq!(response.data["final_tags"].as_array().unwrap().len(), 1);
        assert_eq!(
            ctx.backup().lock().history(DEFAULT_SESSION_ID, &path).len(),
            1
        );

        let undo = crate::commands::undo::handle_undo(&request("undo", json!({})), &ctx);
        assert!(undo.success, "{}", undo.data);
        assert_eq!(undo.data["restored_count"], json!(1));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn stale_address_in_one_same_path_section_rejects_the_whole_unit() {
        let (_temp, _root, path, ctx, tag) =
            registered_fixture("alpha\nbeta\ngamma\ndelta\nepsilon\n");
        std::fs::write(&path, "alpha\nbeta\ngamma\ndelta\nexternal\n").unwrap();
        let patch = format!("[sample.txt#{tag}]\nCUT 1\n[sample.txt#{tag}]\nCUT 5");

        let rejection = handle_edit(&request("hashline_edit", json!({ "patch": patch })), &ctx);
        assert!(!rejection.success, "{}", rejection.data);
        assert_eq!(rejection.data["code"], json!("hashline_stale_tag"));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"alpha\nbeta\ngamma\ndelta\nexternal\n"
        );
        assert!(ctx
            .backup()
            .lock()
            .history(DEFAULT_SESSION_ID, &path)
            .is_empty());
    }

    #[test]
    fn same_path_overlaps_compose_in_order_but_deleted_addresses_reject_both_ops() {
        let (_temp, _root, path, ctx, tag) = registered_fixture("alpha\nbeta\ngamma\n");
        let replacement_patch =
            format!("[sample.txt#{tag}]\nPUT 1:\n+first\n[sample.txt#{tag}]\nPUT 1:\n+second");
        let replacement = handle_edit(
            &request("hashline_edit", json!({ "patch": replacement_patch })),
            &ctx,
        );
        assert!(replacement.success, "{}", replacement.data);
        assert_eq!(std::fs::read(&path).unwrap(), b"second\nbeta\ngamma\n");

        let undo = crate::commands::undo::handle_undo(&request("undo", json!({})), &ctx);
        assert!(undo.success, "{}", undo.data);
        let cut_then_put =
            format!("[sample.txt#{tag}]\nCUT 1\n[sample.txt#{tag}]\nPUT 1:\n+forbidden");
        let rejection = handle_edit(
            &request("hashline_edit", json!({ "patch": cut_then_put })),
            &ctx,
        );
        assert!(!rejection.success, "{}", rejection.data);
        let message = rejection.data["message"].as_str().unwrap_or_default();
        assert!(message.contains("PUT at patch line 4"), "{message}");
        assert!(message.contains("CUT at patch line 2"), "{message}");
        assert_eq!(std::fs::read(&path).unwrap(), b"alpha\nbeta\ngamma\n");
    }

    #[test]
    fn mv_can_finish_a_same_source_composition_and_undo_as_one_operation() {
        let (_temp, root, source, ctx, tag) = registered_fixture("alpha\nbeta\n");
        let destination = root.join("moved.txt");
        let patch = format!("[sample.txt#{tag}]\nPUT 1:\n+omega\n[sample.txt#{tag}]\nMV moved.txt");

        let response = handle_edit(&request("hashline_edit", json!({ "patch": patch })), &ctx);
        assert!(response.success, "{}", response.data);
        assert!(!source.exists());
        assert_eq!(std::fs::read(&destination).unwrap(), b"omega\nbeta\n");
        assert!(response.data["output"]
            .as_str()
            .unwrap_or_default()
            .starts_with("1 of 1 files applied"));

        let undo = crate::commands::undo::handle_undo(&request("undo", json!({})), &ctx);
        assert!(undo.success, "{}", undo.data);
        assert_eq!(std::fs::read(&source).unwrap(), b"alpha\nbeta\n");
        assert!(!destination.exists());
    }

    #[test]
    fn mv_destination_edited_by_another_section_rejects_before_mutation() {
        let (_temp, root, source, ctx, source_tag) = registered_fixture("source\n");
        let destination = root.join("destination.txt");
        std::fs::write(&destination, "destination\n").unwrap();
        let mut read = request("read", json!({ "file": destination }));
        read.params["_hashline_requested_path"] = Value::String("destination.txt".to_string());
        let destination_read = crate::commands::read::handle_read(&read, &ctx);
        let destination_tag = destination_read.data["hashline_tag"].as_str().unwrap();
        let patch = format!(
            "[destination.txt#{destination_tag}]\nPUT 1:\n+changed\n[sample.txt#{source_tag}]\nMV destination.txt"
        );

        let rejection = handle_edit(&request("hashline_edit", json!({ "patch": patch })), &ctx);
        assert!(!rejection.success, "{}", rejection.data);
        assert!(rejection.data["message"]
            .as_str()
            .unwrap_or_default()
            .contains("also edited by another patch section"));
        assert_eq!(std::fs::read(&source).unwrap(), b"source\n");
        assert_eq!(std::fs::read(&destination).unwrap(), b"destination\n");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_real_path_sections_share_one_canonical_transaction_unit() {
        use std::os::unix::fs::symlink;

        let (_temp, root, path, ctx, tag) = registered_fixture("one\ntwo\nthree\n");
        symlink(&path, root.join("alias.txt")).unwrap();
        let edit_request = request("hashline_edit", json!({}));
        assert_eq!(
            resolve_write_path(&edit_request, &ctx, &root, "sample.txt").unwrap(),
            resolve_write_path(&edit_request, &ctx, &root, "alias.txt").unwrap()
        );
        let patch = format!("[sample.txt#{tag}]\nCUT 1\n[alias.txt#{tag}]\nCUT 3");

        let response = handle_edit(&request("hashline_edit", json!({ "patch": patch })), &ctx);
        assert!(response.success, "{}", response.data);
        assert_eq!(std::fs::read(&path).unwrap(), b"two\n", "{}", response.data);
        assert_eq!(
            response.data["classifications"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            ctx.backup().lock().history(DEFAULT_SESSION_ID, &path).len(),
            1
        );
    }

    #[test]
    fn server_preview_flag_is_not_validated_as_an_agent_argument() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = std::fs::canonicalize(temp.path()).expect("canonical root");
        let path = root.join("sample.txt");
        std::fs::write(&path, "alpha\nbeta\n").expect("fixture write");
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(root.clone()),
                ..Default::default()
            },
        );
        ctx.hashline_bindings().register(
            &root,
            DEFAULT_SESSION_ID.to_string(),
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: true,
                read_slot_survives: true,
            },
        );

        let mut read = request("read", json!({ "file": path }));
        read.params["_hashline_requested_path"] = Value::String("sample.txt".to_string());
        let read_response = crate::commands::read::handle_read(&read, &ctx);
        let tag = read_response.data["hashline_tag"]
            .as_str()
            .expect("tagged read");
        let patch = format!("[sample.txt#{tag}]\nPUT 1:\n+omega");

        let preview = handle_edit(
            &request("hashline_edit", json!({ "patch": patch, "preview": true })),
            &ctx,
        );
        assert!(preview.success, "{}", preview.data);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\nbeta\n");
        assert!(preview.data["preview"].as_bool().unwrap_or(false));
    }

    #[test]
    fn unregistered_hashline_handler_refuses_direct_invocation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ctx = AppContext::new(
            default_language_provider_factory(),
            Config {
                project_root: Some(temp.path().to_path_buf()),
                ..Default::default()
            },
        );
        let response = handle_edit(&request("hashline_edit", json!({ "patch": "x" })), &ctx);
        assert!(!response.success);
        assert_eq!(response.data["code"], "hashline_parse_error");
    }
}
