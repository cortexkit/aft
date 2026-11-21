#[path = "../gh_shim_relay/bindings_fixture.rs"]
mod fixture;
use fixture::files;
pub use fixture::*;
use serde_json::Value;

#[test]
fn binding_provenance_pins_all_nineteen_upstream_files() {
    let files = files();
    let provenance =
        include_str!("../../../aft/tests/fixtures/gh_shim/plexus-bindings-contract/PROVENANCE.md");
    assert!(provenance.contains("https://github.com/cortexkit/plexus"));
    assert!(
        provenance.contains("crates/plexus-core/tests/fixtures/gh_shim_wire/bindings-contract/")
    );
    assert_eq!(files.len(), CASES.len() * 2 + 1);
    for (name, bytes) in files {
        verify_bytes(name, bytes).unwrap();
    }
}

#[test]
fn binding_provenance_rejects_a_one_byte_vector_change() {
    let (name, bytes) = files()
        .into_iter()
        .find(|(name, _)| *name == "agent.reply.json")
        .unwrap();
    verify_bytes(name, bytes).unwrap();
    let mut changed = bytes.to_vec();
    assert_eq!(changed.pop(), Some(b'\n'));
    changed.push(b' ');
    assert_eq!(
        serde_json::from_slice::<Value>(bytes).unwrap(),
        serde_json::from_slice::<Value>(&changed).unwrap()
    );
    assert!(verify_bytes(name, &changed)
        .unwrap_err()
        .contains("digest mismatch"));
}

#[test]
fn binding_stand_in_replays_every_real_handler_vector() {
    for name in CASES {
        let (request, reply) = vector(name);
        assert_eq!(
            stand_in_reply(&request, "o/x", "agent-x", "app-x", 3, name != "no-grant"),
            Some(reply),
            "{name}"
        );
    }
}
