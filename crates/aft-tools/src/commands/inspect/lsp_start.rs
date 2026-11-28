use super::*;

/// Start every applicable producer at once, each bounded by the same startup
/// deadline, and return one outcome per server in `server_keys` order.
///
/// Starting them one after another made the startup budget a sum: on a busy
/// machine a project with a dozen servers spent it on the first few
/// handshakes, and every later server was reported as "startup wait budget
/// exhausted" without ever having been tried. Each start already runs its
/// spawn and `initialize` handshake without the manager lock, so starts of
/// different servers do not wait on each other.
pub(super) fn start_applicable_servers_concurrently(
    ctx: &AppContext,
    applicability: &ApplicableServerSnapshot,
    startup_deadline: Instant,
) -> Vec<ApplicableServerStartOutcomes> {
    let config = ctx.config();
    let cancellation = crate::executor::current_job_cancellation();
    let start = |server: &crate::lsp::roots::ServerKey| {
        let _cancellation = cancellation
            .clone()
            .map(crate::executor::install_job_cancellation);
        let outcome =
            ctx.lsp_start_applicable_server_until(applicability, server, &config, startup_deadline);
        if let Some(token) = &cancellation {
            token.report_progress();
        }
        outcome
    };
    std::thread::scope(|scope| {
        let handles = applicability
            .server_keys
            .iter()
            .map(|server| {
                let spawned = std::thread::Builder::new()
                    .name("aft-inspect-lsp-start".into())
                    .spawn_scoped(scope, move || start(server));
                (server, spawned)
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|(server, spawned)| match spawned {
                Ok(handle) => handle
                    .join()
                    .unwrap_or_else(|_| start_panicked_outcome(server, "server start panicked")),
                // If the thread cannot be spawned, start the server on this
                // thread instead: slower, because it no longer overlaps the
                // other starts, but the server is still started.
                Err(_) => start(server),
            })
            .collect()
    })
}

fn start_panicked_outcome(
    server: &crate::lsp::roots::ServerKey,
    reason: &str,
) -> ApplicableServerStartOutcomes {
    ApplicableServerStartOutcomes {
        failures: vec![ApplicableServerFailure {
            server_key: server.clone(),
            result: crate::lsp::manager::ServerAttemptResult::SpawnFailed {
                binary: String::new(),
                reason: reason.to_string(),
            },
        }],
        ..ApplicableServerStartOutcomes::default()
    }
}
