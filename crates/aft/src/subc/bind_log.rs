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

    fn bind(principal: &Option<Principal>, harness: &str, capabilities: &[String]) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let app = App::default_shared();
            let executor = Arc::new(Executor::new());
            let (writer, mut replies) = mpsc::channel(8);
            let (completion, _completed) = mpsc::channel(8);
            let (lossy_tx, _lossy_rx) = mpsc::channel(8);
            let (reliable_tx, _reliable_rx) = mpsc::unbounded_channel();
            let senders = PushSenders {
                lossy_tx,
                reliable_tx,
                lossy_overflow: Arc::new(push::LossyOverflow::default()),
                lossy_seq: Arc::new(AtomicU64::new(0)),
                fleet_status_client: FleetStatusClient::channel(1).0,
            };
            let request = ModuleControlRequest::RouteBind {
                route_channel: 7,
                epoch: 1,
                target: RouteTarget::ManagementSurface {
                    module_id: "aft".into(),
                },
                identity: subc_protocol::BindIdentity::new(Path::new("."), harness, "bind-log"),
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
                &A0Egress::new(writer.clone(), completion.clone()),
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
                usize::MAX,
                None,
                // A management bind creates no actor, so no storage is opened.
                Path::new("unused-management-storage"),
            )
            .await
            .unwrap();
            let reply = replies.try_recv().expect("management bind answer");
            assert_eq!(reply.header.corr, 91);
            if matches!(principal, Some(Principal::Direct)) {
                let decoded: ModuleControlResponse = serde_json::from_slice(&reply.body).unwrap();
                assert!(matches!(
                    decoded,
                    ModuleControlResponse::RouteBindAck { .. }
                ));
            } else {
                assert_eq!(reply.header.ty, FrameType::Error);
            }
        });
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
