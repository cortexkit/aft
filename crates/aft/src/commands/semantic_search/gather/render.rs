use super::GatherWalk;
use crate::grep_executor::truncate_at_char_boundary;
use crate::protocol::Response;

const MAX_ADDED_BYTES: usize = 12 * 1024;
const MAX_NODES: usize = 12;

pub(super) fn available_bytes(response: &Response) -> usize {
    let rendered = crate::subc_format::format_response_with_context(
        "search",
        response,
        &crate::subc_format::FormatContext::default(),
    );
    // Finalization can add a status bar after the search formatter runs.
    super::super::snippet_bounds::SEARCH_MAX_OUTPUT_BYTES
        .saturating_sub(rendered.len().saturating_add(512))
        .min(MAX_ADDED_BYTES)
}

fn append(response: &mut Response, addition: &str, budget: usize) {
    if addition.len() <= budget {
        if let Some(serde_json::Value::String(text)) = response.data.get_mut("text") {
            text.push_str(addition);
        }
    }
}

pub(super) fn append_state(response: &mut Response, state: &str, budget: usize) {
    if state == "root_mismatch" {
        append(
            response,
            "\n\ngather: skipped callgraph=unavailable reason=other_project",
            budget,
        );
        return;
    }
    let state = match state {
        "building" | "suspended" | "off" | "unavailable" | "error" => state,
        "root_unavailable" => "unavailable",
        "no_seeds" => "no_seeds",
        "configuration_failure" => "configuration_failure",
        _ => "walk_failed",
    };
    append(
        response,
        &format!("\n\ngather: skipped callgraph={state}"),
        budget,
    );
}

pub(super) fn append_walk(
    response: &mut Response,
    mut walk: GatherWalk,
    budget: usize,
    has_trace: bool,
) {
    walk.kept.sort_by(|a, b| {
        a.seed_ordinal
            .cmp(&b.seed_ordinal)
            .then(a.hop_distance.cmp(&b.hop_distance))
            .then(a.id.cmp(&b.id))
    });
    let model = if walk.model.len() <= 64
        && walk
            .model
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "-_.".contains(ch))
    {
        walk.model.as_str()
    } else {
        "endpoint"
    };
    let fallback = if walk.fallback_reasons.is_empty() {
        "none".into()
    } else {
        walk.fallback_reasons
            .iter()
            .map(|(round, reason)| format!("{round}:{reason}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    let footer = |kept| {
        format!("\ngather: model={model} rounds={} considered={} kept={kept} fallback={fallback} {:.1}s context: {} gaps={} stop={}", walk.rounds, walk.considered, walk.elapsed.as_secs_f64(), if has_trace { "host trace" } else { "query only" }, walk.graph_gaps.len(), walk.stop_reason.unwrap_or("none"))
    };
    let header = |kept| {
        format!(
            "\n\nRelated code (gathered for this search, {} rounds, {kept} of {} nodes kept):\n",
            walk.rounds, walk.considered
        )
    };
    let reserve = header(MAX_NODES).len() + footer(MAX_NODES).len();
    if reserve > budget {
        let disclosure = footer(0);
        if disclosure.len() <= budget {
            append(response, &disclosure, budget);
        } else {
            append(response, "\n\ngather: skipped output_budget", budget);
        }
        return;
    }
    let mut bodies = String::new();
    let mut shown = 0;
    for node in walk.kept.iter().take(MAX_NODES) {
        let title = format!(
            "{}:{} {}: {}  · {}\n",
            node.path.display(),
            node.line,
            node.name,
            node.kind,
            node.edge
        );
        let remaining = budget.saturating_sub(reserve + bodies.len() + title.len());
        let body = node
            .body
            .lines()
            .map(|line| format!("  {line}\n"))
            .collect::<String>();
        let marker = "\n  … body cut to budget\n";
        if remaining <= marker.len() || body.is_empty() {
            break;
        }
        bodies.push_str(&title);
        if body.len() <= remaining {
            bodies.push_str(&body);
        } else {
            bodies.push_str(truncate_at_char_boundary(&body, remaining - marker.len()));
            bodies.push_str(marker);
        }
        shown += 1;
    }
    if shown == 0 {
        append(response, &footer(0), budget);
    } else {
        append(
            response,
            &format!("{}{}{}", header(shown), bodies, footer(shown)),
            budget,
        );
    }
}
