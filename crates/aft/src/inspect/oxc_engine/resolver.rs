use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use oxc_resolver::{ResolveOptions, Resolver, TsconfigDiscovery};
use rustc_hash::FxHashMap;
use serde_json::Value;

use super::types::{
    DynamicImportFact, FileFacts, FileId, ImportFact, OxcResolvedEdge, ReExportFact,
    ResolverConfigInput,
};
use crate::inspect::frameworks::{detected_route_frameworks, Framework};

const JS_MODULE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
const BUILD_OUTPUT_DIRS: &[&str] = &["dist", "build", "out", "output", "esm", "cjs"];

#[derive(Debug, Clone)]
pub struct ResolvedImport {
    pub fact: ImportFact,
    pub target: Option<FileId>,
}

#[derive(Debug, Clone)]
pub struct ResolvedReExport {
    pub fact: ReExportFact,
    pub target: Option<FileId>,
}

#[derive(Debug, Clone)]
pub struct ResolvedDynamicImport {
    pub fact: DynamicImportFact,
    pub target: Option<FileId>,
}

#[derive(Debug, Clone)]
pub struct ResolvedModule {
    pub facts: FileFacts,
    pub imports: Vec<ResolvedImport>,
    pub re_exports: Vec<ResolvedReExport>,
    pub dynamic_imports: Vec<ResolvedDynamicImport>,
}

#[derive(Debug, Default)]
pub struct ResolverConfigTracker {
    inputs: BTreeMap<PathBuf, String>,
}

impl ResolverConfigTracker {
    pub fn record_if_file(&mut self, path: &Path) {
        if !path.is_file() {
            return;
        }
        let normalized = normalize_path(path);
        if self.inputs.contains_key(&normalized) {
            return;
        }
        if let Ok(bytes) = fs::read(path) {
            self.inputs
                .insert(normalized, blake3::hash(&bytes).to_hex().to_string());
        }
    }

    pub fn inputs(&self) -> Vec<ResolverConfigInput> {
        self.inputs
            .iter()
            .map(|(path, hash)| ResolverConfigInput {
                path: path.clone(),
                content_hash: hash.clone(),
            })
            .collect()
    }

    pub fn fingerprint(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for (path, hash) in &self.inputs {
            hasher.update(path.to_string_lossy().as_bytes());
            hasher.update(b"\0");
            hasher.update(hash.as_bytes());
            hasher.update(b"\0");
        }
        hasher.finalize().to_hex().to_string()
    }
}

pub struct ModuleResolver {
    project_root: PathBuf,
    resolver: Resolver,
    path_to_id: FxHashMap<PathBuf, FileId>,
    file_set: BTreeSet<PathBuf>,
    root_package_name: Option<String>,
    sveltekit_lib_alias: bool,
}

#[derive(Debug, Clone, Default)]
struct PackageEntries {
    root: Vec<String>,
    subpaths: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Default)]
struct ResolverPassCache {
    nearest_configs: FxHashMap<(PathBuf, String), Option<PathBuf>>,
    package_entries: FxHashMap<PathBuf, Option<PackageEntries>>,
    resolutions: FxHashMap<(PathBuf, String), Option<FileId>>,
    resolved_path_ids: FxHashMap<PathBuf, Option<FileId>>,
}

impl ResolverPassCache {
    fn record_nearest_config(
        &mut self,
        tracker: &mut ResolverConfigTracker,
        start_dir: &Path,
        boundary: &Path,
        file_name: &str,
    ) {
        let key = (start_dir.to_path_buf(), file_name.to_string());
        let path = self
            .nearest_configs
            .entry(key)
            .or_insert_with(|| nearest_named_file(start_dir, boundary, file_name))
            .clone();
        if let Some(path) = path {
            tracker.record_if_file(&path);
        }
    }
}

