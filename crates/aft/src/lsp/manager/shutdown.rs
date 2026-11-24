use super::*;

impl LspManager {
    /// Shut down every server concurrently within one shared grace period.
    pub fn shutdown_all(&mut self) -> LspShutdownAllOutcome {
        let clients = self.take_all_clients();
        Self::shutdown_taken_clients(clients, self.child_registry.clone())
    }

    /// Shut down clients collected from every root under one process-wide deadline.
    /// The registry also terminates server process groups whose replies never arrive.
    pub fn shutdown_taken_clients(
        clients: Vec<(ServerKey, LspClient)>,
        child_registry: LspChildRegistry,
    ) -> LspShutdownAllOutcome {
        Self::shutdown_all_clients(clients, child_registry, LSP_SHUTDOWN_ALL_BUDGET, false)
    }

    /// Process exit must also reap clients detached when their last route closed.
    /// Their shutdown threads cannot be relied on after the daemon exits.
    pub(crate) fn shutdown_at_process_exit(
        clients: Vec<(ServerKey, LspClient)>,
        registry: LspChildRegistry,
    ) -> LspShutdownAllOutcome {
        Self::shutdown_all_clients(clients, registry, LSP_SHUTDOWN_ALL_BUDGET, true)
    }

    pub(super) fn shutdown_all_clients(
        clients: Vec<(ServerKey, LspClient)>,
        child_registry: LspChildRegistry,
        budget: Duration,
        include_registered: bool,
    ) -> LspShutdownAllOutcome {
        Self::shutdown_all_clients_with_wait(
            clients,
            child_registry,
            budget,
            include_registered,
            Instant::now(),
            |rx, until| rx.recv_deadline(until),
        )
    }

