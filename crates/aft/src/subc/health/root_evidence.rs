use crate::context::AppContext;
use crate::data_layer::checkout::RefreshCounts;
use crate::data_layer::surfaces::DataCheckoutProbe;
use serde_json::Value;

pub(super) fn refresh_counts(ctx: &AppContext) -> Option<RefreshCounts> {
    let DataCheckoutProbe::Bound(checkout) = ctx.probe_data_checkout()? else {
        return None;
    };
    checkout
        .checkout()
        .planes()
        .callgraph
        .then(|| checkout.checkout().refresh_counts())
}

pub(super) fn health_failure_evidence(value: &Value) -> bool {
    match value {
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            key != "callgraph_refresh"
                && ((health_failure_field(key) && health_nondefault(value))
                    || health_failure_evidence(value))
        }),
        Value::Array(rows) => rows.iter().any(health_failure_evidence),
        _ => false,
    }
}

pub(super) fn health_failure_field(key: &str) -> bool {
    key.contains("fail")
        || key.contains("error")
        || key.contains("refusal")
        || matches!(key, "loss_unrecovered" | "watch_lost_races_total")
}

pub(super) fn health_nondefault(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::Array(rows) => rows.iter().any(health_nondefault),
        Value::Object(fields) => fields.values().any(health_nondefault),
        Value::String(text) => !text.is_empty(),
        Value::Bool(true) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{compact_health_root, reduce_health_root_details};
    use super::*;
    use serde_json::json;

    #[test]
    fn refresh_fallback_counters_survive_compaction_without_marking_a_root_failed() {
        let counts = json!(RefreshCounts {
            refreshes: 3,
            guard_fallbacks: 1,
            precheck_fallbacks: 0
        });
        let mut root = json!({"callgraph_refresh": counts});
        assert!(!health_failure_evidence(&root));
        compact_health_root(&mut root);
        reduce_health_root_details(&mut root);
        assert_eq!(root["callgraph_refresh"], counts);
    }
}