impl ModuleResolver {
    pub fn new(project_root: &Path, files: &[PathBuf]) -> Self {
        let project_root = normalize_path(project_root);
        let mut path_to_id = FxHashMap::default();
        let mut file_set = BTreeSet::new();
        for (idx, path) in files.iter().enumerate() {
            let id = FileId(idx);
            let normalized = normalize_path(path);
            file_set.insert(normalized.clone());
            path_to_id.insert(normalized.clone(), id);
            if let Ok(canonical) = fs::canonicalize(path) {
                path_to_id.insert(normalize_path(&canonical), id);
            }
        }
        let root_package_name = package_json_name(&project_root.join("package.json"));
        let sveltekit_lib_alias = sveltekit_lib_alias_enabled(&project_root.join("package.json"));
        Self {
            project_root,
            resolver: create_resolver(),
            path_to_id,
            file_set,
            root_package_name,
            sveltekit_lib_alias,
        }
    }

    pub fn resolve_modules(
        &self,
        facts: &[FileFacts],
    ) -> (
        Vec<ResolvedModule>,
        ResolverConfigTracker,
        Vec<OxcResolvedEdge>,
    ) {
        let mut tracker = ResolverConfigTracker::default();
        let mut cache = ResolverPassCache::default();
        let mut edges = Vec::new();
        let modules = facts
            .iter()
            .map(|file_facts| {
                let imports = file_facts
                    .imports
                    .iter()
                    .map(|import| {
                        let target = self.resolve_specifier(
                            &file_facts.path,
                            &import.source,
                            &mut tracker,
                            &mut cache,
                        );
                        edges.push(OxcResolvedEdge {
                            from_file: file_facts.path.clone(),
                            specifier: import.source.clone(),
                            resolved_file: target.map(|id| facts[id.0].path.clone()),
                            kind: format!("import::{:?}", import.kind),
                            line: import.line,
                            is_type_only: import.is_type_only,
                        });
                        ResolvedImport {
                            fact: import.clone(),
                            target,
                        }
                    })
                    .collect::<Vec<_>>();
                let re_exports = file_facts
                    .re_exports
                    .iter()
                    .map(|re_export| {
                        let target = self.resolve_specifier(
                            &file_facts.path,
                            &re_export.source,
                            &mut tracker,
                            &mut cache,
                        );
                        edges.push(OxcResolvedEdge {
                            from_file: file_facts.path.clone(),
                            specifier: re_export.source.clone(),
                            resolved_file: target.map(|id| facts[id.0].path.clone()),
                            kind: format!("re_export::{:?}", re_export.kind),
                            line: re_export.line,
                            is_type_only: re_export.is_type_only,
                        });
                        ResolvedReExport {
                            fact: re_export.clone(),
                            target,
                        }
                    })
                    .collect::<Vec<_>>();
                let dynamic_imports = file_facts
                    .dynamic_imports
                    .iter()
                    .map(|dynamic| {
                        let target = dynamic.source.as_ref().and_then(|source| {
                            self.resolve_specifier(
                                &file_facts.path,
                                source,
                                &mut tracker,
                                &mut cache,
                            )
                        });
                        if let Some(source) = &dynamic.source {
                            edges.push(OxcResolvedEdge {
                                from_file: file_facts.path.clone(),
                                specifier: source.clone(),
                                resolved_file: target.map(|id| facts[id.0].path.clone()),
                                kind: "dynamic_import".to_string(),
                                line: dynamic.line,
                                is_type_only: false,
                            });
                        }
                        ResolvedDynamicImport {
                            fact: dynamic.clone(),
                            target,
                        }
                    })
                    .collect::<Vec<_>>();
                ResolvedModule {
                    facts: file_facts.clone(),
                    imports,
                    re_exports,
                    dynamic_imports,
                }
            })
            .collect::<Vec<_>>();
        (modules, tracker, edges)
    }

