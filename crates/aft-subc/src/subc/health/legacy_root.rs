use subc_protocol::scope::ScopeStamp;

use super::Frame;

pub(super) fn project_root(scope: Option<&ScopeStamp>, frame: &Frame) -> String {
    scope
        .and_then(|stamp| stamp.workdir.clone())
        .or_else(|| {
            super::super::legacy_root::legacy_route_project_root(frame)
                .map(|root| root.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}
