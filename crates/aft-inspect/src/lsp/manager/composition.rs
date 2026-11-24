use super::LspManager;

impl LspManager {
    #[doc(hidden)]
    pub fn resume_for_composition(&self) {
        self.resume();
    }
}

#[cfg(any(test, feature = "test-support"))]
impl super::ApplicabilityWalk {
    #[doc(hidden)]
    pub fn has_candidate_for_test(&self, kind: &super::ServerKind) -> bool {
        self.candidates
            .iter()
            .any(|candidate| &candidate.key.kind == kind)
    }
}
