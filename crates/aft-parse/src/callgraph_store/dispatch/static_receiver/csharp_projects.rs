//! Project boundaries for C# type lookup. Paths are relative to one checkout.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use super::csharp_membership::{self as membership, Markers, DIRECTORY_INPUTS};

#[derive(Debug, Default)]
pub struct CSharpProjects {
    declarations: HashMap<String, Folder>,
    membership_imports: HashMap<String, BTreeSet<String>>,
    projects: Vec<Project>,
    lookups: Mutex<HashMap<String, Arc<Lookup>>>,
}

pub(super) type ProjectId = usize;

#[derive(Debug, Clone, Copy)]
enum Folder {
    One(ProjectId),
    Ambiguous,
}

pub(super) const UNKNOWN: u8 = 0;
pub(super) const AGGREGATOR: u8 = 1;
pub(super) const COMPILES: u8 = 2;
const MAX_REACHABLE: usize = 1000;

#[derive(Debug)]
struct Project {
    path: String,
    folder: String,
    references: BTreeSet<String>,
    closure: OnceLock<Closure>,
}

#[derive(Debug, Default)]
struct Closure {
    projects: Option<HashSet<ProjectId>>,
    folders: BTreeSet<String>,
}

#[derive(Debug)]
struct Lookup {
    project: Option<ProjectId>,
    folders: Vec<String>,
}

impl CSharpProjects {
    pub const PROJECT_READ_LIMIT: u64 = 64 * 1024;
    /// Load compact project facts from the checkout snapshot. No directory or
    /// project files are reopened during resolution.
    pub fn new<'a>(paths: impl IntoIterator<Item = (&'a [u8], Option<&'a [u8]>)>) -> Self {
        let mut this = Self::default();
        let mut files = HashMap::new();
        let mut project_paths = Vec::new();
        for (path, bytes) in paths {
            let markers = Markers::decode(bytes);
            if let Some(folder) = Self::manifest_folder(path) {
                project_paths.push((path, folder, markers));
            }
            if bytes.is_some() || Self::is_membership_config_path(path) {
                if let Ok(path) = std::str::from_utf8(path) {
                    files.insert(path, markers);
                }
            }
        }
        for (path, folder, markers) in project_paths {
            if markers.kind == AGGREGATOR {
                continue;
            }
            let supported = markers.kind == COMPILES
                && std::str::from_utf8(path).ok().is_some_and(|path| {
                    let check = membership::project(path, &files);
                    this.membership_imports
                        .entry(folder.clone())
                        .or_default()
                        .extend(check.imports);
                    check.known
                });
            let entry = if supported {
                let id = this.projects.len();
                let references = markers
                    .references()
                    .filter_map(|path| reference_path(&folder, path))
                    .collect();
                this.projects.push(Project {
                    path: String::from_utf8_lossy(path).into_owned(),
                    folder: folder.clone(),
                    references,
                    closure: OnceLock::new(),
                });
                Folder::One(id)
            } else {
                Folder::Ambiguous
            };
            this.declarations
                .entry(folder)
                .and_modify(|kind| *kind = Folder::Ambiguous)
                .or_insert(entry);
        }
        this
    }

    /// Compact configuration fact from the project's own bounded XML read.
    pub fn project_markers(source: &[u8]) -> Vec<u8> {
        let Some(facts) = super::msbuild::classify(source) else {
            return vec![UNKNOWN, 0];
        };
        let mut out = vec![facts.kind, u8::from(facts.membership_safe)];
        if facts.kind == COMPILES {
            for path in facts.references {
                out.extend(b"R\t");
                out.extend(path.as_bytes());
                out.push(b'\n');
            }
        }
        for path in facts.imports {
            out.extend(b"I\t");
            out.extend(path.as_bytes());
            out.push(b'\n');
        }
        out
    }

    pub fn is_membership_config_path(path: &[u8]) -> bool {
        path.ends_with(b".csproj")
            || path
                .rsplit(|&byte| byte == b'/')
                .next()
                .is_some_and(|name| {
                    DIRECTORY_INPUTS
                        .iter()
                        .any(|input| name == input.as_bytes())
                })
    }

    pub fn has_project_root(source: &[u8]) -> bool {
        super::msbuild::has_project_root(source)
    }

    pub fn manifest_folder(path: &[u8]) -> Option<String> {
        path.ends_with(b".csproj")
            .then(|| Self::membership_folder(path))
            .flatten()
    }

    pub fn membership_folder(path: &[u8]) -> Option<String> {
        if !Self::is_membership_config_path(path) {
            return None;
        }
        let dir = path
            .iter()
            .rposition(|&byte| byte == b'/')
            .map_or(&b""[..], |end| &path[..end]);
        Some(String::from_utf8_lossy(dir).into_owned())
    }

    pub fn dependency(folder: &str) -> String {
        format!("csproject:{folder}")
    }

    pub fn import_dependency(path: &str) -> String {
        format!("csimport:{path}")
    }

    fn depend_folder(&self, folder: &str, depends: &mut impl FnMut(String)) {
        depends(Self::dependency(folder));
        if let Some(imports) = self.membership_imports.get(folder) {
            for dir in membership::ancestors(folder).skip(1) {
                depends(Self::dependency(dir));
            }
            for path in imports {
                depends(Self::import_dependency(path));
            }
        }
    }

    pub(super) fn owner(&self, file: &str, mut depends: impl FnMut(String)) -> Option<ProjectId> {
        let directory = parent(file);
        let lookup = {
            let mut lookups = self
                .lookups
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Arc::clone(lookups.entry(directory.to_string()).or_insert_with(|| {
                let mut folder = directory;
                let mut folders = Vec::new();
                let project = loop {
                    folders.push(folder.to_string());
                    match self.declarations.get(folder) {
                        Some(Folder::One(project)) => break Some(*project),
                        Some(Folder::Ambiguous) => break None,
                        None if folder.is_empty() => break None,
                        None => folder = parent(folder),
                    }
                };
                Arc::new(Lookup { project, folders })
            }))
        };
        for folder in &lookup.folders {
            self.depend_folder(folder, &mut depends);
        }
        lookup.project
    }

    pub(super) fn reachable(
        &self,
        project: ProjectId,
        mut depends: impl FnMut(String),
    ) -> Option<&HashSet<ProjectId>> {
        let closure = self.projects[project]
            .closure
            .get_or_init(|| self.walk(project));
        for folder in &closure.folders {
            self.depend_folder(folder, &mut depends);
        }
        closure.projects.as_ref()
    }

    fn walk(&self, start: ProjectId) -> Closure {
        let mut folders = BTreeSet::new();
        let mut projects = HashSet::from([start]);
        let mut pending = vec![start];
        while let Some(id) = pending.pop() {
            let project = &self.projects[id];
            folders.insert(project.folder.clone());
            for target in &project.references {
                let folder = parent(target);
                folders.insert(folder.to_string());
                if let Some(Folder::One(next)) = self.declarations.get(folder) {
                    // A different project filename in the same folder does
                    // not prove a reference to the one present today.
                    if self.projects[*next].path == *target && projects.insert(*next) {
                        if projects.len() > MAX_REACHABLE + 1 {
                            return Closure {
                                projects: None,
                                folders,
                            };
                        }
                        pending.push(*next);
                    }
                }
            }
        }
        Closure {
            projects: Some(projects),
            folders,
        }
    }
}

