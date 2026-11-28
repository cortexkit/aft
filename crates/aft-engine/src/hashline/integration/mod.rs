//! Session registration, schema selection, preflight, and host transport
//! integration for the hashline edit surface.
//!
//! This module is the exclusive owner of governed dual-mode edit schema
//! artifacts and the transport-neutral response adapters shared by NDJSON,
//! subc, MCP, OpenCode, and Pi. Engine modules stay transport-agnostic; hosts
//! capture a session binding and call into these adapters.

mod binding;
mod preflight;
mod schema;
mod transport;

#[cfg(any(test, feature = "test-support"))]
pub(crate) use transport::build_display_metadata;

pub use binding::{
    effective_for_capture, BindingGuard, BindingHandle, BindingRegistry, DowngradeWarning,
    HashlineBinding, RegistrationOutcome, RegistrationRequest, SessionKey,
};
pub use preflight::{
    hashline_preflight, hashline_preflight_from_args, permission_metadata, PermissionPhase,
    PreflightFileSummary, PreflightOperation, PreflightResult, PERMISSION_ORCHESTRATION_ORDER,
};
pub use schema::{
    edit_description_for, edit_schema_for, governed_edit_manifest_entry, hashline_edit_schema,
    legacy_edit_schema, regenerate_governed_edit_artifacts, select_edit_schema,
    select_edit_schema_for_capture, translate_edit_for_session, translate_gate_on_edit,
    EditSchemaArm, GateOnTranslation, GovernedEditManifestEntry, HASHLINE_EDIT_COMMAND,
    HASHLINE_EDIT_DESCRIPTION, HASHLINE_PREFLIGHT_COMMAND, LEGACY_EDIT_DESCRIPTION,
};
pub use transport::{
    all_failed_payload_preserved, carrier_keys, display_files_from_envelope,
    rejection_for_contract, rejection_transport_registry, rejection_transport_status,
    render_agent_output, render_mutation_response, render_rejection_response,
    required_rejection_fields, shipped_envelope_is_renderable, strip_hashline_fields,
    synthetic_applied_file, synthetic_envelope, synthetic_failed_file,
    transports_preserve_carriers, DisplayFileBytes, MutationRenderInput,
    RejectionTransportContract, TransportKind,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::apply::FileClassification;
    use crate::hashline::scan::scan_bytes;
    use crate::hashline::syntax::{HashlineRejection, HashlineRejectionCode, RejectionStage};
    use crate::hashline::transaction::{FileOutcome, FileRole};
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::Duration;

    fn root() -> PathBuf {
        PathBuf::from("/tmp/hashline-integration-root")
    }

    fn on_request() -> RegistrationRequest {
        RegistrationRequest {
            configured_enabled: true,
            edit_slot_survives: true,
            read_slot_survives: true,
        }
    }

    fn off_request() -> RegistrationRequest {
        RegistrationRequest {
            configured_enabled: false,
            edit_slot_survives: true,
            read_slot_survives: true,
        }
    }

    // ── A7 registration matrix ──────────────────────────────────────────────

    #[test]
    fn a7_gate_off_preserves_legacy_schema_arm() {
        let registry = BindingRegistry::new();
        let outcome = registry.register(root(), "s-off", off_request());
        assert!(!outcome.effective);
        assert!(outcome.downgrade.is_none());

        let guard = registry.capture(root(), "s-off").expect("bound");
        let arm = select_edit_schema_for_capture(Some(&guard));
        assert_eq!(arm, EditSchemaArm::Legacy);

        let schema = edit_schema_for(arm);
        assert_eq!(schema["required"], json!(["filePath"]));
        assert!(schema["properties"].get("patch").is_none());
        assert!(schema["properties"].get("edits").is_some());

        // Unregistered sessions also expose legacy only.
        assert_eq!(select_edit_schema_for_capture(None), EditSchemaArm::Legacy);
        assert!(!effective_for_capture(None));
    }

    #[test]
    fn a7_gate_on_exposes_hashline_schema_at_every_layer() {
        let registry = BindingRegistry::new();
        let outcome = registry.register(root(), "s-on", on_request());
        assert!(outcome.effective);

        let guard = registry.capture(root(), "s-on").expect("bound");
        let arm = select_edit_schema_for_capture(Some(&guard));
        assert_eq!(arm, EditSchemaArm::Hashline);

        let schema = edit_schema_for(arm);
        assert_eq!(schema["required"], json!(["patch"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert!(schema["properties"].get("filePath").is_none());
        assert!(schema["properties"].get("edits").is_none());

        let entry = governed_edit_manifest_entry(arm);
        assert!(entry.supports_tool);
        assert!(entry.hoisted);
        assert_eq!(entry.lane, "mutation");
        assert_eq!(entry.name, "edit");

        // Translation routes only to hashline_edit before any legacy shape check.
        let translated =
            translate_edit_for_session(Some(&guard), &json!({"patch": "[a.rs#ABCD]\nREM"}))
                .expect("ok")
                .expect("gate-on");
        assert_eq!(translated.command, HASHLINE_EDIT_COMMAND);

        // Legacy keys are parse errors, never legacy-routed.
        let err = translate_edit_for_session(
            Some(&guard),
            &json!({"path": "a.rs", "oldString": "x", "newString": "y"}),
        )
        .expect_err("legacy keys rejected");
        assert_eq!(err.code, HashlineRejectionCode::ParseError);
        assert_eq!(err.stage, RejectionStage::Parse);
    }

    #[test]
    fn hashline_description_is_a_complete_agent_quick_reference() {
        for fragment in [
            "[path#TAG]",
            "four hexadecimal digits",
            "current tagged read",
            "REM and MV require a whole-file tagged read",
            "multiple sections compose in patch order against pre-request coordinates",
            "`0` (BOF)",
            "`N.=M`",
            "`<N`/`>N`",
            "`N*`/`<N*`/`>N*`",
            "`$`/`$-K`",
            "plain `N` PUT replaces",
            "`PUT <address>:`",
            "`+` alone is blank",
            "PUT without `:` copies",
            "CUT: `CUT <address> [@name]`",
            "REM: bare `REM` only",
            "MV: `MV <destination>`",
            "`*** Begin Patch`/`*** End Patch`",
        ] {
            assert!(
                HASHLINE_EDIT_DESCRIPTION.contains(fragment),
                "quick reference is missing {fragment:?}"
            );
        }
        assert_eq!(
            LEGACY_EDIT_DESCRIPTION,
            "Edit a file by finding and replacing text, or by targeting named symbols. To write or overwrite a whole file, use the `write` tool — `edit` requires an explicit edit mode and will not silently overwrite a file from `content` alone."
        );
    }

    #[test]
    fn a7_session_never_exposes_both_edit_schemas() {
        let artifacts = regenerate_governed_edit_artifacts();
        assert_eq!(artifacts["dual_mode"], json!(true));
        assert_eq!(
            artifacts["invariant"],
            json!("a session never exposes both edit schemas")
        );

        // One registration publishes exactly one arm.
        for effective in [false, true] {
            let arm = select_edit_schema(effective);
            let other = select_edit_schema(!effective);
            assert_ne!(arm, other);
            let schema = edit_schema_for(arm);
            let other_schema = edit_schema_for(other);
            assert_ne!(schema["required"], other_schema["required"]);
        }
    }

    #[test]
    fn a7_downgrade_warning_when_edit_not_registered() {
        let registry = BindingRegistry::new();
        let outcome = registry.register(
            root(),
            "s-down",
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: false,
                read_slot_survives: true,
            },
        );
        assert!(!outcome.effective);
        let warning = outcome.downgrade.expect("downgrade");
        assert_eq!(warning.code, "hashline_downgraded");
        assert_eq!(warning.reason, "edit_not_registered");
        assert_eq!(
            warning.to_json(),
            json!({"code": "hashline_downgraded", "reason": "edit_not_registered"})
        );

        let guard = registry.capture(root(), "s-down").expect("bound");
        assert!(!guard.effective());
        assert_eq!(
            select_edit_schema_for_capture(Some(&guard)),
            EditSchemaArm::Legacy
        );
    }

    #[test]
    fn a7_downgrade_names_the_tagged_read_slot_when_only_read_is_missing() {
        // The #292 surface: `edit` is registered and looks healthy, but nothing
        // in the session can mint a tag, so the patch arm must not be offered
        // and the warning has to name the slot that is actually missing.
        let registry = BindingRegistry::new();
        let outcome = registry.register(
            root(),
            "s-no-read",
            RegistrationRequest {
                configured_enabled: true,
                edit_slot_survives: true,
                read_slot_survives: false,
            },
        );
        assert!(!outcome.effective);
        let warning = outcome.downgrade.expect("downgrade");
        assert_eq!(warning.code, "hashline_downgraded");
        assert_eq!(warning.reason, "tagged_read_unavailable");

        let guard = registry.capture(root(), "s-no-read").expect("bound");
        assert!(!guard.effective());
        assert_eq!(
            select_edit_schema_for_capture(Some(&guard)),
            EditSchemaArm::Legacy
        );
        assert_eq!(
            edit_schema_for(select_edit_schema_for_capture(Some(&guard)))["required"],
            json!(["filePath"])
        );

        // Both slots missing keeps naming the read slot: it is the one that
        // makes the surface unusable even after the edit slot comes back.
        let both = RegistrationRequest {
            configured_enabled: true,
            edit_slot_survives: false,
            read_slot_survives: false,
        };
        assert_eq!(
            both.downgrade_warning().expect("downgrade").reason,
            "tagged_read_unavailable"
        );
    }

    #[test]
    fn hashline_description_names_the_tools_that_mint_tags() {
        // Navigation output looks edit-ready but publishes no snapshot, so the
        // description has to say which calls actually produce an addressable tag.
        assert!(HASHLINE_EDIT_DESCRIPTION.contains(
            "Only `read` (and accepted AFT `cat`/`head`/`tail` rewrites) mint hashline tags. \
`aft_zoom`, `aft_outline`, `grep`, `aft_search`, and conflict snippets do not. \
After navigation, call `read` on every file and range the patch addresses."
        ));
        assert!(!LEGACY_EDIT_DESCRIPTION.contains("mint hashline tags"));
    }

    #[test]
    fn a7_canary_matrix_profiles() {
        // default (enabled false), minimal surface (edit survives), all-only
        // pruning (edit survives when selected), disabled_tools:["edit"].
        let cases = [
            (false, true, false, false),
            (true, true, true, false),
            (true, true, true, false),
            (true, false, false, true),
        ];
        let registry = BindingRegistry::new();
        for (i, (configured, survives, expect_effective, expect_downgrade)) in
            cases.into_iter().enumerate()
        {
            let outcome = registry.register(
                root(),
                format!("canary-{i}"),
                RegistrationRequest {
                    configured_enabled: configured,
                    edit_slot_survives: survives,
                    read_slot_survives: true,
                },
            );
            assert_eq!(outcome.effective, expect_effective, "case {i}");
            assert_eq!(outcome.downgrade.is_some(), expect_downgrade, "case {i}");
        }
    }

    // ── A11 arm flip ────────────────────────────────────────────────────────

    #[test]
    fn a11_same_effective_reregistration_preserves_stores() {
        let registry = BindingRegistry::new();
        registry.register(root(), "s-preserve", on_request());
        {
            let guard = registry.capture(root(), "s-preserve").expect("bound");
            guard.with_binding_mut(|binding| {
                let snap = scan_bytes(b"hello\n");
                binding.snapshots_mut().publish("src/a.rs", snap);
            });
        }
        let outcome = registry.register(root(), "s-preserve", on_request());
        assert!(outcome.stores_preserved);
        assert!(!outcome.stores_cleared);

        let guard = registry.capture(root(), "s-preserve").expect("bound");
        guard.with_binding(|binding| {
            assert!(binding
                .snapshots()
                .contains("src/a.rs", &scan_bytes(b"hello\n").tag));
        });
    }

    #[test]
    fn a11_effective_flip_clears_only_that_session_stores() {
        let registry = BindingRegistry::new();
        registry.register(root(), "s-a", on_request());
        registry.register(root(), "s-b", on_request());

        {
            let a = registry.capture(root(), "s-a").expect("a");
            a.with_binding_mut(|b| {
                b.snapshots_mut().publish("a.rs", scan_bytes(b"aaa\n"));
            });
            let b = registry.capture(root(), "s-b").expect("b");
            b.with_binding_mut(|b| {
                b.snapshots_mut().publish("b.rs", scan_bytes(b"bbb\n"));
            });
        }

        let outcome = registry.register(root(), "s-a", off_request());
        assert!(outcome.stores_cleared);
        assert!(!outcome.effective);

        let a = registry.capture(root(), "s-a").expect("a");
        a.with_binding(|b| {
            assert!(b.snapshots().is_empty());
            assert!(!b.effective());
        });
        let b = registry.capture(root(), "s-b").expect("b");
        b.with_binding(|b| {
            assert!(b.snapshots().contains("b.rs", &scan_bytes(b"bbb\n").tag));
            assert!(b.effective());
        });
    }

    #[test]
    fn a11_concurrent_sessions_opposite_modes_under_one_root() {
        let registry = BindingRegistry::new();
        registry.register(root(), "on", on_request());
        registry.register(root(), "off", off_request());

        let on = registry.capture(root(), "on").expect("on");
        let off = registry.capture(root(), "off").expect("off");
        assert!(on.effective());
        assert!(!off.effective());
        assert_eq!(
            select_edit_schema_for_capture(Some(&on)),
            EditSchemaArm::Hashline
        );
        assert_eq!(
            select_edit_schema_for_capture(Some(&off)),
            EditSchemaArm::Legacy
        );
    }

    #[test]
    fn a11_rebind_drains_in_flight_before_clearing_stores() {
        let registry = Arc::new(BindingRegistry::new());
        registry.register(root(), "drain", on_request());

        let barrier = Arc::new(Barrier::new(2));
        let registry_edit = Arc::clone(&registry);
        let barrier_edit = Arc::clone(&barrier);

        let editor = thread::spawn(move || {
            let guard = registry_edit.capture(root(), "drain").expect("capture");
            guard.with_binding_mut(|b| {
                b.snapshots_mut().publish("x.rs", scan_bytes(b"before\n"));
            });
            // Signal re-register thread that we hold the binding.
            barrier_edit.wait();
            // Hold briefly so re-register must drain.
            thread::sleep(Duration::from_millis(50));
            let count = guard.with_binding(|b| b.snapshots().snapshot_count());
            // Edit still sees the old store while the guard is held.
            assert_eq!(count, 1);
            drop(guard);
        });

        barrier.wait();
        let outcome = registry.register(root(), "drain", off_request());
        assert!(outcome.stores_cleared);
        assert!(!outcome.effective);

        editor.join().expect("editor");

        let after = registry.capture(root(), "drain").expect("after");
        after.with_binding(|b| {
            assert!(b.snapshots().is_empty());
            assert!(!b.effective());
        });
    }

    #[test]
    fn a11_teardown_removes_binding() {
        let registry = BindingRegistry::new();
        registry.register(root(), "gone", on_request());
        assert!(registry.teardown(root(), "gone"));
        assert!(registry.capture(root(), "gone").is_none());
        assert!(!registry.teardown(root(), "gone"));
    }

    // ── A14 host display ────────────────────────────────────────────────────

    #[test]
    fn a14_single_file_display_envelope() {
        let after = b"line\n";
        let tag = scan_bytes(after).tag;
        let file = synthetic_applied_file("src/a.rs", b"old\n", after, &tag);
        let envelope = synthetic_envelope(true, true, vec![file], Some("op-1".into()), false);
        let display = vec![DisplayFileBytes {
            requested_path: "src/a.rs".into(),
            before: b"old\n".to_vec(),
            after: Some(after.to_vec()),
            remove_file: false,
            move_from: None,
        }];
        let payload = render_mutation_response(MutationRenderInput {
            envelope: &envelope,
            display_files: &display,
            project_root: Some(root().as_path()),
            transport: TransportKind::OpenCode,
        });

        assert_eq!(payload["success"], json!(true));
        assert_eq!(payload["complete"], json!(true));
        assert_eq!(payload["filePath"], json!("src/a.rs"));
        assert!(payload["metadata"]["diff"]
            .as_str()
            .unwrap()
            .contains("src/a.rs"));
        assert_eq!(
            payload["metadata"]["files"][0]["relativePath"],
            json!("src/a.rs")
        );
        assert!(payload["output"]
            .as_str()
            .unwrap()
            .contains(&format!("[src/a.rs#{tag}]")));
        assert_eq!(payload["op_id"], json!("op-1"));
    }

    #[test]
    fn a14_multi_file_patch_order_stable_metadata() {
        let a_after = b"a\n";
        let b_after = b"b\n";
        let files = vec![
            synthetic_applied_file("a.rs", b"A\n", a_after, &scan_bytes(a_after).tag),
            synthetic_applied_file("b.rs", b"B\n", b_after, &scan_bytes(b_after).tag),
        ];
        let envelope = synthetic_envelope(true, true, files, Some("op-m".into()), false);
        let display = vec![
            DisplayFileBytes {
                requested_path: "a.rs".into(),
                before: b"A\n".to_vec(),
                after: Some(a_after.to_vec()),
                remove_file: false,
                move_from: None,
            },
            DisplayFileBytes {
                requested_path: "b.rs".into(),
                before: b"B\n".to_vec(),
                after: Some(b_after.to_vec()),
                remove_file: false,
                move_from: None,
            },
        ];
        let payload = render_mutation_response(MutationRenderInput {
            envelope: &envelope,
            display_files: &display,
            project_root: None,
            transport: TransportKind::Ndjson,
        });
        let meta_files = payload["metadata"]["files"].as_array().unwrap();
        assert_eq!(meta_files.len(), 2);
        assert_eq!(meta_files[0]["relativePath"], json!("a.rs"));
        assert_eq!(meta_files[1]["relativePath"], json!("b.rs"));
        // First-file filePath contract.
        assert_eq!(payload["filePath"], json!("a.rs"));
    }

    #[test]
    fn a14_mv_displays_destination_diff_and_names_source() {
        let dest = FileOutcome {
            canonical_path: PathBuf::from("dest.rs"),
            requested_path: "dest.rs".into(),
            role: FileRole::MvDestination,
            classification: FileClassification::Applied,
            mutation_state: FileClassification::Applied.mutation_state(),
            final_bytes: Some(b"moved\n".to_vec()),
            final_tag: Some(scan_bytes(b"moved\n").tag),
            affected: Default::default(),
            warnings: vec![],
            format_skipped_reason: None,
            backup_id: Some("bak".into()),
            remove_file: false,
            tag_notice: None,
        };
        let source = FileOutcome {
            canonical_path: PathBuf::from("src.rs"),
            requested_path: "src.rs".into(),
            role: FileRole::MvSource,
            classification: FileClassification::Applied,
            mutation_state: FileClassification::Applied.mutation_state(),
            final_bytes: None,
            final_tag: None,
            affected: Default::default(),
            warnings: vec![],
            format_skipped_reason: None,
            backup_id: None,
            remove_file: true,
            tag_notice: None,
        };
        let envelope =
            synthetic_envelope(true, true, vec![dest, source], Some("op-mv".into()), false);
        let display = vec![DisplayFileBytes {
            requested_path: "dest.rs".into(),
            before: vec![],
            after: Some(b"moved\n".to_vec()),
            remove_file: false,
            move_from: Some("src.rs".into()),
        }];
        let payload = render_mutation_response(MutationRenderInput {
            envelope: &envelope,
            display_files: &display,
            project_root: None,
            transport: TransportKind::OpenCode,
        });
        let entry = &payload["metadata"]["files"][0];
        assert_eq!(entry["type"], json!("move"));
        assert_eq!(entry["sourcePath"], json!("src.rs"));
        assert!(payload["output"]
            .as_str()
            .unwrap()
            .contains("source removed: src.rs"));
    }

    #[test]
    fn a14_stripped_envelope_remains_renderable_including_all_failed() {
        let failed = synthetic_failed_file("a.rs", FileClassification::FailedBaselineDrift);
        let skipped = synthetic_failed_file("b.rs", FileClassification::NotAttempted);
        let envelope = synthetic_envelope(false, false, vec![failed, skipped], None, false);
        let display = vec![
            DisplayFileBytes {
                requested_path: "a.rs".into(),
                before: b"a\n".to_vec(),
                after: None,
                remove_file: false,
                move_from: None,
            },
            DisplayFileBytes {
                requested_path: "b.rs".into(),
                before: b"b\n".to_vec(),
                after: None,
                remove_file: false,
                move_from: None,
            },
        ];

        for transport in TransportKind::ALL {
            let payload = render_mutation_response(MutationRenderInput {
                envelope: &envelope,
                display_files: &display,
                project_root: None,
                transport: *transport,
            });
            assert!(
                all_failed_payload_preserved(&payload),
                "transport {}",
                transport.as_str()
            );
            let stripped = strip_hashline_fields(&payload);
            assert!(shipped_envelope_is_renderable(&stripped));
            assert!(stripped.get("hashline").is_none());
            assert!(stripped.get("classifications").is_none());
            let output = stripped["output"].as_str().unwrap();
            assert!(
                output.starts_with("0 of 2 files applied"),
                "stripped text must lead with counts: {output}"
            );
            assert_eq!(stripped["success"], json!(false));
            assert_eq!(stripped["all_failed"], json!(true));
        }
    }

    // ── A15 transport preservation (Pi first-class + all hosts) ─────────────

    #[test]
    fn a15_text_tag_carrier_survives_all_transports() {
        let after = b"tagged\n";
        let tag = scan_bytes(after).tag;
        let file = synthetic_applied_file("lib.rs", b"old\n", after, &tag);
        let envelope = synthetic_envelope(true, true, vec![file], Some("op".into()), false);
        let display = vec![DisplayFileBytes {
            requested_path: "lib.rs".into(),
            before: b"old\n".to_vec(),
            after: Some(after.to_vec()),
            remove_file: false,
            move_from: None,
        }];

        let mut payloads = Vec::new();
        for transport in TransportKind::ALL {
            let payload = render_mutation_response(MutationRenderInput {
                envelope: &envelope,
                display_files: &display,
                project_root: None,
                transport: *transport,
            });
            let output = payload["output"].as_str().unwrap();
            assert!(
                output.contains(&format!("[lib.rs#{tag}]")),
                "{} missing tag carrier",
                transport.as_str()
            );
            assert_eq!(payload["transport"], json!(transport.as_str()));
            payloads.push((*transport, payload));
        }
        assert!(transports_preserve_carriers(payloads.as_slice()));

        // MCP mirrors text under content for adapters.
        let mcp = payloads
            .iter()
            .find(|(t, _)| *t == TransportKind::Mcp)
            .map(|(_, p)| p)
            .unwrap();
        assert_eq!(mcp["content"][0]["type"], json!("text"));
        assert!(mcp["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(&format!("[lib.rs#{tag}]")));

        // Pi and OpenCode keep filePath + metadata for hoisted adapters.
        for kind in [TransportKind::Pi, TransportKind::OpenCode] {
            let p = payloads
                .iter()
                .find(|(t, _)| *t == kind)
                .map(|(_, p)| p)
                .unwrap();
            assert!(p.get("filePath").is_some());
            assert!(p["metadata"]["files"].is_array());
        }
    }

    #[test]
    fn a15_all_failed_failure_payloads_through_shipped_host_paths() {
        let files = vec![
            synthetic_failed_file("one.rs", FileClassification::FailedBackup),
            synthetic_failed_file("two.rs", FileClassification::NotAttempted),
        ];
        // op_id present when a journal entry existed before failure.
        let envelope = synthetic_envelope(
            false,
            false,
            files,
            Some("op-partial-journal".into()),
            false,
        );
        let display = vec![
            DisplayFileBytes {
                requested_path: "one.rs".into(),
                before: b"1\n".to_vec(),
                after: None,
                remove_file: false,
                move_from: None,
            },
            DisplayFileBytes {
                requested_path: "two.rs".into(),
                before: b"2\n".to_vec(),
                after: None,
                remove_file: false,
                move_from: None,
            },
        ];

        for transport in TransportKind::ALL {
            let payload = render_mutation_response(MutationRenderInput {
                envelope: &envelope,
                display_files: &display,
                project_root: None,
                transport: *transport,
            });
            assert_eq!(payload["success"], json!(false));
            assert_eq!(payload["all_failed"], json!(true));
            assert_eq!(payload["complete"], json!(false));
            assert_eq!(payload["op_id"], json!("op-partial-journal"));
            assert!(payload["classifications"].as_array().unwrap().len() >= 2);
            assert!(all_failed_payload_preserved(&payload));
        }
    }

    #[test]
    fn a15_preview_has_no_op_id_or_final_tags() {
        let file = synthetic_applied_file("p.rs", b"a\n", b"b\n", "ABCD");
        let envelope = synthetic_envelope(true, true, vec![file], None, true);
        let display = vec![DisplayFileBytes {
            requested_path: "p.rs".into(),
            before: b"a\n".to_vec(),
            after: Some(b"b\n".to_vec()),
            remove_file: false,
            move_from: None,
        }];
        let payload = render_mutation_response(MutationRenderInput {
            envelope: &envelope,
            display_files: &display,
            project_root: None,
            transport: TransportKind::Pi,
        });
        assert_eq!(payload["preview"], json!(true));
        assert!(payload.get("op_id").is_none());
        assert!(payload["output"]
            .as_str()
            .unwrap()
            .contains("preview: no files were modified"));
    }

    // ── Preflight + permission orchestration ────────────────────────────────

    #[test]
    fn preflight_is_parse_only_and_lists_affected_paths() {
        let patch = "\
[src/a.rs#ABCD]
PUT 1:
+hello
[src/b.rs#EF01]
CUT 2
MV dest/b.rs";
        let result = hashline_preflight(patch, Some(root().as_path())).expect("preflight");
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.affected_rel_paths, vec!["src/a.rs", "src/b.rs"]);
        assert!(result.affected_paths[0].ends_with("src/a.rs"));
        assert!(result
            .mv_destinations
            .iter()
            .any(|p| p.ends_with("dest/b.rs")));
        assert_eq!(result.files[0].operations[0].kind, "PUT");
        assert_eq!(result.files[1].operations[0].kind, "CUT");
        assert_eq!(result.files[1].operations[1].kind, "MV");

        let patterns = result.permission_patterns();
        assert!(patterns.iter().any(|p| p.contains("src/a.rs")));
        assert!(patterns.iter().any(|p| p.contains("dest/b.rs")));

        let meta = permission_metadata(&result);
        assert_eq!(meta["surface"], json!("hashline"));
        assert_eq!(meta["file_count"], json!(2));

        assert_eq!(
            PERMISSION_ORCHESTRATION_ORDER,
            &[
                PermissionPhase::Preflight,
                PermissionPhase::PermissionCheck,
                PermissionPhase::Preview,
                PermissionPhase::Apply,
            ]
        );
    }

    #[test]
    fn preflight_rejects_legacy_shaped_args() {
        let err = hashline_preflight_from_args(&json!({"path": "a.rs", "edits": []}), None)
            .expect_err("legacy");
        assert_eq!(err.code, HashlineRejectionCode::ParseError);
    }

    // ── A18 transport + steering portions ───────────────────────────────────

    #[test]
    fn a18_rejection_registry_covers_every_code_with_stage_and_steering() {
        let registry = rejection_transport_registry();
        let codes: std::collections::BTreeSet<_> =
            registry.iter().map(|r| r.code.as_str()).collect();
        for code in [
            "hashline_missing_tag",
            "hashline_malformed_tag",
            "hashline_unknown_tag",
            "hashline_evicted_tag",
            "hashline_ambiguous_tag",
            "hashline_stale_tag",
            "hashline_unseen_line",
            "hashline_boundary_ineligible",
            "hashline_untaggable_path",
            "hashline_register_overflow",
            "hashline_backup_unavailable",
            "hashline_parse_error",
        ] {
            assert!(codes.contains(code), "missing {code}");
        }

        // Both ambiguous stages with opposite steering.
        let amb_res = registry
            .iter()
            .find(|r| {
                r.code == HashlineRejectionCode::AmbiguousTag
                    && r.stage == RejectionStage::Resolution
            })
            .expect("ambiguous resolution");
        let amb_rec = registry
            .iter()
            .find(|r| {
                r.code == HashlineRejectionCode::AmbiguousTag && r.stage == RejectionStage::Recovery
            })
            .expect("ambiguous recovery");
        assert_ne!(amb_res.steering, amb_rec.steering);
        assert!(amb_res.steering.contains("non-hashline"));
        assert!(amb_res.steering.contains("re-reading preserves"));
        assert!(amb_rec.steering.contains("re-address"));

        // Both stale stages with opposite steering.
        let stale_ver = registry
            .iter()
            .find(|r| {
                r.code == HashlineRejectionCode::StaleTag && r.stage == RejectionStage::Verification
            })
            .expect("stale verification");
        let stale_rec = registry
            .iter()
            .find(|r| {
                r.code == HashlineRejectionCode::StaleTag && r.stage == RejectionStage::Recovery
            })
            .expect("stale recovery");
        assert_ne!(stale_ver.steering, stale_rec.steering);

        for contract in &registry {
            assert!(!contract.mutates_files);
            assert!(!contract.mutates_stores);
            assert_eq!(contract.transport_status, "error");
            let rejection = rejection_for_contract(contract);
            assert_eq!(rejection.steering, contract.steering);
            for transport in TransportKind::ALL {
                let payload = render_rejection_response(&rejection, *transport);
                let fields = required_rejection_fields(&payload);
                assert_eq!(fields["code"], json!(contract.code.as_str()));
                assert_eq!(fields["stage"], json!(contract.stage.as_str()));
                assert_eq!(fields["steering"], json!(contract.steering));
                assert_eq!(fields["success"], json!(false));
                assert!(fields["output"]
                    .as_str()
                    .unwrap()
                    .contains(contract.steering));
            }
        }
    }

    #[test]
    fn a18_gate_on_legacy_keys_are_parse_stage_on_every_transport() {
        let rejection = translate_gate_on_edit(&json!({
            "filePath": "a.rs",
            "edits": [{"oldString": "x", "newString": "y"}]
        }))
        .expect_err("legacy");
        assert_eq!(rejection.stage, RejectionStage::Parse);
        for transport in TransportKind::ALL {
            let payload = render_rejection_response(&rejection, *transport);
            assert_eq!(payload["stage"], json!("parse"));
            assert_eq!(payload["code"], json!("hashline_parse_error"));
        }
    }

    // ── Governed schema regeneration ────────────────────────────────────────

    #[test]
    fn governed_artifacts_embed_both_arms_and_hashline_only_patch_field() {
        let doc = regenerate_governed_edit_artifacts();
        let legacy = &doc["arms"]["legacy"]["schema"];
        let hashline = &doc["arms"]["hashline"]["schema"];
        assert_eq!(legacy["required"], json!(["filePath"]));
        assert_eq!(hashline["required"], json!(["patch"]));
        assert_eq!(
            doc["arms"]["hashline"]["command"],
            json!(HASHLINE_EDIT_COMMAND)
        );
        assert_eq!(
            doc["arms"]["hashline"]["preflight_command"],
            json!(HASHLINE_PREFLIGHT_COMMAND)
        );
        // Gate-off translation remains a no-op from this module.
        let off = translate_edit_for_session(None, &json!({"filePath": "a.rs"}))
            .expect("legacy passthrough");
        assert!(off.is_none());
    }

    #[test]
    fn header_syntax_boundary_steering_stable_across_transports() {
        let missing = HashlineRejection::missing_tag("no tag");
        let malformed = HashlineRejection::malformed_tag("bad tag");
        assert_eq!(missing.stage, RejectionStage::Header);
        assert_eq!(malformed.stage, RejectionStage::Header);
        assert_eq!(missing.steering, malformed.steering);
        for transport in TransportKind::ALL {
            let p = render_rejection_response(&missing, *transport);
            assert_eq!(p["stage"], json!("header"));
        }
    }
}