    fn resolve_specifier(
        &self,
        from_file: &Path,
        specifier: &str,
        tracker: &mut ResolverConfigTracker,
        cache: &mut ResolverPassCache,
    ) -> Option<FileId> {
        if is_external_builtin_or_url(specifier) {
            return None;
        }
        // Keep the resolver's path identity stable before handing paths to oxc_resolver.
        // On Windows, std::fs::canonicalize returns verbatim (`\\?\`) paths, but
        // oxc_resolver's tsconfig discovery can miss `compilerOptions.paths` aliases
        // when the issuer is verbatim even though relative imports still resolve.
        let from_file = normalize_path(from_file);
        let from_dir = from_file.parent().unwrap_or(&self.project_root);
        let cache_key = (from_dir.to_path_buf(), specifier.to_string());
        if let Some(target) = cache.resolutions.get(&cache_key) {
            return *target;
        }

        cache.record_nearest_config(tracker, from_dir, &self.project_root, "tsconfig.json");
        cache.record_nearest_config(tracker, from_dir, &self.project_root, "package.json");

        let resolved_path = self
            .resolve_with_oxc(&from_file, from_dir, specifier)
            .or_else(|| self.resolve_sveltekit_lib_fallback(specifier))
            .or_else(|| self.resolve_local_fallback(from_dir, specifier))
            .or_else(|| self.resolve_package_fallback(specifier, tracker, cache));

        let target = resolved_path.and_then(|resolved_path| {
            self.id_for_resolved_path(&resolved_path, cache)
                .or_else(|| self.id_for_build_output_remap(&resolved_path))
        });
        cache.resolutions.insert(cache_key, target);
        target
    }

    fn resolve_with_oxc(
        &self,
        from_file: &Path,
        from_dir: &Path,
        specifier: &str,
    ) -> Option<PathBuf> {
        let resolved = self
            .resolver
            .resolve_file(from_file, specifier)
            .or_else(|_| self.resolver.resolve(from_dir, specifier))
            .ok()?;
        Some(normalize_path(resolved.path()))
    }

    fn resolve_local_fallback(&self, from_dir: &Path, specifier: &str) -> Option<PathBuf> {
        if !is_relative_or_absolute(specifier) {
            return None;
        }
        let base = if specifier.starts_with('/') {
            PathBuf::from(specifier)
        } else {
            from_dir.join(specifier)
        };
        candidate_paths(&base)
            .into_iter()
            .map(|candidate| normalize_path(&candidate))
            .find(|candidate| self.file_set.contains(candidate) || candidate.is_file())
    }

    fn resolve_sveltekit_lib_fallback(&self, specifier: &str) -> Option<PathBuf> {
        if !self.sveltekit_lib_alias {
            return None;
        }
        let Some(rest) = specifier.strip_prefix("$lib") else {
            return None;
        };
        if !rest.is_empty() && !rest.starts_with('/') {
            return None;
        }
        let base = self
            .project_root
            .join("src/lib")
            .join(rest.trim_start_matches('/'));
        candidate_paths(&base)
            .into_iter()
            .map(|candidate| normalize_path(&candidate))
            .find(|candidate| self.file_set.contains(candidate) || candidate.is_file())
    }

    fn resolve_package_fallback(
        &self,
        specifier: &str,
        tracker: &mut ResolverConfigTracker,
        cache: &mut ResolverPassCache,
    ) -> Option<PathBuf> {
        let (package_name, subpath) = package_name_and_subpath(specifier)?;
        let package_dir = if self.root_package_name.as_deref() == Some(package_name.as_str()) {
            self.project_root.clone()
        } else {
            self.project_root.join("node_modules").join(&package_name)
        };
        let cached_entries = cache
            .package_entries
            .entry(package_dir.clone())
            .or_insert_with(|| {
                let package_json = package_dir.join("package.json");
                tracker.record_if_file(&package_json);
                let value = fs::read_to_string(&package_json)
                    .ok()
                    .and_then(|source| serde_json::from_str::<Value>(&source).ok())?;
                Some(collect_package_entries(&value))
            });
        let package_entries = cached_entries.as_ref()?;

        let subpath_entries;
        let entries = if let Some(subpath) = subpath {
            let key = format!("./{}", subpath.trim_start_matches('/'));
            if let Some(entries) = package_entries.subpaths.get(&key) {
                entries.as_slice()
            } else {
                subpath_entries = vec![subpath.trim_start_matches('/').to_string()];
                subpath_entries.as_slice()
            }
        } else {
            package_entries.root.as_slice()
        };

        entries
            .iter()
            .flat_map(|entry| package_entry_bases(&package_dir, entry))
            .flat_map(|base| candidate_paths(&base))
            .map(|candidate| normalize_path(&candidate))
            .find(|candidate| self.file_set.contains(candidate) || candidate.is_file())
    }