pub(super) fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn reference_path(folder: &str, include: &str) -> Option<String> {
    let include = include.replace('\\', "/");
    let include = include.trim();
    let include = include
        .strip_prefix("$(MSBuildThisFileDirectory)")
        .or_else(|| include.strip_prefix("$(MSBuildProjectDirectory)"))
        .map_or_else(
            || (!include.starts_with('/')).then_some(include),
            |path| Some(path.trim_start_matches('/')),
        )?;
    include
        .ends_with(".csproj")
        .then(|| normalize_inside(folder, include))
        .flatten()
}

pub(super) fn normalize_inside(folder: &str, include: &str) -> Option<String> {
    let include = include.trim();
    if include.is_empty()
        || include.starts_with('/')
        || include.contains(['$', '%', '@', '*', '?', ':', ';', '\r', '\n'])
    {
        return None;
    }
    let mut parts = folder
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    for part in include.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csharp_project_reference_cap_yields_no_proof() {
        let mut projects = (0..MAX_REACHABLE + 2).map(|i| {
            let path = format!("p{i}/p.csproj");
            let xml = format!("<Project><ItemGroup><ProjectReference Include=\"../p{}/p.csproj\" /></ItemGroup></Project>", i+1);
            (path, CSharpProjects::project_markers(xml.as_bytes()))
        }).collect::<Vec<_>>();
        let build = |projects: &[(String, Vec<u8>)]| {
            CSharpProjects::new(
                projects
                    .iter()
                    .map(|(path, config)| (path.as_bytes(), Some(config.as_slice()))),
            )
        };
        let capped = build(&projects);
        assert!(capped.reachable(0, |_| {}).is_none());
        projects.pop();
        let bounded = build(&projects);
        assert_eq!(
            bounded.reachable(0, |_| {}).unwrap().len(),
            MAX_REACHABLE + 1
        );
    }
}
