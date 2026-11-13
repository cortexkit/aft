use super::*;
use crate::bash_background::persistence::{read_task_at, write_task_at, ResolvedTask};
use crate::subc::wire::WriterFrame;

fn replace_command(ctx: &crate::context::AppContext, task: &ResolvedTask, command: &str) {
    let mut metadata = read_task_at(task).unwrap();
    metadata.command = command.into();
    write_task_at(task, &metadata).unwrap();
    let db = ctx.db().unwrap();
    crate::db::bash_tasks::upsert_bash_task(
        &db.lock().unwrap(),
        &metadata.to_bash_task_row("test", &task.paths).unwrap(),
    )
    .unwrap();
}

async fn receive(
    output: &mut tokio::sync::mpsc::Receiver<WriterFrame>,
    corr: u64,
    ty: FrameType,
) -> Value {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame = output.recv().await.expect("shells writer closed");
            if frame.header.corr == corr && frame.header.ty == FrameType::Error {
                panic!(
                    "shells request {corr} failed: {}",
                    String::from_utf8_lossy(frame.body())
                );
            }
            if frame.header.corr == corr && frame.header.ty == ty {
                return serde_json::from_slice(frame.body()).unwrap();
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("shells request {corr} did not produce {ty:?}"))
}

async fn request(
    service: &Arc<Service>,
    tx: &WriterSender,
    metrics: &Arc<DispatchPathMetrics>,
    identity: &ManagementIdentity,
    corr: u64,
    method: &str,
) {
    let frame = Frame::build(
        FrameType::Request,
        subc_protocol::Flags(0),
        41,
        1,
        corr,
        Vec::new(),
    )
    .unwrap();
    service
        .dispatch(
            tx,
            metrics,
            &frame,
            identity,
            &json!({"method":method,"params":{}}),
        )
        .await
        .unwrap();
}

async fn settled_calls(service: &Service) -> u64 {
    let _refresh = service.refresh_gate.lock().await;
    // The per-service counter keeps collections in concurrently running
    // fixtures from affecting the idle assertions.
    service.collect_calls.load(Ordering::Relaxed)
}

#[tokio::test]
async fn shells_v1_last_watch_stops_collection_and_reopens_with_current_first_frame() {
    let (dir, ctx, executor, app) = status_context();
    let (_, _, task) = captured(dir.path(), b"initial\n", false, true);
    replace_command(&ctx, &task, "printf before");
    let service = Service::new(app, executor);
    let _stop = service.stop.clone().drop_guard();
    let identity = ManagementIdentity {
        principal: Some(subc_protocol::Principal::Direct),
        ..Default::default()
    };
    let metrics = Arc::new(DispatchPathMetrics::new());
    let (tx, mut output) = tokio::sync::mpsc::channel(32);

    request(&service, &tx, &metrics, &identity, 1, "shells.watch").await;
    let first = receive(&mut output, 1, FrameType::StreamData).await;
    assert_eq!(first["type"], "resync");
    assert_eq!(first["rows"][0]["command"], "printf before");
    let initial_calls = settled_calls(&service).await;
    replace_command(&ctx, &task, "printf while-watched");
    crate::bash_background::shell_metadata::facts_changed();
    let changed = receive(&mut output, 1, FrameType::StreamData).await;
    assert_eq!(changed["type"], "upsert");
    assert_eq!(changed["row"]["command"], "printf while-watched");
    assert!(settled_calls(&service).await > initial_calls);

    // Queue a background refresh behind the collection gate, then close the
    // final viewer before releasing it. Waiting work must recheck the viewer.
    let gate = service.refresh_gate.lock().await;
    let before_close = service.collect_calls.load(Ordering::Relaxed);
    let queued_service = service.clone();
    let queued = tokio::spawn(async move { queued_service.refresh_if_subscribed(true).await });
    tokio::task::yield_now().await;
    assert!(service.cancel(route_key(41, 1), 1).await);
    drop(gate);
    queued.await.unwrap();
    assert_eq!(settled_calls(&service).await, before_close);
    assert!(service.subscriptions.lock().await.is_empty());
    let cached_key = service.db_facts.read().await.key.clone();
    assert!(cached_key.is_some());

    replace_command(&ctx, &task, "printf while-idle");
    crate::bash_background::shell_metadata::facts_changed();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        settled_calls(&service).await,
        before_close,
        "metadata and timer wakes collected without a viewer"
    );
    assert!(
        service.db_facts.read().await.key == cached_key,
        "idle service discarded its warm facts"
    );

    request(&service, &tx, &metrics, &identity, 2, "shells.list").await;
    let listed = receive(&mut output, 2, FrameType::Response).await;
    assert_eq!(listed["result"]["rows"][0]["command"], "printf while-idle");
    let after_list = settled_calls(&service).await;
    assert_eq!(after_list, before_close + 1);
    crate::bash_background::shell_metadata::facts_changed();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        settled_calls(&service).await,
        after_list,
        "list left background collection enabled"
    );

    replace_command(&ctx, &task, "printf reopened");
    // With the periodic driver stopped, only the request's direct refresh can
    // make the first frame current; it cannot be rescued by the next tick.
    service.stop();
    service.driver_done.cancelled().await;
    request(&service, &tx, &metrics, &identity, 3, "shells.watch").await;
    let reopened = receive(&mut output, 3, FrameType::StreamData).await;
    assert_eq!(reopened["type"], "resync");
    assert_eq!(reopened["rows"][0]["command"], "printf reopened");
    assert_eq!(settled_calls(&service).await, after_list + 1);
    assert!(service.cancel(route_key(41, 1), 3).await);
}