    fn id_for_resolved_path(&self, path: &Path, cache: &mut ResolverPassCache) -> Option<FileId> {
        let normalized = normalize_path(path);
        if let Some(id) = cache.resolved_path_ids.get(&normalized) {
            return *id;
        }
        let id = self.path_to_id.get(&normalized).copied().or_else(|| {
            fs::canonicalize(path)
                .ok()
                .and_then(|canonical| self.path_to_id.get(&normalize_path(&canonical)).copied())
        });
        cache.resolved_path_ids.insert(normalized, id);
        id
    }

    fn id_for_build_output_remap(&self, path: &Path) -> Option<FileId> {
        let rel = path.strip_prefix(&self.project_root).ok()?;
        let rel_str = slash_path(rel);
        let src_rel = remap_build_output_to_src(&rel_str)?;
        package_entry_bases(&self.project_root, &src_rel)
            .into_iter()
            .flat_map(|base| candidate_paths(&base))
            .map(|candidate| normalize_path(&candidate))
            .find_map(|candidate| self.path_to_id.get(&candidate).copied())
    }
}

fn create_resolver() -> Resolver {
    Resolver::new(ResolveOptions {
        extensions: vec![
            ".ts".into(),
            ".tsx".into(),
            ".js".into(),
            ".jsx".into(),
            ".mts".into(),
            ".mjs".into(),
            ".cts".into(),
            ".cjs".into(),
            ".json".into(),
        ],
        extension_alias: vec![
            (
                ".js".into(),
                vec![".ts".into(), ".tsx".into(), ".js".into()],
            ),
            (".jsx".into(), vec![".tsx".into(), ".jsx".into()]),
            (".mjs".into(), vec![".mts".into(), ".mjs".into()]),
            (".cjs".into(), vec![".cts".into(), ".cjs".into()]),
        ],
        condition_names: vec![
            "types".into(),
            "import".into(),
            "module".into(),
            "browser".into(),
            "default".into(),
        ],
        main_fields: vec!["browser".into(), "module".into(), "main".into()],
        alias_fields: vec![vec!["browser".into()]],
        tsconfig: Some(TsconfigDiscovery::Auto),
        ..Default::default()
    })
}

fn is_external_builtin_or_url(specifier: &str) -> bool {
    specifier.starts_with("node:") || specifier.starts_with("data:") || specifier.contains("://")
}

fn is_relative_or_absolute(specifier: &str) -> bool {
    specifier.starts_with("./")
        || specifier.starts_with("../")
        || specifier.starts_with('/')
        || specifier == "."
        || specifier == ".."
}

fn nearest_named_file(start_dir: &Path, boundary: &Path, file_name: &str) -> Option<PathBuf> {
    let boundary = normalize_path(boundary);
    let mut current = normalize_path(start_dir);
    loop {
        let candidate = current.join(file_name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if current == boundary || !current.starts_with(&boundary) {
            return None;
        }
        if !current.pop() {
            return None;
        }
    }
}

fn candidate_paths(base: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    push_candidate_paths(base, &mut candidates);
    if let Some(source_base) = declaration_file_source_base(base) {
        push_candidate_paths(&source_base, &mut candidates);
    }
    candidates
}

fn push_candidate_paths(base: &Path, candidates: &mut Vec<PathBuf>) {
    candidates.push(base.to_path_buf());

    let has_remappable_ext = base
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| JS_MODULE_EXTENSIONS.contains(&ext))
        .unwrap_or(false);

    if base.extension().is_none() || has_remappable_ext {
        for extension in JS_MODULE_EXTENSIONS {
            candidates.push(base.with_extension(extension));
        }
    }

    for extension in JS_MODULE_EXTENSIONS {
        candidates.push(base.join(format!("index.{extension}")));
    }
}

fn declaration_file_source_base(base: &Path) -> Option<PathBuf> {
    let file_name = base.file_name()?.to_str()?;
    for suffix in [".d.ts", ".d.mts", ".d.cts"] {
        if let Some(stem) = file_name.strip_suffix(suffix) {
            return Some(base.with_file_name(stem));
        }
    }
    None
}

