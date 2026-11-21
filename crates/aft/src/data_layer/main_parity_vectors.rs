use super::{
    Checkout, FillBudget, FillFailure, Meaning, Round, SemanticBackendConfig, FILES_PER_ROUND,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

static ROOTS: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);

pub(super) fn use_constant_vectors(root: PathBuf) {
    ROOTS.lock().unwrap().insert(root);
}

pub(super) fn uses_constant_vectors(root: &Path) -> bool {
    ROOTS.lock().unwrap().contains(root)
}

impl Meaning {
    pub(super) fn fill_main_parity_vectors(
        &self,
        checkout: &Arc<Checkout>,
        config: &SemanticBackendConfig,
    ) -> Round {
        let plane = match self.plane_for(checkout, config, || {
            Ok(crate::semantic_index::SemanticIndexFingerprint::for_config_dimension(config, 2))
        }) {
            Ok(plane) => plane,
            Err(error) => return Round::Failed(FillFailure::Failed(error)),
        };
        match plane.fill_rounds(
            &mut |texts| Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect()),
            FillBudget {
                max_batch: 64,
                files_per_round: FILES_PER_ROUND,
            },
            1,
            &mut |_| {},
        ) {
            Ok(report) if report.error.is_none() && report.files_left == 0 => Round::Done,
            Ok(report) if report.error.is_none() => Round::More,
            Ok(report) => Round::Failed(FillFailure::Failed(format!("{report:?}"))),
            Err(error) => Round::Failed(FillFailure::Failed(error.to_string())),
        }
    }
}