    // Keep deadline selection shared with deterministic tests. Only receiving
    // reports is replaceable; spawning, killing, dropping and reporting are real.
    pub(in crate::lsp) fn shutdown_all_clients_with_wait(
        clients: Vec<(ServerKey, LspClient)>,
        child_registry: LspChildRegistry,
        budget: Duration,
        include_registered: bool,
        started: Instant,
        mut receive: impl FnMut(
            &Receiver<ShutdownResult>,
            Instant,
        ) -> Result<ShutdownResult, RecvTimeoutError>,
    ) -> LspShutdownAllOutcome {
        let keys_by_pid = clients
            .iter()
            .map(|(key, client)| (client.child_pid(), key.clone()))
            .collect::<HashMap<_, _>>();
        let mut pending_processes = clients
            .iter()
            .map(|(_, client)| (client.child_pid(), client.child_process()))
            .collect::<HashMap<_, _>>();
        let mut registered_only = Vec::new();
        if include_registered {
            for process in child_registry.tracked_processes() {
                let pid = process.pid();
                if let std::collections::hash_map::Entry::Vacant(entry) =
                    pending_processes.entry(pid)
                {
                    entry.insert(process);
                    registered_only.push(pid);
                }
            }
        }
        let mut pending_pids = pending_processes.keys().copied().collect::<HashSet<_>>();
        let servers = pending_pids.len();
        let (result_tx, result_rx) = unbounded();

        for (key, mut client) in clients {
            let pid = client.child_pid();
            let result_tx = result_tx.clone();
            std::thread::spawn(move || {
                let result = client.shutdown();
                // Do the drop before reporting completion so the registry cannot
                // briefly report a reaped client as still live after this method
                // has returned its shutdown summary. The drop also waits for the
                // child, so a report means the process has been reaped.
                drop(client);
                let _ = result_tx.send((Some(key), pid, result));
            });
        }
        for pid in registered_only {
            let result_tx = result_tx.clone();
            std::thread::spawn(move || {
                // Detached clients already have a reaping worker. Registry
                // removal can precede its wait, so only process disappearance
                // confirms that the worker (or the OS) has finished reaping.
                while crate::process_identity::is_process_alive(pid) {
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = result_tx.send((None, pid, Ok(())));
            });
        }
        drop(result_tx);

        // Graceful handshakes get the budget minus the forced-termination
        // reserve; whatever is still running then is killed, and the reserve
        // is the only wait for those kills to take effect. Either way the
        // phase ends by `deadline`.
        let deadline = started + budget;
        let force_at = deadline - LSP_FORCED_TERMINATION_RESERVE.min(budget / 2);
        let mut outcome = LspShutdownAllOutcome::default();
        while !pending_pids.is_empty() {
            // Pass the absolute cutoff through to the channel: descheduling
            // before a receive must not restart a relative timeout later.
            match receive(&result_rx, force_at) {
                Ok((key, pid, result)) => {
                    if !pending_pids.remove(&pid) {
                        continue;
                    }
                    match result {
                        Ok(()) => outcome.graceful += 1,
                        Err(err) => {
                            outcome.forced += 1;
                            if let Some(key) = key {
                                Self::log_shutdown_error(&key, &err);
                            }
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }

        if !pending_pids.is_empty() {
            let pids = pending_pids.into_iter().collect::<Vec<_>>();
            outcome.forced += pids.len();
            for pid in &pids {
                if let Some(key) = keys_by_pid.get(pid) {
                    if crate::bash_background::process::is_process_alive(*pid) {
                        slog_error!(
                            "error shutting down language server {:?} at {}: timed out waiting for shutdown",
                            key.kind,
                            key.root.display()
                        );
                    }
                }
            }
            // The graceful workers are still blocked in their per-client
            // Shutdown request, so the shared registry kills the process
            // groups directly. Dispatch off the caller's path: even a kill
            // syscall or registry cleanup can stall under process-creation
            // load. A killed server closes its pipes, unblocking its worker,
            // which reaps it and reports in. Never join the dispatcher here.
            let kill_processes = pids
                .iter()
                .filter_map(|pid| pending_processes.remove(pid))
                .collect::<Vec<_>>();
            std::thread::spawn(move || {
                child_registry.force_kill_processes(&kill_processes);
            });
            let mut unreaped = pids.into_iter().collect::<HashSet<_>>();
            while !unreaped.is_empty() {
                // An expired deadline still takes a report that is already queued,
                // so a late wake-up counts every server reaped in the meantime.
                match receive(&result_rx, deadline) {
                    Ok((_, pid, _)) => {
                        unreaped.remove(&pid);
                    }
                    Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
                }
            }
            // Waiting longer would break the ceiling. The dispatcher continues
            // sending kills, and the client workers continue reaping, without
            // holding up the caller. A pending kill is not a completed reap.
            outcome.unreaped = unreaped.len();
        }

        outcome.elapsed = started.elapsed();
        slog_info!(
            "lsp shutdown_all: servers={} graceful={} killed={} unreaped={} elapsed_ms={}",
            servers,
            outcome.graceful,
            outcome.forced,
            outcome.unreaped,
            outcome.elapsed.as_millis()
        );
        outcome
    }

    /// Shut down taken clients on a detached thread named `aft-lsp-idle-reap`.
    /// Idle reapers use this so a hung Shutdown handshake cannot stall the
    /// daemon loop or hold the manager mutex.
    pub(crate) fn spawn_idle_lsp_reap(clients: Vec<(ServerKey, LspClient)>) {
        if clients.is_empty() {
            return;
        }
        let spawn_result = std::thread::Builder::new()
            .name("aft-lsp-idle-reap".into())
            .spawn(move || {
                for (key, mut client) in clients {
                    if let Err(err) = client.shutdown_for_idle_reap() {
                        Self::log_shutdown_error(&key, &err);
                    }
                }
            });
        if let Err(err) = spawn_result {
            slog_error!("failed to spawn idle LSP reap thread: {err}");
        }
    }

    pub(super) fn log_shutdown_error(key: &ServerKey, error: &LspError) {
        match error {
            LspError::Timeout(message)
                if message.contains("; did not exit within ")
                    && message.ends_with("; killed and reaped") =>
            {
                slog_warn!("LSP server shutdown {:?}: {}", key, message);
            }
            LspError::ServerDisconnected(_) => slog_info!(
                "language server {:?} at {} exited during shutdown",
                key.kind,
                key.root.display()
            ),
            _ => slog_error!("error shutting down {:?}: {}", key, error),
        }
    }
}
