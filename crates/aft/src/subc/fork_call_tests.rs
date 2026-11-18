use serde_json::{json, Value};

use super::*;
use crate::access_policy::{ask_route_guard, AskRoute};
use crate::subc::tool_provider::{self, CatalogPreset};

/// The role's published fork-call samples.
fn vectors() -> Value {
    let (_, text) = cortexkit_role_tool_provider::vectors::FILES
        .iter()
        .find(|(file, _)| *file == "fork-calls.json")
        .expect("the role ships its fork-call samples");
    serde_json::from_str(text).unwrap()
}

#[test]
fn every_catalog_marks_exactly_the_read_only_tools() {
    for preset in CatalogPreset::ALL {
        for (runon, revision_reads) in [(false, false), (true, false), (false, true)] {
            for tool in tool_provider::tools_for_session(preset, &[], true, runon, revision_reads) {
                assert_eq!(
                    tool.read_only,
                    READ_ONLY_TOOLS.contains(&tool.name.as_str()),
                    "{} in the {} preset",
                    tool.name,
                    preset.name()
                );
            }
        }
        // The served reply spells some tools differently per preset; the mark
        // follows the tool, and `false` is never sent.
        let reply = tool_provider::catalog(json!({"preset": preset.name()}), &[], true).unwrap();
        for tool in reply["tools"].as_array().unwrap() {
            let name = tool["name"].as_str().unwrap();
            let expected = read_only(name).then_some(&Value::Bool(true));
            assert_eq!(tool.get("read_only"), expected, "{name}");
        }
    }
    let head: Vec<String> =
        tool_provider::tools_for_session(CatalogPreset::Head, &[], true, false, false)
            .into_iter()
            .filter(|tool| tool.read_only)
            .map(|tool| tool.name)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
    let mut expected = READ_ONLY_TOOLS.to_vec();
    expected.sort_unstable();
    assert_eq!(head, expected);
}

#[test]
fn plugin_spellings_of_marked_tools_are_read_only_and_nothing_else_is() {
    for name in [
        "read",
        "aft_search",
        "aft_outline",
        "aft_zoom",
        "aft_callgraph",
        "ast_grep_search",
        "aft_conflicts",
    ] {
        assert!(read_only(name), "{name}");
    }
    for name in [
        "inspect",
        "aft_inspect",
        "safety",
        "aft_safety",
        "edit",
        "write",
        "apply_patch",
        "bash",
        "bash_status",
        "ast_replace",
        "ast_grep_replace",
        "delete",
        "unknown",
    ] {
        assert!(!read_only(name), "{name}");
    }
}

#[test]
fn admission_follows_the_published_samples() {
    let vectors = vectors();
    assert_eq!(admit(&vectors["call"]).unwrap(), Some("read".to_owned()));
    let unmarked = &vectors["refusals"][0];
    let refused = admit(&unmarked["call"]).unwrap_err();
    assert_eq!(serde_json::to_value(&refused).unwrap(), unmarked["error"]);
    for spelling in ["aft_outline", "ast_grep_search"] {
        let call = json!({"name": spelling, "arguments": {}, "fork_id": "fork:1"});
        assert_eq!(admit(&call).unwrap().as_deref(), Some(spelling));
    }
    let refused =
        admit(&json!({"name": "aft_inspect", "arguments": {}, "fork_id": "fork:1"})).unwrap_err();
    assert_eq!(
        refused.detail,
        Some(json!({"tool": "aft_inspect", "reason": "not_read_only"}))
    );
    for malformed in [json!(null), json!(7), json!(""), json!("has space")] {
        let call = json!({"name": "read", "arguments": {}, "fork_id": malformed});
        let refused = admit(&call).unwrap_err();
        assert_eq!(errors::invalid_request_field(&refused), Some("fork_id"));
    }
}

#[test]
fn requests_without_a_fork_call_are_left_alone() {
    for body in [
        json!({"name": "edit", "arguments": {}}),
        json!({"op": "tool.call", "name": "bash", "arguments": {}}),
        // Only tool calls carry the marker; other operations ignore it.
        json!({"op": "tool.catalog", "fork_id": "not valid"}),
        json!({"name": "tool.withdraw", "fork_id": 7}),
    ] {
        assert_eq!(admit(&body).unwrap(), None, "{body}");
    }
}

#[test]
fn only_routes_that_could_ask_become_fork_routes() {
    let owner = std::sync::Arc::new(());
    let asking = [
        AskRoute::Plugin {
            scope: crate::access_policy::plugin::Scope::new(&owner, "head"),
            once: Vec::new(),
        },
        AskRoute::Elicitation { once: Vec::new() },
        AskRoute::Consent {
            once: Vec::new(),
            launch: None,
        },
    ];
    for route in asking {
        assert_eq!(
            ask_route(Some("read"), route.clone()),
            AskRoute::Fork {
                tool: "read".into()
            }
        );
        assert_eq!(ask_route(None, route.clone()), route);
    }
    for route in [AskRoute::Lowered, AskRoute::Deny, AskRoute::Unavailable] {
        assert_eq!(ask_route(Some("read"), route.clone()), route);
    }
}

/// The real access check: a read the plugin route would ask about is refused
/// on a fork route with the published refusal, while reads that need no
/// question still run.
#[test]
fn a_fork_route_refuses_what_would_ask_and_allows_the_rest() {
    let dir = tempfile::tempdir_in(crate::test_support::manifest_dir()).unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("root");
    let outside = base.join("outside");
    for path in [&root, &outside] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::write(root.join("inside.txt"), "inside").unwrap();
    std::fs::write(outside.join("protected.txt"), "outside").unwrap();
    let mut config = crate::config_resolve::resolve_config(&[crate::config_resolve::ConfigTier {
        tier: "user".into(),
        source: "/fixture/user/aft.jsonc".into(),
        doc: "{}".into(),
    }])
    .config;
    config.project_root = Some(root.clone());
    config.storage_dir = Some(base.join("store"));
    config.backup.enabled = Some(false);
    let ctx = crate::context::AppContext::new(Box::new(crate::language::StubProvider), config);
    let outside_read = |ctx: &crate::context::AppContext| {
        ctx.validate_read_path("fork", "head", &outside.join("protected.txt"))
    };

    let owner = std::sync::Arc::new(());
    {
        let _route = ask_route_guard(Some(AskRoute::Plugin {
            scope: crate::access_policy::plugin::Scope::new(&owner, "head"),
            once: Vec::new(),
        }));
        let asked = outside_read(&ctx).unwrap_err();
        assert!(
            crate::access_policy::is_access_request(&asked),
            "the probe must be one the plugin route asks about: {:?}",
            asked.data
        );
    }

    let _route = ask_route_guard(Some(AskRoute::Fork {
        tool: "read".into(),
    }));
    let refused = outside_read(&ctx).unwrap_err();
    assert!(crate::access_policy::is_access_request(&refused));
    let error = reply_refusal(&refused).expect("a fork refusal");
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        vectors()["refusals"][1]["error"]
    );
    assert!(ctx
        .validate_read_path("fork", "head", &root.join("inside.txt"))
        .is_ok());
}

#[test]
fn only_fork_refusals_become_role_errors() {
    let other = crate::protocol::Response::error("id", "path_outside_root", "outside");
    assert_eq!(reply_refusal(&other), None);
    let success = crate::protocol::Response::success("id", json!({"code": "fork_call_refused"}));
    assert_eq!(reply_refusal(&success), None);
}
