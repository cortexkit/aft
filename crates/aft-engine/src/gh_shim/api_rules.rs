use super::{
    ApiRule, MERGE_ASYNC_PATH_GLOB, PRIVATE_REPORTING_PATH_GLOB, PULL_EDIT_PATH_GLOB,
    REVIEW_REPLY_PATH_GLOB, STACK_ADD_PATH_GLOB, STACK_CREATE_PATH_GLOB, STACK_UNSTACK_PATH_GLOB,
};

pub(super) fn api_rule_matches_path(rule: &ApiRule, path: &str) -> bool {
    if matches!(
        rule.path_glob.as_str(),
        PRIVATE_REPORTING_PATH_GLOB
            | REVIEW_REPLY_PATH_GLOB
            | PULL_EDIT_PATH_GLOB
            | STACK_CREATE_PATH_GLOB
            | STACK_ADD_PATH_GLOB
            | STACK_UNSTACK_PATH_GLOB
            | MERGE_ASYNC_PATH_GLOB
            | super::workflow_approval::PATH_GLOB
            | super::environment_admin::PUT_GLOB
            | super::environment_admin::POLICY_GLOB
    ) {
        // glob's `*` can span slashes. These reviewed endpoints need each `*`
        // to match one path segment, not a subtree.
        let pattern = rule.path_glob.split('/').collect::<Vec<_>>();
        let segments = path.split('/').collect::<Vec<_>>();
        return pattern.len() == segments.len()
            && pattern.iter().zip(&segments).all(|(expected, actual)| {
                if *expected == "*" {
                    !actual.is_empty()
                } else {
                    expected == actual
                }
            });
    }
    glob::Pattern::new(&rule.path_glob).is_ok_and(|pattern| pattern.matches(path))
}

/// Stable name for an API rule in bypass audits and governed requests.
pub(super) fn api_tuple(rule: &ApiRule) -> String {
    format!(
        "api:{}:{}",
        rule.method.to_ascii_uppercase(),
        rule.path_glob
    )
}

pub(super) fn is_api_tuple(tuple: &str) -> bool {
    tuple.starts_with("api:")
}
