use crate::subc::*;

use super::{detach_shell_row, LedgerEdge};

#[derive(Default)]
pub(in crate::subc) struct ShellDetaches(tokio::task::JoinSet<()>);

impl ShellDetaches {
    pub(in crate::subc) fn start(
        &mut self,
        edge: &LedgerEdge,
        executor: &Arc<Executor>,
        routes: &HashMap<RouteChannel, RouteIdentity>,
    ) {
        while let Some(result) = self.0.try_join_next() {
            if let Err(error) = result {
                log::warn!("call ledger detach worker failed: {error}");
            }
        }
        edge.start_detaches(executor, |route| !routes.contains_key(&route), &mut self.0);
    }

    pub(in crate::subc) async fn finish(mut self, edge: &LedgerEdge, executor: &Arc<Executor>) {
        edge.start_detaches(executor, |_| true, &mut self.0);
        while let Some(result) = self.0.join_next().await {
            if let Err(error) = result {
                log::warn!("call ledger detach worker failed: {error}");
            }
        }
    }
}

impl LedgerEdge {
    fn start_detaches(
        &self,
        executor: &Arc<Executor>,
        gone: impl Fn(RouteChannel) -> bool,
        tasks: &mut tokio::task::JoinSet<()>,
    ) {
        self.received
            .lock()
            .unwrap()
            .retain(|(route, _), _| !gone(*route));
        let mut calls = self.calls.lock().unwrap();
        let detached: Vec<_> = calls
            .iter_mut()
            .filter_map(|((route, _), call)| {
                if !call.shell || call.detached || !gone(*route) {
                    return None;
                }
                let (done, receipt) = oneshot::channel();
                call.detached = true;
                call.detach_ready = Some(receipt);
                Some((call.root.clone(), call.key.clone(), done))
            })
            .collect();
        drop(calls);
        for (root, key, done) in detached {
            let executor = executor.clone();
            tasks.spawn(async move {
                let rx = executor.submit_async(
                    root,
                    Lane::Mutating,
                    "ledger-detach".into(),
                    Box::new(move |ctx| {
                        detach_shell_row(ctx, &key);
                        Response::success("ledger-detach", json!({}))
                    }),
                );
                let _ = await_executor_response(rx, "ledger-detach".into()).await;
                let _ = done.send(());
            });
        }
    }
}
