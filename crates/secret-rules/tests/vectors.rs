//! Conformance vectors: each committed JSON case runs the crate and compares
//! the whole trace. The same files run on Linux, macOS and Windows.

mod support;

use secret_rules::{
    apply_target_edits, authority_matches_connect, canonical_host, check_admissible,
    encode_for_location, evaluate_redirect, evaluate_request_with_connect, mentions_label_loose,
    normalize_request_target, parse_at, parse_exact, scan, scrub_bytes, split_absolute,
    AuditReason, AuditRecordV1, ConnectAuthority, Decision, HeaderField, LabelRule, Location,
    MaterialRefusal, RequestTarget, RequestView, ScopeRef, TargetRefusal,
};
use serde_json::{json, Map, Value};

#[test]
fn p1_placeholder_vectors() {
    run_group("secret_rules_v1.json", "P1");
}

#[test]
fn p2_location_vectors() {
    run_group("secret_rules_v1.json", "P2");
}

#[test]
fn p3_material_vectors() {
    // P3 lives in its own file so the admissibility cases can be read next to
    // the encoding ones without the location cases in between.
    run_group("secret_rules_p3.json", "P3");
}

#[test]
fn p4_ambiguity_vectors() {
    run_group("secret_rules_v1.json", "P4");
}

#[test]
fn m1_host_port_vectors() {
    run_group("secret_rules_m1.json", "M1");
}

#[test]
fn m2_path_vectors() {
    run_group("secret_rules_m2.json", "M2");
}

#[test]
fn m3_redirect_vectors() {
    run_group("secret_rules_m3.json", "M3");
}

#[test]
fn m4_authority_vectors() {
    run_group("secret_rules_m4.json", "M4");
}

#[test]
fn s1_encoding_vectors() {
    run_group("secret_rules_s1.json", "S1");
}

#[test]
fn s2_streaming_vectors() {
    run_group("secret_rules_s2.json", "S2");
}

#[test]
fn a1_audit_vectors() {
    run_group("secret_rules_a1.json", "A1");
}

#[test]
fn every_acceptance_group_has_vectors() {
    support::check_groups_present();
}

