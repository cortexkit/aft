use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::client::ServerDiagnosticCapabilities;

/// Document diagnostic registrations belong to one server instance. A static
/// initialize capability is independent and must survive dynamic unregistration.
#[derive(Default)]
pub(crate) struct DiagnosticRegistrations {
    entries: HashMap<String, Registration>,
    next_generation: u64,
}

struct Registration {
    generation: u64,
    caps: ServerDiagnosticCapabilities,
    selector: Selector,
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
                self.next_generation = self.next_generation.wrapping_add(1);
                self.entries.insert(
                    id.into(),
                    Registration {
                        generation: self.next_generation,
                        caps: ServerDiagnosticCapabilities {
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
                        selector: Selector::parse(
                            options.get("documentSelector").unwrap_or(&Value::Null),
                        ),
                    },
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
        let mut entries = self.applicable(root, file);
        // Workspace routing still uses a single provider. Prefer a provider
        // that actually advertises workspace support when choosing that route.
        if file.is_none() {
            entries.sort_by_key(|registration| !registration.caps.workspace_diagnostics);
        }
        entries
            .first()
            .map(|registration| registration.caps.clone())
    }

    pub(super) fn document_providers(
        &self,
        root: &Path,
        file: &Path,
    ) -> Vec<(u64, Option<String>)> {
        let mut providers = self.applicable(root, Some(file));
        providers.dedup_by(|first, second| first.caps.identifier == second.caps.identifier);
        providers
            .into_iter()
            .map(|registration| {
                (
                    registration.generation,
                    registration.caps.identifier.clone(),
                )
            })
            .collect()
    }

    fn applicable(&self, root: &Path, file: Option<&Path>) -> Vec<&Registration> {
        let mut entries = self
            .entries
            .iter()
            .filter(|(_, registration)| {
                file.is_none_or(|file| registration.selector.matches(root, file))
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by(|(left_id, left), (right_id, right)| {
            left.caps
                .identifier
                .cmp(&right.caps.identifier)
                .then_with(|| left_id.cmp(right_id))
        });
        entries
            .into_iter()
            .map(|(_, registration)| registration)
            .collect()
    }
}

enum Selector {
    All,
    Filters(Vec<Filter>),
}

struct Filter {
    language: Option<String>,
    pattern: Option<(globset::GlobMatcher, Option<PathBuf>)>,
}

impl Selector {
    fn parse(selector: &Value) -> Self {
        if selector.is_null() {
            return Self::All;
        }
        Self::Filters(
            selector
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Filter::parse)
                .collect(),
        )
    }

    fn matches(&self, root: &Path, file: &Path) -> bool {
        match self {
            Self::All => true,
            Self::Filters(filters) => filters.iter().any(|filter| filter.matches(root, file)),
        }
    }
}

impl Filter {
    fn parse(filter: &Value) -> Option<Self> {
        let filter = filter.as_object()?;
        if filter.is_empty()
            || filter
                .keys()
                .any(|key| !matches!(key.as_str(), "language" | "scheme" | "pattern"))
            || filter
                .get("scheme")
                .is_some_and(|scheme| scheme.as_str() != Some("file"))
        {
            return None;
        }
        let language = match filter.get("language") {
            Some(language) => Some(language.as_str()?.into()),
            None => None,
        };
        let pattern = match filter.get("pattern") {
            Some(pattern) => {
                let (glob, base) = if let Some(glob) = pattern.as_str() {
                    (glob, None)
                } else {
                    let base = pattern
                        .get("baseUri")
                        .and_then(|base| {
                            base.as_str()
                                .or_else(|| base.get("uri").and_then(Value::as_str))
                        })
                        .and_then(|uri| uri.parse::<lsp_types::Uri>().ok())
                        .and_then(|uri| super::position::uri_to_path(&uri))?;
                    (pattern.get("pattern")?.as_str()?, Some(base))
                };
                let matcher = globset::GlobBuilder::new(glob)
                    .literal_separator(true)
                    .build()
                    .ok()?
                    .compile_matcher();
                Some((matcher, base))
            }
            None => None,
        };
        Some(Self { language, pattern })
    }

    fn matches(&self, root: &Path, file: &Path) -> bool {
        self.language
            .as_deref()
            .is_none_or(|language| language == super::manager::language_id_for_path(file, None))
            && self.pattern.as_ref().is_none_or(|(matcher, base)| {
                let absolute = file.to_string_lossy().replace('\\', "/");
                let relative = file
                    .strip_prefix(base.as_deref().unwrap_or(root))
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"));
                matcher.is_match(absolute) || relative.is_some_and(|p| matcher.is_match(p))
            })
    }
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
