use super::*;

pub struct SpeechManifest(Manifest);

impl SpeechManifest {
    pub fn new() -> Self {
        Self(fixture_manifest())
    }

    pub fn bindings(&self) -> &BTreeMap<String, String> {
        &self.0.bindings
    }

    pub fn insert_binding(&mut self, repo: String, agent: String) {
        self.0.bindings.insert(repo, agent);
    }

    pub fn bump_version(&mut self) {
        self.0.manifest_version += 1;
    }

    pub fn remove_issue_comment(&mut self) {
        for rows in self.0.tiers.values_mut() {
            rows.retain(|row| row.tuple() != "issue comment");
        }
        self.0.canonicalization.remove("issue comment");
    }

    pub fn sign_and_load(&self, root: PathBuf) {
        let paths = StatePaths::from_root(root);
        write_signed_manifest(&paths, self.0.clone(), unix_seconds());
        load_manifest(&paths, unix_seconds()).unwrap();
    }

    pub fn issue_comment_wire(
        &self,
        args: &[OsString],
        repo: &str,
        agent: &str,
    ) -> (Value, crate::gh_shim_speech::invocation::Invocation) {
        let manifest = &self.0;
        let Classification::Governed { tuple, canonical } =
            classify(args, manifest, current_platform())
        else {
            panic!("fixture must govern issue comments")
        };
        let request =
            canonicalize_governed(args, &tuple, &canonical, manifest.manifest_version).unwrap();
        let invocation = request.invocation.clone().unwrap();
        let rung = RungDetermination::r3(
            0,
            manifest.manifest_version,
            &RungRecordProvenance {
                image_path: "/aft".into(),
                version: "test".into(),
                repo_key: repo.into(),
            },
        );
        let request = governed_wire_request(&rung.record, agent, request);
        (request, invocation)
    }
}

use ring::signature::{Ed25519KeyPair, KeyPair};

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

pub(super) fn signed(manifest: &Manifest, fetched_at_unix_secs: u64) -> SignedManifest {
    let key = Ed25519KeyPair::from_seed_unchecked(&TEST_SEED).expect("test key");
    assert_eq!(key.public_key().as_ref(), DEV_MANIFEST_PUBLIC_KEY);
    signed_with(
        manifest,
        fetched_at_unix_secs,
        &TEST_SEED,
        DEV_MANIFEST_KEY_ID,
    )
}

pub(super) fn write_signed_manifest(paths: &StatePaths, manifest: Manifest, now: u64) {
    fs::create_dir_all(&paths.root).expect("state root");
    fs::write(
        &paths.manifest,
        serde_json::to_vec(&signed(&manifest, now)).expect("signed manifest"),
    )
    .expect("manifest cache");
}
