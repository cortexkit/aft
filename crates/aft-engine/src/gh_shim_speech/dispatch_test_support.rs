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
include!("manifest_fixture.rs");
