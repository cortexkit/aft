//! Project setup for unit tests of index, cache and configure lifecycle work.

#[path = "../tests/helpers/admission.rs"]
mod shared;
pub(crate) use shared::settle;

pub(crate) fn prepare_project(ctx: &crate::context::AppContext, root: &std::path::Path) {
    assert!(crate::folder_admission_runtime::snapshot(ctx).is_none());
    let root = root.canonicalize().unwrap();
    let mut config = (*ctx.config()).clone();
    config.project_root = Some(root.clone());
    let budgets = crate::folder_admission::ProbeBudgets {
        time: std::time::Duration::from_secs(5),
        ..Default::default()
    };
    let mut decision = crate::folder_admission::probe(&root, &config.project_detection, budgets);
    if decision.classification != crate::folder_admission::FolderClassification::Project {
        crate::test_project::mark_project(&root).unwrap();
        decision = crate::folder_admission::probe(&root, &config.project_detection, budgets);
    }
    assert_eq!(
        decision.classification,
        crate::folder_admission::FolderClassification::Project,
        "project fixture was not admitted: {decision:?}"
    );
    // Seed only the classifier's decision. Index loaders and configure tail
    // stages must still start through the handler and gates under test.
    crate::folder_admission_runtime::install_standalone(ctx, config, decision);
}
