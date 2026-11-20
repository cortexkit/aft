//! Project boundaries for C# type lookup. Paths are relative to one checkout.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Default)]
pub struct CSharpProjects {
    declarations: HashMap<String, Folder>,
    lookups: Mutex<HashMap<String, Arc<Lookup>>>,
}

#[derive(Debug, Clone, Copy)]
enum Folder {
    One,
    Ambiguous,
}

#[derive(Debug)]
struct Lookup {
    project: Option<Arc<str>>,
    folders: Vec<String>,
}

impl CSharpProjects {
    /// The supplied checkout paths include project files that are not parsed.
    /// Index the project filenames in one pass instead of reading directories
    /// from disk and accidentally looking beyond the checkout snapshot.
    pub fn new<'a>(paths: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let mut this = Self::default();
        for path in paths {
            if let Some(folder) = Self::manifest_folder(path) {
                this.declarations
                    .entry(folder)
                    .and_modify(|kind| *kind = Folder::Ambiguous)
                    .or_insert(Folder::One);
            }
        }
        this
    }

    pub fn manifest_folder(path: &[u8]) -> Option<String> {
        let name = path.rsplit(|&byte| byte == b'/').next()?;
        if !name.ends_with(b".csproj") {
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

    pub(super) fn owner(&self, file: &str, mut depends: impl FnMut(String)) -> Option<Arc<str>> {
        fn parent(path: &str) -> &str {
            path.rsplit_once('/').map_or("", |(dir, _)| dir)
        }
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
                        Some(Folder::One) => break Some(Arc::from(folder)),
                        Some(Folder::Ambiguous) => break None,
                        None if folder.is_empty() => break None,
                        None => folder = parent(folder),
                    }
                };
                Arc::new(Lookup { project, folders })
            }))
        };
        for folder in &lookup.folders {
            depends(Self::dependency(folder));
        }
        lookup.project.clone()
    }
}
