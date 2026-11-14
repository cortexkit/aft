use super::wire::{ListParams, OutputParams, WatchParams, MAX_CHUNK_BYTES};
use serde::Deserialize;
use serde_json::Value;
use subc_protocol::{ErrorBody, Principal};

pub(super) const OPERATIONS: [&str; 4] =
    ["shells.list", "shells.watch", "shells.output", "aft.status"];

pub(in crate::subc) fn is_request(body: &Value) -> bool {
    method(body).is_some_and(|method| OPERATIONS.contains(&method))
}

fn method(body: &Value) -> Option<&str> {
    body.get("method")
        .or_else(|| body.get("op"))
        .and_then(Value::as_str)
}

// Only log known operation names, never request-supplied strings.
pub(super) fn operation(body: &Value) -> &'static str {
    match method(body) {
        Some("shells.list") => "shells.list",
        Some("shells.watch") => "shells.watch",
        Some("shells.output") => "shells.output",
        Some("aft.status") => "aft.status",
        _ => "unknown",
    }
}

pub(super) fn caller_kind(
    principal: Option<&Principal>,
    harness: Option<&str>,
    scoped: bool,
) -> &'static str {
    if !operator(principal, harness, scoped) {
        return "other";
    }
    match harness {
        Some(harness) if harness.starts_with("fed-phone:") => "phone",
        Some(harness) if harness.starts_with("fed-sim:") => "simulator",
        _ => "desktop",
    }
}

pub(super) fn log_refusal(op: &'static str, error: &ErrorBody) {
    crate::slog_info!("shells/v1 refusal: op={} code={}", op, error.code);
}

pub(in crate::subc) fn operator_refusal(body: &Value) -> ErrorBody {
    let error = operator_required();
    log_refusal(operation(body), &error);
    error
}

/// Federated agent routes must never inherit the phone operator's authority.
/// Keep this predicate shared by route admission and request dispatch.
pub(in crate::subc) fn operator(
    principal: Option<&Principal>,
    harness: Option<&str>,
    scoped: bool,
) -> bool {
    !scoped
        && (matches!(principal, Some(Principal::Direct))
            && !harness.is_some_and(crate::subc::harness_forces_untrusted)
            || matches!(principal, Some(Principal::Reserved { module_id }) if module_id == "callosum")
                && harness
                    .is_some_and(|h| h.starts_with("fed-phone:") || h.starts_with("fed-sim:")))
}

pub(in crate::subc) fn operator_required() -> ErrorBody {
    super::wire::error(
        "shells_operator_required",
        "shells operations require an operator route",
    )
}

pub(super) fn invalid(message: impl Into<String>) -> ErrorBody {
    ErrorBody::new("invalid_params", message)
}

pub(super) enum Request {
    List(ListParams),
    Watch(WatchParams),
    Output(OutputParams),
    Status { agent_ids: Option<Vec<String>> },
}

pub(super) fn admit(
    principal: Option<&Principal>,
    harness: Option<&str>,
    scoped: bool,
    body: &Value,
) -> Result<Request, ErrorBody> {
    if !operator(principal, harness, scoped) {
        return Err(operator_required());
    }
    let params = body
        .get("params")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    fn parse<T: serde::de::DeserializeOwned>(params: Value) -> Result<T, ErrorBody> {
        serde_json::from_value(params).map_err(|error| invalid(error.to_string()))
    }
    match method(body) {
        Some("shells.list") => Ok(Request::List(parse(params)?)),
        Some("shells.watch") => Ok(Request::Watch(parse(params)?)),
        Some("shells.output") => {
            let output: OutputParams = parse(params)?;
            if output.task_id.is_empty() {
                return Err(invalid("shells.output needs a task_id"));
            }
            if output
                .ansi
                .as_deref()
                .is_some_and(|ansi| !["strip", "keep"].contains(&ansi))
            {
                return Err(invalid("ansi must be strip or keep"));
            }
            if output
                .max_chunk_bytes
                .is_some_and(|bytes| bytes < 64 || bytes > MAX_CHUNK_BYTES)
            {
                return Err(invalid("max_chunk_bytes must be between 64 and 65536"));
            }
            Ok(Request::Output(output))
        }
        Some("aft.status") => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Params {
                agent_ids: Option<Vec<String>>,
            }
            let params: Params = parse(params)?;
            Ok(Request::Status {
                agent_ids: params.agent_ids,
            })
        }
        _ => Err(invalid("unknown shells operation")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_v1_operator_gate_checks_every_principal_and_stamp() {
        let direct = Principal::Direct;
        let callosum = Principal::Reserved {
            module_id: "callosum".into(),
        };
        for (principal, harness, expected) in [
            (Some(&direct), None, true),
            (Some(&direct), Some("fed:facade"), false),
            (Some(&callosum), Some("fed-phone:person"), true),
            (Some(&callosum), Some("fed-sim:person"), true),
            (Some(&callosum), Some("fed-computer:person"), false),
            (Some(&callosum), Some("fed-unknown:person"), false),
            (Some(&callosum), Some("callosum-self"), false),
            (Some(&callosum), None, false),
            (None, Some("fed-phone:person"), false),
        ] {
            assert_eq!(operator(principal, harness, false), expected);
            assert!(!operator(principal, harness, true));
            for method in OPERATIONS {
                let request = serde_json::json!({"method":method, "params":{"task_id":"bg_1"}});
                if !expected {
                    assert_eq!(
                        admit(principal, harness, false, &request)
                            .err()
                            .unwrap()
                            .code,
                        "shells_operator_required"
                    );
                }
                assert_eq!(
                    admit(principal, harness, true, &request)
                        .err()
                        .unwrap()
                        .code,
                    "shells_operator_required"
                );
            }
        }
        for principal in [
            Principal::Unverified,
            Principal::Reserved {
                module_id: "prefrontal-core".into(),
            },
            Principal::Reserved {
                module_id: "cingulate".into(),
            },
        ] {
            assert!(!operator(Some(&principal), Some("fed-phone:person"), false));
        }
    }

    #[test]
    fn shells_v1_rejects_malformed_parameters_before_registry_access() {
        for (method, params) in [
            ("shells.list", serde_json::json!({"agent_ids":7})),
            ("shells.watch", serde_json::json!({"status":"yes"})),
            ("shells.output", serde_json::json!({})),
            (
                "shells.output",
                serde_json::json!({"task_id":"bg_1","ansi":"base64"}),
            ),
            (
                "shells.output",
                serde_json::json!({"task_id":"bg_1","max_chunk_bytes":65537}),
            ),
            (
                "shells.output",
                serde_json::json!({"task_id":"bg_1","max_chunk_bytes":63}),
            ),
            ("aft.status", serde_json::json!({"agent_ids":[7]})),
        ] {
            assert_eq!(
                admit(
                    Some(&Principal::Direct),
                    None,
                    false,
                    &serde_json::json!({"method":method,"params":params})
                )
                .err()
                .unwrap()
                .code,
                "invalid_params"
            );
        }
    }
}
