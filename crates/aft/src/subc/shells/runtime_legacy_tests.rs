// Frozen collection path from before task sampling and store-generation caching.
// Keep this independent of the cached observation path for wire-equivalence tests.
use super::*;

pub(super) fn collect_legacy(
    executor: &Executor,
    db: Option<&std::sync::Mutex<crate::db::TrackedConnection>>,
    include_rows: bool,
    previous: DbFacts,
    revision: u64,
) -> Collected {
    #[cfg(test)]
    COLLECT_CALLS.fetch_add(1, Ordering::Relaxed);
    let contexts = executor.actor_contexts();
    let bound_roots: std::collections::BTreeSet<_> = contexts
        .iter()
        .filter_map(|ctx| {
            ctx.config()
                .project_root
                .as_ref()
                .map(|root| root.display().to_string())
        })
        .collect();
    let key = FactsKey {
        revision,
        roots: bound_roots.clone(),
        db_identity: db.map(|db| std::ptr::from_ref(db) as usize),
    };
    let dirty = previous.key.as_ref() != Some(&key);
    let mut facts = previous;
    let mut db_available = true;
    if dirty {
        if let Some(db) = db {
            if let Some(saved) = with_db_read(db, |conn| {
                crate::db::bash_tasks::operator_shell_rows(conn, now().saturating_sub(RETENTION_MS))
            }) {
                facts.saved = saved;
            } else {
                db_available = false;
            }
        }
    }
    let observe = db_available && (include_rows || dirty);
    let mut observations = Vec::new();
    let mut sources = HashMap::new();
    for ctx in &contexts {
        if observe {
            let registry = ctx.bash_background();
            for snapshot in registry.shell_snapshots() {
                if !state::retained(&project_row(&snapshot, None), now()) {
                    continue;
                }
                if let Some(reader) = registry.shell_reader(&snapshot.metadata.task_id) {
                    sources.insert(snapshot.metadata.task_id.clone(), reader);
                }
                observations.push(snapshot);
            }
        }
    }
    if observe {
        for saved in facts.saved.iter().cloned() {
            if sources.contains_key(&saved.task_id) {
                continue;
            }
            if let Some((snapshot, reader)) = ShellReader::persisted(saved) {
                if !state::retained(&project_row(&snapshot, None), now()) {
                    continue;
                }
                sources.insert(snapshot.metadata.task_id.clone(), reader);
                observations.push(snapshot);
            }
        }
    }
    if dirty && db_available {
        let scopes = observations
            .iter()
            .filter_map(|snapshot| snapshot.metadata.scope.as_ref().map(ScopeKey::from_stamp))
            .collect();
        (facts, db_available) = read_db_facts(db, &scopes, &bound_roots, facts);
        if db_available {
            facts.key = Some(key);
        }
    }
    #[cfg(test)]
    AFTER_DB_FACTS.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
    let mut rows = Vec::new();
    if include_rows && db_available {
        for snapshot in observations {
            let mut row = project_row(&snapshot, None);
            row.revision = snapshot.metadata.shell_revision;
            row.holder = resolved_holder(
                snapshot
                    .metadata
                    .scope
                    .as_ref()
                    .and_then(|scope| facts.holders.get(&ScopeKey::from_stamp(scope)))
                    .cloned(),
                &snapshot.metadata,
            );
            rows.push(row);
        }
    }
    let mut root_agents = facts.root_agents.clone();
    root_agents.retain(|root, _| bound_roots.contains(root));
    for row in &mut rows {
        let mut fingerprint_row = row.clone();
        fingerprint_row.revision = 0;
        let fingerprint = blake3::hash(&serde_json::to_vec(&fingerprint_row).unwrap())
            .to_hex()
            .to_string();
        if let Some(registry) = sources.get(&row.task_id) {
            row.revision = registry
                .shell_revision(&row.task_id, &fingerprint)
                .unwrap_or(row.revision);
        }
    }
    let mut roots = Vec::new();
    let mut degraded = false;
    let mut building = false;
    for ctx in contexts {
        let (snapshot, _) = project_status(&ctx);
        let Some(root) = snapshot["project_root"].as_str() else {
            continue;
        };
        let mut agents = root_agents.remove(root).unwrap_or_default();
        agents.sort();
        agents.dedup();
        let text_index = readiness(&snapshot["search_index"]);
        let call_graph = if snapshot["callgraph_store"].is_null()
            && snapshot["features"]["callgraph_store"] == false
        {
            Readiness {
                state: "disabled".into(),
                reason: Some("configured_disabled".into()),
            }
        } else {
            readiness(&snapshot["callgraph_store"])
        };
        let meaning = readiness(&snapshot["semantic_index"]);
        degraded |= snapshot["degraded"].as_bool().unwrap_or(false)
            || [&text_index, &call_graph, &meaning]
                .iter()
                .any(|plane| plane.state == "degraded");
        building |= [&text_index, &call_graph, &meaning]
            .iter()
            .any(|plane| plane.state == "building");
        let lsp = ctx
            .try_lsp()
            .map(|manager| manager.operator_server_states());
        let lsp = lsp.map(|servers| {
            let mut lsp: Vec<_> = servers
                .into_iter()
                .map(|(server, state, reason)| Lsp {
                    server,
                    state,
                    reason,
                })
                .collect();
            let config = ctx.config();
            for server in config.disabled_lsp.iter().chain(
                config
                    .lsp_servers
                    .iter()
                    .filter(|server| server.disabled)
                    .map(|server| &server.id),
            ) {
                lsp.retain(|entry| &entry.server != server);
                lsp.push(Lsp {
                    server: server.clone(),
                    state: "disabled".into(),
                    reason: Some("configured_disabled".into()),
                });
            }
            lsp.sort_by(|a, b| a.server.cmp(&b.server));
            lsp
        });
        roots.push(RootStatus {
            root: root.into(),
            agent_ids: agents,
            agent_ids_truncated: facts.truncated_roots.contains(root).then_some(true),
            text_index,
            call_graph,
            meaning,
            lsp,
            errors: snapshot["status_bar_values"]["errors"].as_u64(),
            warnings: snapshot["status_bar_values"]["warnings"].as_u64(),
            todos: snapshot["status_bar_values"]["todos"].as_u64(),
        });
    }
    for (root, mut agents) in root_agents {
        agents.sort();
        agents.dedup();
        let readiness = Readiness {
            state: if bound_roots.contains(&root) {
                "building"
            } else {
                "disabled"
            }
            .into(),
            reason: Some(
                if bound_roots.contains(&root) {
                    "status_busy"
                } else {
                    "root_not_bound"
                }
                .into(),
            ),
        };
        roots.push(RootStatus {
            agent_ids_truncated: facts.truncated_roots.contains(&root).then_some(true),
            root,
            agent_ids: agents,
            text_index: readiness.clone(),
            call_graph: readiness.clone(),
            meaning: readiness,
            lsp: None,
            errors: None,
            warnings: None,
            todos: None,
        });
    }
    roots.sort_by(|a, b| a.root.cmp(&b.root));
    Collected {
        rows,
        status: Status {
            aft: Aft {
                version: env!("CARGO_PKG_VERSION").into(),
                card: if degraded {
                    "degraded"
                } else if building {
                    "building"
                } else {
                    "ready"
                }
                .into(),
            },
            roots,
        },
        sources,
        db: facts,
        db_available,
        rows_refreshed: include_rows,
    }
}
