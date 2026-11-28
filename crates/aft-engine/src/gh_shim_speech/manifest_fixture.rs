pub(super) const TEST_SEED: [u8; 32] = [
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
];

pub(super) fn fixture_manifest() -> Manifest {
    serde_json::from_str(include_str!("fixtures/initial-manifest-v1.json"))
        .expect("initial manifest fixture")
}

pub(super) fn signed_with(
    manifest: &Manifest,
    fetched_at_unix_secs: u64,
    seed: &[u8; 32],
    key_id: &str,
) -> SignedManifest {
    let key = Ed25519KeyPair::from_seed_unchecked(seed).expect("test key");
    let bytes = serde_json::to_vec(manifest).expect("manifest bytes");
    SignedManifest {
        artifact_id: MANIFEST_ARTIFACT_ID.to_string(),
        envelope_version: ENVELOPE_VERSION,
        key_id: key_id.to_string(),
        fetched_at_unix_secs,
        signature: base64::engine::general_purpose::STANDARD.encode(key.sign(&bytes).as_ref()),
        manifest_bytes: String::from_utf8(bytes).expect("manifest bytes are UTF-8"),
    }
}

const DEV_MANIFEST_KEY_ID: &str = "21fe31dfa154a261";

pub(super) fn pin_for(seed: &[u8; 32]) -> trust_root::Pin {
    let key = Ed25519KeyPair::from_seed_unchecked(seed).unwrap();
    trust_root::Pin {
        key_id: trust_root::key_id(key.public_key().as_ref()),
        public_key_hex: key
            .public_key()
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
        retired_key_ids: Vec::new(),
    }
}

pub(super) fn fixture_pin() -> trust_root::Pin {
    pin_for(&TEST_SEED)
}

fn publish_pin(paths: &StatePaths, pin: Option<trust_root::Pin>) {
    trust_root::publish(
        paths,
        trust_root::Reply::Root { pin, generation: 1 },
        unix_seconds(),
    )
    .unwrap();
}

pub(super) fn write_fixture_pin(paths: &StatePaths) {
    publish_pin(paths, Some(fixture_pin()));
}

pub(super) fn signed(manifest: &Manifest, fetched_at_unix_secs: u64) -> SignedManifest {
    signed_with(
        manifest,
        fetched_at_unix_secs,
        &TEST_SEED,
        DEV_MANIFEST_KEY_ID,
    )
}

pub(super) fn write_signed_manifest(paths: &StatePaths, manifest: Manifest, now: u64) {
    write_fixture_pin(paths);
    fs::write(
        &paths.manifest,
        serde_json::to_vec(&signed(&manifest, now)).expect("signed manifest"),
    )
    .expect("manifest cache");
}
