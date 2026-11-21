//! Compile membership checks over captured MSBuild documents, without evaluating MSBuild.

use std::collections::{BTreeSet, HashMap, HashSet};

use super::csharp_projects::{normalize_inside, parent, UNKNOWN};

const MAX_IMPORT_DEPTH: usize = 16;
const MAX_IMPORTED_FILES: usize = 256;

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Markers<'a> {
    pub kind: u8,
    membership_safe: bool,
    entries: &'a str,
}

impl<'a> Markers<'a> {
    pub fn decode(bytes: Option<&'a [u8]>) -> Self {
        let Some([kind, safe, rest @ ..]) = bytes else {
            return Self::default();
        };
        let Ok(entries) = std::str::from_utf8(rest) else {
            return Self::default();
        };
        Self {
            kind: *kind,
            membership_safe: *safe == 1,
            entries,
        }
    }

    pub fn references(self) -> impl Iterator<Item = &'a str> {
        self.entries
            .lines()
            .filter_map(|line| line.strip_prefix("R\t"))
    }

    fn imports(self) -> impl Iterator<Item = &'a str> {
        self.entries
            .lines()
            .filter_map(|line| line.strip_prefix("I\t"))
    }
}

pub(super) const DIRECTORY_INPUTS: [&str; 2] = ["Directory.Build.props", "Directory.Build.targets"];

#[derive(Debug, Default)]
pub(super) struct Membership {
    pub known: bool,
    /// Paths consulted through `<Import>`. A missing import is recorded so
    /// adding that file refreshes the callers whose project proof failed.
    pub imports: BTreeSet<String>,
}

pub(super) fn project(path: &str, files: &HashMap<&str, Markers<'_>>) -> Membership {
    let mut check = Check {
        files,
        completed: HashSet::new(),
        active: HashSet::new(),
        imports: BTreeSet::new(),
    };
    let known = check.document(path, 0)
        && ancestors(parent(path)).all(|folder| {
            DIRECTORY_INPUTS.iter().all(|name| {
                let path = if folder.is_empty() {
                    name.to_string()
                } else {
                    format!("{folder}/{name}")
                };
                !files.contains_key(path.as_str()) || check.document(&path, 0)
            })
        });
    Membership {
        known,
        imports: check.imports,
    }
}

pub(super) fn ancestors(folder: &str) -> impl Iterator<Item = &str> {
    std::iter::successors(Some(folder), |dir| (!dir.is_empty()).then(|| parent(dir)))
}

struct Check<'a, 's> {
    files: &'a HashMap<&'s str, Markers<'s>>,
    completed: HashSet<&'s str>,
    active: HashSet<&'s str>,
    imports: BTreeSet<String>,
}

impl Check<'_, '_> {
    fn document(&mut self, path: &str, depth: usize) -> bool {
        let Some((&key, markers)) = self.files.get_key_value(path) else {
            return false;
        };
        if depth > MAX_IMPORT_DEPTH || markers.kind == UNKNOWN || !markers.membership_safe {
            return false;
        }
        if self.completed.contains(key) {
            return true;
        }
        if self.completed.len() + self.active.len() >= MAX_IMPORTED_FILES
            || !self.active.insert(key)
        {
            return false;
        }
        let safe = markers.imports().all(|written| {
            // The checkout scan only inspects literal import paths; it
            // cannot expand variables, lists of items, or wildcards.
            if written.contains(['$', '%', '@', '*', '?', ';']) {
                return true;
            }
            let include = written.replace('\\', "/");
            let Some(target) = normalize_inside(parent(path), &include) else {
                return false;
            };
            self.imports.insert(target.clone());
            self.document(&target, depth + 1)
        });
        self.active.remove(key);
        if safe {
            self.completed.insert(key);
        }
        safe
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::callgraph_store::join::dispatch::CSharpProjects;

    fn check(documents: &[(String, String)]) -> bool {
        let bytes = documents
            .iter()
            .map(|(path, source)| {
                (
                    path.as_str(),
                    CSharpProjects::project_markers(source.as_bytes()),
                )
            })
            .collect::<Vec<_>>();
        let facts = bytes
            .iter()
            .map(|(path, data)| (*path, Markers::decode(Some(data))))
            .collect();
        project("App.csproj", &facts).known
    }

    #[test]
    fn csharp_membership_import_walk_refuses_to_exceed_its_bounds() {
        let chain = |length| {
            let mut paths = vec!["App.csproj".to_string()];
            paths.extend((1..=length).map(|i| format!("config{i}.props")));
            paths
                .iter()
                .enumerate()
                .map(|(i, path)| {
                    let xml = paths.get(i + 1).map_or_else(
                        || "<Project />".into(),
                        |target| format!("<Project><Import Project=\"{target}\" /></Project>"),
                    );
                    (path.clone(), xml)
                })
                .collect::<Vec<_>>()
        };
        assert!(check(&chain(MAX_IMPORT_DEPTH)));
        assert!(!check(&chain(MAX_IMPORT_DEPTH + 1)));

        let fan_out = |count| {
            let mut files = vec![("App.csproj".into(), "<Project>".to_string())];
            for i in 0..count {
                let path = format!("import{i}.props");
                files[0]
                    .1
                    .push_str(&format!("<Import Project=\"{path}\" />"));
                files.push((path, "<Project />".into()));
            }
            files[0].1.push_str("</Project>");
            files
        };
        assert!(check(&fan_out(MAX_IMPORTED_FILES - 1)));
        assert!(!check(&fan_out(MAX_IMPORTED_FILES)));
    }
}
