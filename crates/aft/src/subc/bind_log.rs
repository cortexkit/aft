use super::Principal;

pub(super) fn consumer_capabilities(
    principal: &Option<Principal>,
    harness: &str,
    capabilities: &[String],
) {
    crate::slog_info!(
        "subc bind consumer capabilities: principal={} harness={harness} tool_result_v1={} held_reply_v1={} consumer_capabilities={:?} consumer_capabilities_total={}",
        super::principal_label(principal),
        capabilities.iter().any(|capability| capability == super::result_body::RESULT_FORMAT),
        capabilities.iter().any(|capability| capability == "held-reply/v1"),
        &capabilities[..capabilities.len().min(8)],
        capabilities.len(),
    );
}

#[cfg(test)]
mod tests {
    use super::super::*;

    fn bind_answer(
        principal: &Option<Principal>,
        harness: &str,
        capabilities: &[String],
    ) -> (Frame, Option<ManagementIdentity>) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let app = App::default_shared();
            let executor = Arc::new(Executor::new());
            let (writer, mut replies) = mpsc::channel(8);
            let (completion, mut completed) = mpsc::channel(8);
            let (lossy_tx, _lossy_rx) = mpsc::channel(8);
            let (reliable_tx, _reliable_rx) = mpsc::unbounded_channel();
            let a0 = A0Egress::new(writer.clone(), completion.clone());
            let senders = PushSenders {
                lossy_tx,
                reliable_tx,
                lossy_overflow: Arc::new(push::LossyOverflow::default()),
                lossy_seq: Arc::new(AtomicU64::new(0)),
                fleet_status_client: FleetStatusClient::channel(1).0,
            };
            let request = {
                let mut bind = serde_json::to_value(ModuleControlRequest::RouteBind {
                    route_channel: 7,
                    epoch: 1,
                    target: RouteTarget::ManagementSurface {
                        module_id: "aft".into(),
                    },
                    identity: subc_protocol::BindIdentity::new(harness, "bind-log"),
                    principal: principal.clone(),
                    consumer_capabilities: if capabilities.is_empty() {
                        None
                    } else {
                        Some(capabilities.to_vec())
                    },
                    admission_facts: Default::default(),
                    scope: None,
                    role_versions: None,
                    call_mode: None,
                })
                .unwrap();
                bind["identity"]["project_root"] = serde_json::json!(Path::new("."));

                bind
            };
            let frame = Frame::build(
                FrameType::Request,
                control_flags(),
                0,
                0,
                91,
                serde_json::to_vec(&request).unwrap(),
            )
            .unwrap();
            handle_control_request(
                &writer,
                &writer,
                &a0,
                None,
                &frame,
                &app,
                &executor,
                &Arc::new(standing::StandingActor::new(app.clone(), executor.clone())),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &mut HashMap::new(),
                &Arc::new(StdMutex::new(HashMap::new())),
                &mut PendingSubcResponses::default(),
                &mut RetryBuffer::new(),
                &mut HashMap::new(),
                &Arc::new(Notify::new()),
                &completion,
                &Arc::new(DispatchPathMetrics::new()),
                None,
                &HealthRollupCache::new(),
                &senders,
                |_, _| panic!("management bind must not dispatch a tool"),
                None,
                None,
                usize::MAX,
                None,
                // A management bind creates no actor, so no storage is opened.
                Path::new("unused-management-storage"),
            )
            .await
            .unwrap();
            let mut reply = replies.try_recv().expect("management bind answer");
            assert_eq!(reply.header.corr, 91);
            let identity = if reply.header.ty == FrameType::Response {
                assert!(reply.begin_ack_write());
                reply.finish_ack_write(true);
                let ControlCompletion::ManagementAckWritten {
                    route, identity, ..
                } = completed.recv().await.expect("written management ack")
                else {
                    panic!("management bind must publish a management ack");
                };
                assert_eq!(route, route_key(7, 1));
                Some(identity)
            } else {
                None
            };
            (reply.frame.clone(), identity)
        })
    }

    fn bind(principal: &Option<Principal>, harness: &str, capabilities: &[String]) {
        let (reply, _) = bind_answer(principal, harness, capabilities);
        if matches!(principal, Some(Principal::Direct)) {
            let decoded: ModuleControlResponse = serde_json::from_slice(&reply.body).unwrap();
            assert!(matches!(
                decoded,
                ModuleControlResponse::RouteBindAck { .. }
            ));
        } else {
            assert_eq!(reply.header.ty, FrameType::Error);
        }
    }

    #[test]
    fn management_bind_requires_the_catalog_display_declaration() {
        let entry = |id: &str, provides: Vec<&str>| -> subc_client_rs::CatalogEntry {
            serde_json::from_value(json!({
                "module_id": id, "roles": [], "control_ops": [],
                "capabilities": {"provides": provides, "requires": [], "must_never_reach": []}
            }))
            .unwrap()
        };
        let other = Some(Principal::Reserved {
            module_id: "other-module".into(),
        });
        let display = Some(Principal::Reserved {
            module_id: "display-module".into(),
        });
        for modules in [
            vec![entry("other-module", vec![])],
            vec![
                entry("other-module", vec![]),
                entry("display-module", vec!["tool-display/v1"]),
            ],
        ] {
            let reader = rich_ui::display::declared_display(&modules);
            crate::late_result_bus::with_test_reader(reader.as_deref(), || {
                let (reply, route) = bind_answer(&other, "other-module", &[]);
                assert_eq!(reply.header.ty, FrameType::Error);
                let error: ErrorBody = serde_json::from_slice(&reply.body).unwrap();
                assert_eq!(error.code, "route_refused");
                assert_eq!(
                    error.message,
                    "AFT management routes require a first-party principal"
                );
                assert!(route.is_none());

                let (reply, route) = bind_answer(&display, "display-module", &[]);
                if reader.is_none() {
                    assert_eq!(reply.header.ty, FrameType::Error);
                    assert!(route.is_none());
                    return;
                }
                assert_eq!(reply.header.ty, FrameType::Response);
                assert!(matches!(
                    serde_json::from_slice::<ModuleControlResponse>(&reply.body).unwrap(),
                    ModuleControlResponse::RouteBindAck { .. }
                ));
                let route = route.expect("declaring display module has an installed route");
                assert!(route.admission == ManagementAdmission::DisplayOnly);
                for role in manifest::build_manifest().provides {
                    if let subc_protocol::manifest::ProviderRole::ManagementSurface {
                        operations,
                        ..
                    } = role
                    {
                        for op in operations {
                            for request in [json!({"name": op.name}), json!({"op": op.name})] {
                                assert_eq!(
                                    admit_management_flow_request(&route, Some(&request)).is_ok(),
                                    op.name == cortexkit_role_tool_display::DISPLAY_READ,
                                    "{}",
                                    op.name
                                );
                            }
                        }
                    }
                }
            });
        }
        let flow = ManagementIdentity {
            admission: ManagementAdmission::FlowOnly,
            ..Default::default()
        };
        assert_eq!(
            admit_management_flow_request(&flow, Some(&json!({"op": "display.read"})))
                .unwrap_err()
                .code,
            "flow_scope_not_supported"
        );
    }

    #[test]
    fn bind_log_records_consumer_capabilities() {
        let principal = Some(Principal::Reserved {
            module_id: "opencode".to_string(),
        });
        let (_, lines) = crate::logging::capture_log_lines(|| {
            bind(
                &principal,
                "opencode",
                &["tool-result/v1".into(), "held-reply/v1".into()],
            );
            bind(&None, "mcp", &[]);
            bind(&Some(Principal::Direct), "pi", &["tool-result/v1".into()]);
            bind(
                &principal,
                "opencode",
                &[
                    "a".into(),
                    "b".into(),
                    "c".into(),
                    "d".into(),
                    "e".into(),
                    "f".into(),
                    "g".into(),
                    "h".into(),
                    "held-reply/v1".into(),
                    "tool-result/v1".into(),
                ],
            );
        });
        let lines: Vec<_> = lines
            .into_iter()
            .filter(|line| line.starts_with("INFO subc bind consumer capabilities:"))
            .collect();
        assert_eq!(lines.len(), 4, "one INFO line per bind: {lines:?}");
        assert_eq!(lines[0], "INFO subc bind consumer capabilities: principal=reserved:opencode harness=opencode tool_result_v1=true held_reply_v1=true consumer_capabilities=[\"tool-result/v1\", \"held-reply/v1\"] consumer_capabilities_total=2");
        assert_eq!(lines[1], "INFO subc bind consumer capabilities: principal=absent harness=mcp tool_result_v1=false held_reply_v1=false consumer_capabilities=[] consumer_capabilities_total=0");
        assert_eq!(lines[2], "INFO subc bind consumer capabilities: principal=direct harness=pi tool_result_v1=true held_reply_v1=false consumer_capabilities=[\"tool-result/v1\"] consumer_capabilities_total=1");
        assert_eq!(lines[3], "INFO subc bind consumer capabilities: principal=reserved:opencode harness=opencode tool_result_v1=true held_reply_v1=true consumer_capabilities=[\"a\", \"b\", \"c\", \"d\", \"e\", \"f\", \"g\", \"h\"] consumer_capabilities_total=10");
    }
}