fn run_group(file: &str, group: &str) {
    let mut failures = Vec::new();
    let mut total = 0usize;
    if let Err(err) = support::require_group(file, group) {
        panic!("{err}");
    }
    support::run_group(file, group, &mut run, &mut failures, &mut total);
    assert!(
        total > 0,
        "group {group} ran zero cases, which is not a pass"
    );
    assert!(
        failures.is_empty(),
        "{} of {total} {group} cases failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn run(case: &Value) -> Result<Value, String> {
    match case.get("kind").and_then(Value::as_str) {
        Some("parse") => Ok(run_parse(case)),
        Some("scan") => Ok(run_scan(case)),
        Some("request") => run_request(case),
        Some("material") => run_material(case),
        Some("path") => run_path(case),
        Some("host") => run_host(case),
        Some("redirect") => run_redirect(case),
        Some("authority") => run_authority(case),
        Some("scrub") => run_scrub(case),
        Some("scrub_stream") => run_scrub_stream(case),
        Some("audit") => run_audit(case),
        other => Err(format!("unknown case kind {other:?}")),
    }
}

fn run_parse(case: &Value) -> Value {
    let offset = case["offset"].as_u64().unwrap_or(0) as usize;
    let input = support::bytes(&case["input"]);
    let parsed = parse_at(&input, offset);
    let exact = std::str::from_utf8(&input)
        .ok()
        .and_then(parse_exact)
        .map(|placeholder| placeholder.label.to_string());
    json!({
        "label": parsed.map(|placeholder| placeholder.label.to_string()),
        "len": parsed.map(|placeholder| placeholder.end()),
        "exact": exact,
    })
}

fn run_scan(case: &Value) -> Value {
    let input = support::string(&case["input"]);
    let labels: Vec<String> = scan(&input)
        .into_iter()
        .map(|placeholder| placeholder.label.to_string())
        .collect();
    let mut mentions = Map::new();
    for label in case["mentions"].as_array().cloned().unwrap_or_default() {
        let label = label.as_str().unwrap_or_default();
        mentions.insert(
            label.to_string(),
            json!(mentions_label_loose(&input, label)),
        );
    }
    json!({ "labels": labels, "mentions": mentions })
}

fn case_rule(value: &Value) -> Result<LabelRule, String> {
    let location = match value.get("location").and_then(Value::as_object) {
        Some(map) => match map.get("kind").and_then(Value::as_str) {
            Some("bearer") => Location::Bearer,
            Some("header") => Location::Header(support::string(&map["name"])),
            Some("query") => Location::Query(support::string(&map["name"])),
            Some("json") => Location::Json(support::string(&map["pointer"])),
            other => return Err(format!("bad location kind {other:?}")),
        },
        None => return Err("rule has no location".to_string()),
    };
    Ok(LabelRule {
        label: support::string(&value["label"]),
        host: value
            .get("host")
            .map(support::string)
            .unwrap_or_else(|| "api.example.com".to_string()),
        port: value.get("port").and_then(Value::as_u64).unwrap_or(443) as u16,
        path_prefix: value
            .get("path_prefix")
            .map(support::string)
            .unwrap_or_else(|| "/v1".to_string()),
        location,
        grant_id: "grant".to_string(),
    })
}

fn run_request(case: &Value) -> Result<Value, String> {
    let key = support::key_bytes(case);
    let mut rules = Vec::new();
    for rule in case["rules"].as_array().cloned().unwrap_or_default() {
        rules.push(case_rule(&rule)?);
    }
    let body = support::bytes(&case["body"]);
    let mut headers = Vec::new();
    for header in case["headers"].as_array().cloned().unwrap_or_default() {
        let name = support::string(&header["name"]);
        headers.push(HeaderField::value(
            Box::leak(name.into_boxed_str()),
            Box::leak(support::bytes(&header["value"]).into_boxed_slice()),
        ));
    }
    let target_text = support::string(&case["target"]);
    let target = if let Some(url) = case.get("url").and_then(Value::as_str) {
        let (scheme, host, port, target_form) =
            split_absolute(url).map_err(|err| format!("{err:?}"))?;
        RequestTarget {
            host,
            port,
            scheme: Box::leak(scheme.into_boxed_str()),
            target: Box::leak(target_form.into_boxed_str()),
        }
    } else {
        RequestTarget {
            host: case
                .get("host")
                .map(support::string)
                .unwrap_or_else(|| "api.example.com".to_string()),
            port: case.get("port").and_then(Value::as_u64).unwrap_or(443) as u16,
            scheme: "https",
            target: Box::leak(target_text.into_boxed_str()),
        }
    };
    let request = RequestView {
        target: target.target,
        headers,
        header_bytes: &[],
        body: Box::leak(body.into_boxed_slice()),
    };
    let connect = ConnectAuthority::parse(&format!("{}:{}", target.host, target.port))
        .map_err(|err| format!("{err:?}"))?;
    // One key per label. A case may spell the keys out; otherwise every rule
    // gets the case's single key.
    let explicit = case["keys"].as_array().cloned().unwrap_or_default();
    let keys: Vec<(String, Vec<u8>)> = if explicit.is_empty() {
        rules
            .iter()
            .map(|rule| (rule.label.clone(), key.clone()))
            .collect()
    } else {
        explicit
            .iter()
            .map(|entry| (support::string(&entry["label"]), support::key_bytes(entry)))
            .collect()
    };
    let outcome = evaluate_request_with_connect(&request, &connect, &rules, &keys);
    let mut arms = Vec::new();
    for label in outcome.labels.iter() {
        arms.push(match label {
            secret_rules::LabelOutcome::Substituted { .. } => "substituted".to_string(),
            secret_rules::LabelOutcome::NotWholeValue => "not_whole".to_string(),
            secret_rules::LabelOutcome::OutOfScope => "literal".to_string(),
            secret_rules::LabelOutcome::Refused(refusal) => {
                format!("refused:{}", refusal.reason())
            }
        });
    }
    let mut out = Map::new();
    out.insert("labels".to_string(), json!(arms));
    out.insert(
        "refusal".to_string(),
        json!(outcome.refusal.map(|refusal| refusal.reason().to_string())),
    );
    // Header edits are part of the trace: a wrong replacement byte or offset in
    // a header has to fail the vectors, not slip past the comparison.
    if !outcome.header_edits.is_empty() {
        let edits: Vec<Value> = outcome
            .header_edits
            .iter()
            .map(|edit| {
                // A borrowed value has no offsets into a request buffer; the
                // replacement text is what the vectors check in that case.
                json!({
                    "name": edit.name,
                    "replacement": support::lossy(&edit.replacement),
                    "kind": match edit.kind {
                        secret_rules::HeaderEditKind::WholeValue => "whole_value",
                        secret_rules::HeaderEditKind::BearerCredential => "bearer_credential",
                    },
                })
            })
            .collect();
        out.insert("header_edits".to_string(), json!(edits));
    }
    match &outcome.rewritten_body {
        Some(body) => {
            out.insert("body_out".to_string(), json!(support::lossy(body)));
        }
        None => {
            if !request.body.is_empty() {
                out.insert("body_out".to_string(), json!(support::lossy(request.body)));
            }
        }
    }
    if !outcome.target_edits.is_empty() {
        out.insert(
            "query".to_string(),
            json!(apply_target_edits(target.target, &outcome.target_edits)),
        );
    }
    Ok(Value::Object(out))
}

fn run_material(case: &Value) -> Result<Value, String> {
    let key = support::key_bytes(case);
    let label = support::string(&case["label"]);
    let location = match support::string(&case["location"]).as_str() {
        "bearer" => Location::Bearer,
        "header" => Location::Header("X".to_string()),
        "query" => Location::Query("k".to_string()),
        "json" => Location::Json("/k".to_string()),
        other => return Err(format!("bad material location {other}")),
    };
    let admissible = check_admissible(&key, &label, &location);
    let encoded = encode_for_location(&key, &label, &location);
    Ok(json!({
        "admissible": match &admissible {
            Ok(()) => json!(true),
            Err(_) => json!(false),
        },
        "encoded": encoded.as_ref().ok().map(|bytes| support::lossy(bytes)),
        "error": encoded.as_ref().err().map(material_reason),
    }))
}

fn material_reason(err: &MaterialRefusal) -> String {
    match err {
        MaterialRefusal::Empty => "empty",
        MaterialRefusal::TooShort => "too_short",
        MaterialRefusal::SubstringOfPlaceholder => "in_placeholder",
        MaterialRefusal::NonVisibleHeaderByte => "not_visible",
    }
    .to_string()
}

fn target_reason(err: &TargetRefusal) -> String {
    match err {
        TargetRefusal::SchemeNotHttps => "scheme_not_https",
        TargetRefusal::HostMismatch => "host_mismatch",
        TargetRefusal::PortMismatch => "port_mismatch",
        TargetRefusal::PathOutsidePrefix => "path_outside_prefix",
        TargetRefusal::AmbiguousPathSeparator => "ambiguous_separator",
        TargetRefusal::EmptySegment => "empty_segment",
        TargetRefusal::NotOriginForm => "not_origin_form",
        TargetRefusal::MalformedPath => "malformed_path",
        TargetRefusal::AuthorityMismatch => "authority_mismatch",
        TargetRefusal::MalformedAuthority => "malformed_authority",
        TargetRefusal::FragmentInTarget => "fragment_in_target",
    }
    .to_string()
}

fn run_path(case: &Value) -> Result<Value, String> {
    let input = support::string(&case["input"]);
    let prefix = support::string(&case["prefix"]);
    match normalize_request_target(&input) {
        Ok(path) => Ok(json!({
            "normalized": path.value,
            "wire": support::lossy(&path.wire),
            "matches": secret_rules::path_covers(&path.value, &prefix),
        })),
        Err(err) => Ok(json!({ "error": target_reason(&err) })),
    }
}

fn run_host(case: &Value) -> Result<Value, String> {
    let input = support::string(&case["input"]);
    Ok(match canonical_host(&input) {
        Ok(host) => json!({ "host": host }),
        Err(err) => json!({ "error": target_reason(&err) }),
    })
}

fn run_redirect(case: &Value) -> Result<Value, String> {
    let rules: Vec<LabelRule> = case["rules"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(case_rule)
        .collect::<Result<_, _>>()?;
    let (scheme, host, port, target) =
        split_absolute(&support::string(&case["url"])).map_err(|err| format!("{err:?}"))?;
    let hop = RequestTarget {
        host,
        port,
        scheme: &scheme,
        target: &target,
    };
    let decisions = evaluate_redirect(&rules, &hop);
    let authorized: Vec<String> = decisions
        .iter()
        .filter(|decision| decision.outcome == secret_rules::RedirectOutcome::Authorized)
        .map(|decision| decision.label.clone())
        .collect();
    let reasons: Vec<String> = decisions
        .iter()
        .filter_map(|decision| {
            decision
                .refusal
                .as_ref()
                .map(|refusal| format!("{}:{}", decision.label, refusal.reason()))
        })
        .collect();
    Ok(json!({ "authorized": authorized, "reasons": reasons }))
}

fn run_authority(case: &Value) -> Result<Value, String> {
    let connect = ConnectAuthority::parse(&support::string(&case["connect"]))
        .map_err(|err| format!("{err:?}"))?;
    let host_header = case.get("host").and_then(Value::as_str);
    let authority_header = case.get("authority").and_then(Value::as_str);
    let target = support::string(&case["target"]);
    Ok(
        match authority_matches_connect(&connect, host_header, authority_header, &target) {
            Ok(()) => json!({ "ok": true, "authority": connect.authority() }),
            Err(err) => json!({ "ok": false, "error": target_reason(&err) }),
        },
    )
}

fn run_scrub(case: &Value) -> Result<Value, String> {
    let key = support::key_bytes(case);
    let label = support::string(&case["label"]);
    let input = support::string(&case["input"]);
    let scrubbed = scrub_bytes(&key, &label, input.as_bytes()).map_err(|err| format!("{err:?}"))?;
    let text = support::lossy(&scrubbed);
    // The detector shares no code with the matcher: a representation the
    // matcher forgot cannot be forgotten here too.
    let leaked = support::discloses(&key, &scrubbed);
    Ok(json!({
        "output": text,
        "leaked": leaked,
    }))
}

fn run_scrub_stream(case: &Value) -> Result<Value, String> {
    let key = support::key_bytes(case);
    let label = support::string(&case["label"]);
    let input = support::string(&case["input"]);
    let chunk = case["chunk"].as_u64().unwrap_or(4) as usize;
    let mut scrubber =
        secret_rules::Scrubber::new(vec![
            secret_rules::MatcherSet::new(&label, &key).map_err(|err| format!("{err:?}"))?
        ])
        .map_err(|err| format!("{err:?}"))?;
    let mut out = Vec::new();
    for piece in input.as_bytes().chunks(chunk.max(1)) {
        out.extend_from_slice(&scrubber.push(piece));
    }
    out.extend_from_slice(&scrubber.finish());
    let text = support::lossy(&out);
    // A split inside an encoding must not leave that encoding behind, so the
    // independent detector checks the assembled stream.
    let leaked = support::discloses(&key, &out);
    Ok(json!({
        "output": text,
        "leaked": leaked,
        "overlap": scrubber.overlap(),
    }))
}

fn run_audit(case: &Value) -> Result<Value, String> {
    let decision = match support::string(&case["decision"]).as_str() {
        "substituted" => Decision::Substituted,
        "literal" => Decision::Literal,
        "refused" => Decision::Refused,
        other => return Err(format!("bad decision {other}")),
    };
    let reason_text = support::string(&case["reason"]);
    let reason = AuditReason::parse(&reason_text)
        .ok_or_else(|| format!("bad audit reason {reason_text:?}"))?;
    let mut record = AuditRecordV1::new(
        case["timestamp_ms"].as_u64().unwrap_or(0),
        support::string(&case["label"]),
        decision,
        reason,
        support::string(&case["request_id"]),
        support::string(&case["origin"]),
    );
    if let Some(scope) = case.get("scope").and_then(Value::as_object) {
        record = record.with_scope(Some(ScopeRef {
            owner: support::string(&scope["owner"]),
            kind: support::string(&scope["kind"]),
            reference: support::string(&scope["ref"]),
        }));
    }
    if let Some(rule) = case.get("rule").and_then(Value::as_object) {
        record = record.with_rule(
            support::string(&rule["host"]),
            rule["port"].as_u64().unwrap_or(443) as u16,
            support::string(&rule["path_prefix"]),
            support::string(&rule["location_kind"]),
        );
    }
    let key = support::key_bytes(case);
    let json = record
        .to_json_line_with_key(&key)
        .map_err(|err| format!("{err:?}"))?;
    let parsed: Value = serde_json::from_str(&json).unwrap();
    let leaked = !key.is_empty() && support::discloses(&key, json.as_bytes());
    Ok(json!({ "record": parsed, "leaked": leaked }))
}

/// The request trace of one vector case, for the harness tests.
pub fn request_trace(case: &Value) -> Result<Value, String> {
    run(case)
}
