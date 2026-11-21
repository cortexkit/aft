use std::collections::HashMap;
use std::path::Path;

use serde_json::Value;

use super::client::ServerDiagnosticCapabilities;

/// Document diagnostic registrations belong to one server instance. A static
/// initialize capability is independent and must survive dynamic unregistration.
#[derive(Default)]
pub(crate) struct DiagnosticRegistrations {
    entries: HashMap<String, (ServerDiagnosticCapabilities, Value)>,
}

impl DiagnosticRegistrations {
    pub(crate) fn record(&mut self, method: &str, params: Option<&Value>) {
        let Some(params) = params else {
            return;
        };
        if method == "client/registerCapability" {
            for registration in params
                .get("registrations")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if registration.get("method").and_then(Value::as_str)
                    != Some("textDocument/diagnostic")
                {
                    continue;
                }
                let (Some(id), Some(options)) = (
                    registration.get("id").and_then(Value::as_str),
                    registration
                        .get("registerOptions")
                        .filter(|o| o.is_object()),
                ) else {
                    continue;
                };
                self.entries.insert(
                    id.into(),
                    (
                        ServerDiagnosticCapabilities {
                            pull_diagnostics: true,
                            workspace_diagnostics: options
                                .get("workspaceDiagnostics")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            identifier: options
                                .get("identifier")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            ..ServerDiagnosticCapabilities::default()
                        },
                        options
                            .get("documentSelector")
                            .cloned()
                            .unwrap_or(Value::Null),
                    ),
                );
            }
        } else if method == "client/unregisterCapability" {
            for registration in params
                .get("unregisterations")
                .or_else(|| params.get("unregistrations"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if registration.get("method").and_then(Value::as_str)
                    == Some("textDocument/diagnostic")
                {
                    if let Some(id) = registration.get("id").and_then(Value::as_str) {
                        self.entries.remove(id);
                    }
                }
            }
        }
    }

    pub(crate) fn capabilities(
        &self,
        root: &Path,
        file: Option<&Path>,
    ) -> Option<ServerDiagnosticCapabilities> {
        self.entries
            .values()
            .find(|(_, selector)| file.is_none_or(|file| selector_matches(selector, root, file)))
            .map(|(caps, _)| caps.clone())
    }
}

fn selector_matches(selector: &Value, root: &Path, file: &Path) -> bool {
    if selector.is_null() {
        return true;
    }
    selector.as_array().is_some_and(|filters| {
        filters.iter().any(|filter| {
            let Some(filter) = filter.as_object() else {
                return false;
            };
            if filter.is_empty()
                || filter
                    .keys()
                    .any(|key| !matches!(key.as_str(), "language" | "scheme" | "pattern"))
            {
                return false;
            }
            if filter.get("language").is_some_and(|language| {
                language.as_str() != Some(super::manager::language_id_for_path(file, None))
            }) {
                return false;
            }
            if filter
                .get("scheme")
                .is_some_and(|scheme| scheme.as_str() != Some("file"))
            {
                return false;
            }
            if let Some(pattern) = filter.get("pattern") {
                let (pattern, base) = if let Some(pattern) = pattern.as_str() {
                    (Some(pattern), root.to_path_buf())
                } else {
                    let base = pattern
                        .get("baseUri")
                        .and_then(|base| {
                            base.as_str()
                                .or_else(|| base.get("uri").and_then(Value::as_str))
                        })
                        .and_then(|uri| uri.parse::<lsp_types::Uri>().ok())
                        .and_then(|uri| super::position::uri_to_path(&uri));
                    let Some(base) = base else {
                        return false;
                    };
                    (pattern.get("pattern").and_then(Value::as_str), base)
                };
                let Some(matcher) = pattern
                    .and_then(|pattern| {
                        globset::GlobBuilder::new(pattern)
                            .literal_separator(true)
                            .build()
                            .ok()
                    })
                    .map(|glob| glob.compile_matcher())
                else {
                    return false;
                };
                let absolute = file.to_string_lossy().replace('\\', "/");
                let relative = file
                    .strip_prefix(base)
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"));
                if !matcher.is_match(absolute) && !relative.is_some_and(|p| matcher.is_match(p)) {
                    return false;
                }
            }
            true
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diagnostic_registration_tracks_identifier_selector_and_unregistration() {
        let mut registrations = DiagnosticRegistrations::default();
        registrations.record("client/registerCapability", Some(&json!({"registrations":[{"id":"one","method":"textDocument/diagnostic","registerOptions":{"identifier":"pyright","interFileDependencies":true,"documentSelector":[{"language":"python","scheme":"file","pattern":"**/*.py"}]}}]})));
        let root = Path::new("/workspace");
        assert_eq!(
            registrations
                .capabilities(root, Some(Path::new("/workspace/a.py")))
                .unwrap()
                .identifier
                .as_deref(),
            Some("pyright")
        );
        assert!(registrations
            .capabilities(root, Some(Path::new("/workspace/a.txt")))
            .is_none());
        registrations.record(
            "client/unregisterCapability",
            Some(&json!({"unregisterations":[{"id":"one","method":"textDocument/diagnostic"}]})),
        );
        assert!(registrations.capabilities(root, None).is_none());
    }
}
