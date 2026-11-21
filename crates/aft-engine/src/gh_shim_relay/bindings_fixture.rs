//! Test-only binding-discovery vectors produced by Plexus's real handler.

use serde_json::Value;
use sha2::{Digest, Sha256};

pub const SOURCE_COMMIT: &str = "f174b2136cfddbb19f767dda217321be0272af87";
pub const CASES: [&str; 9] = [
    "agent",
    "connection",
    "extra-field",
    "missing-agent-id",
    "no-grant",
    "non-string-agent-id",
    "none",
    "other-agent",
    "secret-extra-field",
];

macro_rules! fixture {
    ($name:literal) => {
        (
            $name,
            include_bytes!(concat!(
                "../gh_shim_speech/fixtures/plexus-bindings-contract/",
                $name
            ))
            .as_slice(),
        )
    };
}

pub(super) fn files() -> [(&'static str, &'static [u8]); 19] {
    [
        fixture!("agent.request.json"),
        fixture!("agent.reply.json"),
        fixture!("connection.request.json"),
        fixture!("connection.reply.json"),
        fixture!("extra-field.request.json"),
        fixture!("extra-field.reply.json"),
        fixture!("missing-agent-id.request.json"),
        fixture!("missing-agent-id.reply.json"),
        fixture!("no-grant.request.json"),
        fixture!("no-grant.reply.json"),
        fixture!("non-string-agent-id.request.json"),
        fixture!("non-string-agent-id.reply.json"),
        fixture!("none.request.json"),
        fixture!("none.reply.json"),
        fixture!("other-agent.request.json"),
        fixture!("other-agent.reply.json"),
        fixture!("secret-extra-field.request.json"),
        fixture!("secret-extra-field.reply.json"),
        fixture!("UPSTREAM_README.md"),
    ]
}

pub fn verify_bytes(name: &str, bytes: &[u8]) -> Result<(), String> {
    let provenance =
        include_str!("../gh_shim_speech/fixtures/plexus-bindings-contract/PROVENANCE.md");
    if !provenance.contains(SOURCE_COMMIT) {
        return Err("binding-vector source commit changed".into());
    }
    let pin = provenance
        .lines()
        .find_map(|line| {
            let mut columns = line.split('|').map(str::trim);
            columns.next()?;
            let file = columns.next()?.trim_matches('`');
            let digest = columns.next()?.trim_matches('`');
            (file == name).then_some(digest)
        })
        .ok_or_else(|| format!("missing provenance pin for {name}"))?;
    let digest = format!("{:x}", Sha256::digest(bytes));
    if pin != digest {
        return Err(format!(
            "binding-vector digest mismatch for {name}: expected {pin}, got {digest}"
        ));
    }
    Ok(())
}

pub fn vector(name: &str) -> (Value, Value) {
    let load = |suffix| {
        let wanted = format!("{name}.{suffix}.json");
        let (_, bytes) = files()
            .into_iter()
            .find(|(file, _)| *file == wanted)
            .expect("known binding vector");
        verify_bytes(&wanted, bytes).expect("binding vector must match its upstream digest");
        serde_json::from_slice(bytes).expect("upstream binding vector is JSON")
    };
    (load("request"), load("reply"))
}

pub fn agent_reply(repository: &str, agent: &str, handle: &str, generation: u64) -> Value {
    let mut reply = vector("agent").1;
    let result = &mut reply["result"];
    result["repo_binding_generation"] = generation.into();
    let row = &mut result["bindings"][0];
    row["repository"] = repository.into();
    row["agent_id"] = agent.into();
    row["app_handle_id"] = handle.into();
    reply
}

/// Substitute the test's identities into real-handler shapes, never credentials.
/// The grant is caller context: the no-grant and other-agent requests are identical.
pub fn stand_in_reply(
    arguments: &Value,
    repository: &str,
    agent: &str,
    handle: &str,
    generation: u64,
    granted: bool,
) -> Option<Value> {
    let mut selected_agent = vector("agent").0;
    selected_agent["agent_id"] = agent.into();
    let mut selected_connection = vector("connection").0;
    selected_connection["connection_id"] = format!("github-handle-{handle}").into();
    if granted && (arguments == &selected_agent || arguments == &selected_connection) {
        return Some(agent_reply(repository, agent, handle, generation));
    }
    for name in CASES {
        if name == "no-grant" {
            continue;
        }
        let (request, reply) = vector(name);
        if arguments == &request {
            if !granted && reply.get("result").is_some() {
                return Some(vector("no-grant").1);
            }
            return Some(reply);
        }
    }
    let same_shape = |expected: &Value| {
        let actual = arguments.as_object()?;
        let expected = expected.as_object()?;
        Some(
            actual.len() == expected.len()
                && actual.keys().all(|key| expected.contains_key(key))
                && actual["op"] == expected["op"],
        )
    };
    if (same_shape(&selected_agent) == Some(true) && arguments["agent_id"].is_string())
        || (same_shape(&selected_connection) == Some(true)
            && arguments["connection_id"].is_string())
    {
        let mut reply = vector("none").1;
        reply["result"]["repo_binding_generation"] = generation.into();
        return Some(reply);
    }
    None
}