fn package_json_name(package_json: &Path) -> Option<String> {
    let value = fs::read_to_string(package_json)
        .ok()
        .and_then(|source| serde_json::from_str::<Value>(&source).ok())?;
    value
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn sveltekit_lib_alias_enabled(package_json: &Path) -> bool {
    let Some(value) = fs::read_to_string(package_json)
        .ok()
        .and_then(|source| serde_json::from_str::<Value>(&source).ok())
    else {
        return false;
    };
    detected_route_frameworks(&value)
        .iter()
        .any(|framework| matches!(framework, Framework::SvelteKit | Framework::SvelteKitHooks))
}

fn package_name_and_subpath(specifier: &str) -> Option<(String, Option<String>)> {
    if is_relative_or_absolute(specifier) || is_external_builtin_or_url(specifier) {
        return None;
    }
    let mut parts = specifier.split('/');
    let first = parts.next()?.to_string();
    if first.starts_with('@') {
        let second = parts.next()?;
        let package = format!("{first}/{second}");
        let rest = parts.collect::<Vec<_>>().join("/");
        Some((package, (!rest.is_empty()).then_some(rest)))
    } else {
        let rest = parts.collect::<Vec<_>>().join("/");
        Some((first, (!rest.is_empty()).then_some(rest)))
    }
}

fn collect_package_entries(package_json: &Value) -> PackageEntries {
    let mut entries = PackageEntries::default();
    if let Some(browser) = package_json.get("browser") {
        collect_package_export_strings(browser, &mut entries.root);
    }
    if let Some(module) = package_json.get("module").and_then(Value::as_str) {
        entries.root.push(module.to_string());
    }
    if let Some(main) = package_json.get("main").and_then(Value::as_str) {
        entries.root.push(main.to_string());
    }
    if let Some(exports) = package_json.get("exports") {
        collect_package_exports(exports, &mut entries);
    }
    entries
}

fn collect_package_exports(value: &Value, entries: &mut PackageEntries) {
    match value {
        Value::Object(map) if map.keys().any(|key| key.starts_with('.')) => {
            for (key, value) in map {
                if key == "." {
                    collect_package_export_strings(value, &mut entries.root);
                } else if key.starts_with("./") {
                    collect_package_export_strings(
                        value,
                        entries.subpaths.entry(key.clone()).or_default(),
                    );
                }
            }
        }
        _ => collect_package_export_strings(value, &mut entries.root),
    }
}

fn collect_package_export_strings(value: &Value, entries: &mut Vec<String>) {
    match value {
        Value::String(entry) => entries.push(entry.clone()),
        Value::Array(values) => {
            for value in values {
                collect_package_export_strings(value, entries);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_package_export_strings(value, entries);
            }
        }
        _ => {}
    }
}

fn package_entry_bases(package_dir: &Path, entry: &str) -> Vec<PathBuf> {
    if entry.starts_with("node:") || entry.contains("://") {
        return Vec::new();
    }
    let rel = entry.trim_start_matches("./").trim_start_matches('/');
    let mut bases = Vec::new();
    if let Some(src_rel) = remap_build_output_to_src(rel) {
        bases.push(package_dir.join(src_rel));
    }
    bases.push(package_dir.join(rel));
    bases
}

fn remap_build_output_to_src(rel: &str) -> Option<String> {
    const BUILD_FLAVOR_DIRS: &[&str] = &["esm", "cjs"];
    let mut components = rel.split('/');
    let first = components.next()?;
    if !BUILD_OUTPUT_DIRS.contains(&first) {
        return None;
    }
    let mut rest = components.collect::<Vec<_>>();
    if rest.len() > 1 && BUILD_FLAVOR_DIRS.contains(&rest[0]) {
        rest.remove(0);
    }
    if rest.is_empty() {
        return None;
    }
    Some(format!("src/{}", rest.join("/")))
}

#[cfg(windows)]
pub fn normalize_path(path: &Path) -> PathBuf {
    normalize_path_components(&crate::windows_path::normalize_windows_path(path))
}

#[cfg(not(windows))]
pub fn normalize_path(path: &Path) -> PathBuf {
    normalize_path_components(path)
}

fn normalize_path_components(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}
