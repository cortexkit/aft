use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};
use serde_json::Value;

use super::frameworks::{detected_route_frameworks, Framework};
use super::job::{canonicalize_normalized, normalize_path};

pub(crate) const EXECUTABLE_ROOT_ALL_EXPORTS: &str = "*";

const JS_MODULE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
const STORYBOOK_STORY_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx"];
const TOOL_CONFIG_DEFAULT_EXPORTS: &[&str] = &["default"];
const STORYBOOK_CSF_EXPORTS: &[&str] = &[EXECUTABLE_ROOT_ALL_EXPORTS];
const MOCHA_ROOT_HOOK_EXPORTS: &[&str] = &["mochaHooks"];

#[derive(Debug, Clone, Default)]
pub(crate) struct EntryPointSet {
    liveness_root_files: BTreeSet<PathBuf>,
    public_api_files: BTreeSet<PathBuf>,
    executable_root_exports: BTreeMap<PathBuf, BTreeSet<String>>,
    warnings: Vec<String>,
}

impl EntryPointSet {
    pub(crate) fn is_entry_point(&self, file: &Path) -> bool {
        self.is_liveness_root_file(file)
    }

    pub(crate) fn is_liveness_root_file(&self, file: &Path) -> bool {
        contains_path(&self.liveness_root_files, file)
    }

    pub(crate) fn is_public_api_file(&self, file: &Path) -> bool {
        contains_path(&self.public_api_files, file)
    }

    pub(crate) fn public_api_files_relative(&self, project_root: &Path) -> BTreeSet<String> {
        self.public_api_files
            .iter()
            .map(|file| relative_path(project_root, file))
            .collect()
    }

    pub(crate) fn public_api_files(&self) -> Vec<PathBuf> {
        self.public_api_files.iter().cloned().collect()
    }

    pub(crate) fn executable_root_exports(&self) -> BTreeMap<PathBuf, BTreeSet<String>> {
        self.executable_root_exports.clone()
    }

    pub(crate) fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn insert_liveness_root(&mut self, path: &Path) {
        self.liveness_root_files.insert(snapshot_path(path));
    }

    fn insert_public_api_file(&mut self, path: &Path) {
        let path = snapshot_path(path);
        self.liveness_root_files.insert(path.clone());
        self.public_api_files.insert(path);
    }

    fn insert_executable_root_exports(&mut self, path: &Path, exports: BTreeSet<String>) {
        let path = snapshot_path(path);
        self.liveness_root_files.insert(path.clone());
        self.executable_root_exports
            .entry(path)
            .or_default()
            .extend(exports);
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }

    fn has_liveness_roots(&self) -> bool {
        !self.liveness_root_files.is_empty()
    }
}

#[derive(Debug, Default)]
struct ManifestPaths {
    cargo_tomls: Vec<PathBuf>,
    package_jsons: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum EntryPointKind {
    LivenessRoot,
    PublicApi,
}

pub(crate) fn resolve_entry_points(project_root: &Path) -> EntryPointSet {
    let project_root = snapshot_path(project_root);
    let mut manifests = ManifestPaths::default();
    collect_manifests(&project_root, &mut manifests);

    let manifest_found = !manifests.cargo_tomls.is_empty() || !manifests.package_jsons.is_empty();
    let mut entry_points = EntryPointSet::default();

    for manifest in manifests.cargo_tomls {
        collect_cargo_manifest_entry_points(&manifest, &mut entry_points);
    }
    for manifest in manifests.package_jsons {
        collect_package_manifest_entry_points(&manifest, &mut entry_points);
    }

    // A manifest can be present without declaring any runnable or public root
    // (for example a private package.json or a virtual Cargo workspace). In
    // that case, keep the conventional fallback so liveness analysis never
    // starts from an empty root set solely because a manifest exists.
    if !manifest_found || !entry_points.has_liveness_roots() {
        collect_fallback_entry_points(&project_root, &mut entry_points);
    }
    collect_conventional_executable_root_entry_points(&project_root, &mut entry_points);

    entry_points
}

pub(crate) fn collect_entry_point_manifests(project_root: &Path) -> Vec<PathBuf> {
    let project_root = snapshot_path(project_root);
    let mut manifests = ManifestPaths::default();
    collect_manifests(&project_root, &mut manifests);

    let mut paths = manifests.cargo_tomls;
    paths.extend(manifests.package_jsons);
    paths.sort();
    paths.dedup();
    paths
}

fn collect_manifests(project_root: &Path, manifests: &mut ManifestPaths) {
    for path in entry_point_walk_files(project_root) {
        match path.file_name().and_then(|name| name.to_str()) {
            Some("Cargo.toml") => manifests.cargo_tomls.push(path),
            Some("package.json") => manifests.package_jsons.push(path),
            _ => {}
        }
    }

    manifests.cargo_tomls.sort();
    manifests.cargo_tomls.dedup();
    manifests.package_jsons.sort();
    manifests.package_jsons.dedup();
}

fn entry_point_walk_files(project_root: &Path) -> Vec<PathBuf> {
    let mut builder = ignore::WalkBuilder::new(project_root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry(|entry| {
            !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
                || !should_skip_manifest_dir(entry.path())
        });

    let mut files = builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
        })
        .map(|entry| entry.into_path())
        .collect::<Vec<_>>();
    files.sort();
    files
}

fn should_skip_manifest_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            ".git"
                | "node_modules"
                | "target"
                | "venv"
                | ".venv"
                | "__pycache__"
                | ".tox"
                | "dist"
                | "build"
                | ".aft-test-storage"
                | ".aft-cache"
        )
    )
}

fn collect_cargo_manifest_entry_points(manifest: &Path, entry_points: &mut EntryPointSet) {
    let package_dir = manifest.parent().unwrap_or_else(|| Path::new("."));
    let source = match fs::read_to_string(manifest) {
        Ok(source) => source,
        Err(error) => {
            entry_points.warn(format!("failed to read {}: {error}", manifest.display()));
            return;
        }
    };
    let value = match source.parse::<toml::Value>() {
        Ok(value) => value,
        Err(error) => {
            entry_points.warn(format!("failed to parse {}: {error}", manifest.display()));
            return;
        }
    };

    let has_package = value.get("package").is_some_and(toml::Value::is_table);
    collect_cargo_lib_target(package_dir, &value, has_package, entry_points);
    collect_cargo_bin_targets(package_dir, &value, has_package, entry_points);
}

fn collect_cargo_lib_target(
    package_dir: &Path,
    value: &toml::Value,
    has_package: bool,
    entry_points: &mut EntryPointSet,
) {
    if let Some(lib) = value.get("lib") {
        if let Some(path) = lib.get("path").and_then(toml::Value::as_str) {
            insert_existing_entry_point(
                entry_points,
                &package_dir.join(path),
                EntryPointKind::PublicApi,
            );
        } else {
            insert_existing_entry_point(
                entry_points,
                &package_dir.join("src/lib.rs"),
                EntryPointKind::PublicApi,
            );
        }
        return;
    }

    if has_package && package_autodiscovery_enabled(value, "autolib") {
        insert_existing_entry_point(
            entry_points,
            &package_dir.join("src/lib.rs"),
            EntryPointKind::PublicApi,
        );
    }
}

fn collect_cargo_bin_targets(
    package_dir: &Path,
    value: &toml::Value,
    has_package: bool,
    entry_points: &mut EntryPointSet,
) {
    if let Some(bins) = value.get("bin").and_then(toml::Value::as_array) {
        for bin in bins {
            if let Some(path) = bin.get("path").and_then(toml::Value::as_str) {
                insert_existing_entry_point(
                    entry_points,
                    &package_dir.join(path),
                    EntryPointKind::LivenessRoot,
                );
            } else if let Some(name) = bin.get("name").and_then(toml::Value::as_str) {
                let named_bin = package_dir.join("src/bin").join(format!("{name}.rs"));
                if named_bin.is_file() {
                    insert_existing_entry_point(
                        entry_points,
                        &named_bin,
                        EntryPointKind::LivenessRoot,
                    );
                } else {
                    insert_existing_entry_point(
                        entry_points,
                        &package_dir.join("src/main.rs"),
                        EntryPointKind::LivenessRoot,
                    );
                }
            } else {
                insert_existing_entry_point(
                    entry_points,
                    &package_dir.join("src/main.rs"),
                    EntryPointKind::LivenessRoot,
                );
            }
        }
    }

    if has_package && package_autodiscovery_enabled(value, "autobins") {
        insert_existing_entry_point(
            entry_points,
            &package_dir.join("src/main.rs"),
            EntryPointKind::LivenessRoot,
        );
        collect_cargo_src_bin_targets(package_dir, entry_points);
    }
}

fn package_autodiscovery_enabled(value: &toml::Value, key: &str) -> bool {
    value
        .get("package")
        .and_then(|package| package.get(key))
        .and_then(toml::Value::as_bool)
        .unwrap_or(true)
}

fn collect_cargo_src_bin_targets(package_dir: &Path, entry_points: &mut EntryPointSet) {
    let src_bin = package_dir.join("src/bin");
    let Ok(entries) = fs::read_dir(src_bin) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) == Some("rs") && path.is_file()
        {
            insert_existing_entry_point(entry_points, &path, EntryPointKind::LivenessRoot);
        }
    }
}

fn collect_package_manifest_entry_points(manifest: &Path, entry_points: &mut EntryPointSet) {
    let package_dir = manifest.parent().unwrap_or_else(|| Path::new("."));
    let source = match fs::read_to_string(manifest) {
        Ok(source) => source,
        Err(error) => {
            entry_points.warn(format!("failed to read {}: {error}", manifest.display()));
            return;
        }
    };
    let value = match serde_json::from_str::<Value>(&source) {
        Ok(value) => value,
        Err(error) => {
            entry_points.warn(format!("failed to parse {}: {error}", manifest.display()));
            return;
        }
    };

    let mut public_entries = BTreeSet::new();
    if let Some(main) = value.get("main").and_then(Value::as_str) {
        public_entries.insert(main.to_string());
    }
    if let Some(module) = value.get("module").and_then(Value::as_str) {
        public_entries.insert(module.to_string());
    }
    if let Some(exports) = value.get("exports") {
        collect_json_entry_strings(exports, &mut public_entries);
    }

    let mut bin_entries = BTreeSet::new();
    if let Some(bin) = value.get("bin") {
        collect_json_entry_strings(bin, &mut bin_entries);
    }

    for entry in public_entries {
        insert_package_entry(package_dir, &entry, entry_points, EntryPointKind::PublicApi);
    }
    for entry in bin_entries {
        insert_package_entry(
            package_dir,
            &entry,
            entry_points,
            EntryPointKind::LivenessRoot,
        );
    }

    if let Some(scripts) = value.get("scripts").and_then(Value::as_object) {
        for command in scripts.values().filter_map(Value::as_str) {
            collect_script_entry_points(package_dir, command, entry_points);
        }
    }

    collect_package_tool_config_entry_points(package_dir, &value, entry_points);

    collect_framework_route_entry_points(package_dir, &value, entry_points);
}

fn collect_package_tool_config_entry_points(
    package_dir: &Path,
    manifest: &Value,
    entry_points: &mut EntryPointSet,
) {
    let mut path_entries = BTreeSet::new();
    for pointer in [&["mocha", "require"][..]] {
        if let Some(value) = json_value_at_path(manifest, pointer) {
            collect_json_entry_strings(value, &mut path_entries);
        }
    }

    let exports = export_set(MOCHA_ROOT_HOOK_EXPORTS);
    for entry in path_entries {
        insert_package_executable_entry_exports(package_dir, &entry, entry_points, &exports);
    }
}

fn json_value_at_path<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    path.iter()
        .try_fold(value, |current, key| current.get(*key))
}

fn collect_framework_route_entry_points(
    package_dir: &Path,
    manifest: &Value,
    entry_points: &mut EntryPointSet,
) {
    for framework in detected_route_frameworks(manifest) {
        collect_framework_route_files(package_dir, framework, entry_points);
    }
}

fn collect_framework_route_files(
    package_dir: &Path,
    framework: Framework,
    entry_points: &mut EntryPointSet,
) {
    let mut builder = GlobSetBuilder::new();
    for pattern in framework.route_globs() {
        match Glob::new(pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(error) => {
                entry_points.warn(format!(
                    "failed to compile framework route glob {pattern:?}: {error}"
                ));
                return;
            }
        }
    }
    let globset = match builder.build() {
        Ok(globset) => globset,
        Err(error) => {
            entry_points.warn(format!("failed to compile framework route globs: {error}"));
            return;
        }
    };

    let package_dir = snapshot_path(package_dir);
    for path in framework_route_walk_files(&package_dir) {
        let relative = path
            .strip_prefix(&package_dir)
            .unwrap_or(path.as_path())
            .to_string_lossy()
            .replace('\\', "/");
        if globset.is_match(&relative) {
            entry_points
                .insert_executable_root_exports(&path, framework.framework_called_exports());
        }
    }
}

fn framework_route_walk_files(package_dir: &Path) -> Vec<PathBuf> {
    let package_dir = snapshot_path(package_dir);
    let mut builder = ignore::WalkBuilder::new(&package_dir);
    let package_root = package_dir.clone();
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .add_custom_ignore_filename(".aftignore")
        .filter_entry(move |entry| {
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_dir())
            {
                return true;
            }
            if should_skip_manifest_dir(entry.path()) {
                return false;
            }
            let path = snapshot_path(entry.path());
            path == package_root || !path.join("package.json").is_file()
        });

    let mut files = builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
        })
        .map(|entry| snapshot_path(&entry.into_path()))
        .collect::<Vec<_>>();
    files.sort();
    files.dedup();
    files
}

/// Extract local source files referenced by an npm `scripts` command (e.g.
/// `bun run benchmarks/src/runner.ts`, `node scripts/build.mjs`, `tsx x.ts`).
/// These are runnable roots — their reachable code is live even though nothing
/// imports them — so harness/script code is not reported dead. Conservative:
/// only tokens that resolve to an existing local source file are added; flags,
/// bare binaries, config files, and node_modules refs are ignored.
fn collect_script_entry_points(
    package_dir: &Path,
    command: &str,
    entry_points: &mut EntryPointSet,
) {
    let is_separator =
        |c: char| c.is_whitespace() || matches!(c, '&' | '|' | ';' | '(' | ')' | '<' | '>' | ',');
    for token in command.split(is_separator) {
        let token = token
            .trim()
            .trim_matches(|c| c == '"' || c == '\'' || c == '`');
        if token.is_empty() || token.starts_with('-') || token.contains("node_modules") {
            continue;
        }
        let is_source = Path::new(token)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| JS_MODULE_EXTENSIONS.contains(&extension));
        if !is_source {
            continue;
        }
        let candidate = normalize_path(&package_dir.join(token));
        if candidate.is_file() {
            insert_resolved_entry_point(entry_points, &candidate, EntryPointKind::LivenessRoot);
        }
    }
}

fn collect_conventional_executable_root_entry_points(
    project_root: &Path,
    entry_points: &mut EntryPointSet,
) {
    for path in entry_point_walk_files(project_root) {
        if let Some(exports) = conventional_executable_root_exports(&path) {
            entry_points.insert_executable_root_exports(&path, exports);
        }
    }
}

fn conventional_executable_root_exports(file: &Path) -> Option<BTreeSet<String>> {
    if is_tool_config_file(file) {
        return Some(export_set(TOOL_CONFIG_DEFAULT_EXPORTS));
    }
    if is_storybook_story_file(file) {
        return Some(export_set(STORYBOOK_CSF_EXPORTS));
    }
    None
}

fn is_tool_config_file(file: &Path) -> bool {
    if !has_js_module_extension(file, JS_MODULE_EXTENSIONS) {
        return false;
    }
    file.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.ends_with(".config"))
}

fn is_storybook_story_file(file: &Path) -> bool {
    if !has_js_module_extension(file, STORYBOOK_STORY_EXTENSIONS) {
        return false;
    }
    file.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains(".stories."))
}

fn has_js_module_extension(file: &Path, extensions: &[&str]) -> bool {
    file.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extensions.contains(&extension))
}

fn export_set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

fn collect_json_entry_strings(value: &Value, entries: &mut BTreeSet<String>) {
    match value {
        Value::String(entry) => {
            entries.insert(entry.clone());
        }
        Value::Array(values) => {
            for value in values {
                collect_json_entry_strings(value, entries);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_json_entry_strings(value, entries);
            }
        }
        _ => {}
    }
}

fn insert_package_entry(
    package_dir: &Path,
    entry: &str,
    entry_points: &mut EntryPointSet,
    kind: EntryPointKind,
) {
    let entry = entry.trim();
    if entry.is_empty() || entry.starts_with('#') || entry.contains('*') {
        return;
    }

    if let Some(path) = resolve_package_entry(package_dir, entry) {
        insert_resolved_entry_point(entry_points, &path, kind);
    }
}

fn insert_package_executable_entry_exports(
    package_dir: &Path,
    entry: &str,
    entry_points: &mut EntryPointSet,
    exports: &BTreeSet<String>,
) {
    let entry = entry.trim();
    if entry.is_empty() || entry.starts_with('#') || entry.contains('*') {
        return;
    }

    if let Some(path) = resolve_package_entry(package_dir, entry) {
        entry_points.insert_executable_root_exports(&path, exports.clone());
    }
}

fn resolve_package_entry(package_dir: &Path, entry: &str) -> Option<PathBuf> {
    if entry.starts_with("node:") || entry.contains("://") {
        return None;
    }

    let rel = if is_relative_module(entry) {
        entry.trim_start_matches("./").to_string()
    } else {
        entry.trim_start_matches('/').to_string()
    };

    // package.json `main`/`module`/`exports`/`bin` point at BUILD OUTPUT (e.g.
    // dist/index.js), but the inspect scanner only sees SOURCE — `dist/` is
    // build output excluded from the walk. Prefer the source equivalent
    // (src/index.ts) so the source barrel/entry file is the one recognized as a
    // public-API file (and its re-exports suppressed). Falls back to the literal
    // path when the package's source already lives where the entry points.
    // Try the source-remapped path first, then the literal entry. `find`
    // returns the first existing file, so a package whose source lives where the
    // entry points still resolves via the literal fallback.
    let mut bases = Vec::new();
    if let Some(src_rel) = remap_build_output_to_src(&rel) {
        bases.push(package_dir.join(src_rel));
    }
    bases.push(package_dir.join(&rel));

    bases
        .iter()
        .flat_map(|base| candidate_paths(base))
        .map(|candidate| normalize_path(&candidate))
        .find(|candidate| candidate.is_file())
}

/// Map a built entry path (`dist/index.js`) to its likely source location
/// (`src/index.js`, which `candidate_paths` then remaps to `src/index.ts`).
/// Returns `None` when the path is not under a recognized build-output dir, in
/// which case the caller uses the literal path. `lib` is intentionally excluded
/// — it is as commonly a source dir as a build dir, so remapping it risks
/// pointing at an unrelated `src/` file.
fn remap_build_output_to_src(rel: &str) -> Option<String> {
    const BUILD_DIRS: &[&str] = &["dist", "build", "out", "output", "esm", "cjs"];
    const BUILD_FLAVOR_DIRS: &[&str] = &["esm", "cjs"];
    let mut components = rel.split('/');
    let first = components.next()?;
    if !BUILD_DIRS.contains(&first) {
        return None;
    }
    let mut rest: Vec<&str> = components.collect();
    if rest.len() > 1 && BUILD_FLAVOR_DIRS.contains(&rest[0]) {
        rest.remove(0);
    }
    if rest.is_empty() {
        return None;
    }
    Some(format!("src/{}", rest.join("/")))
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

    // A `.js`/`.mjs`/... specifier (NodeNext) resolves to its `.ts`/`.tsx`/...
    // source; probe those too, not just the literal extension.
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

fn is_relative_module(module_path: &str) -> bool {
    module_path.starts_with("./")
        || module_path.starts_with("../")
        || module_path == "."
        || module_path == ".."
}

fn collect_fallback_entry_points(project_root: &Path, entry_points: &mut EntryPointSet) {
    for path in entry_point_walk_files(project_root) {
        if let Some(kind) = conventional_entry_point_kind(project_root, &path) {
            insert_resolved_entry_point(entry_points, &path, kind);
        }
    }
}

fn conventional_entry_point_kind(project_root: &Path, file: &Path) -> Option<EntryPointKind> {
    let relative = file.strip_prefix(project_root).unwrap_or(file);
    let relative_display = relative.to_string_lossy().replace('\\', "/");
    if relative_display.starts_with("bin/") || relative_display.contains("/bin/") {
        return Some(EntryPointKind::LivenessRoot);
    }

    let file_name = relative.file_name().and_then(|name| name.to_str())?;
    match file_name {
        "lib.rs" | "index.ts" | "index.tsx" | "index.js" | "index.jsx" => {
            Some(EntryPointKind::PublicApi)
        }
        "main.rs" | "main.ts" | "main.tsx" | "main.js" | "main.jsx" | "main.py" | "main.go" => {
            Some(EntryPointKind::LivenessRoot)
        }
        _ => None,
    }
}

fn insert_existing_entry_point(
    entry_points: &mut EntryPointSet,
    path: &Path,
    kind: EntryPointKind,
) {
    if path.is_file() {
        insert_resolved_entry_point(entry_points, path, kind);
    }
}

fn insert_resolved_entry_point(
    entry_points: &mut EntryPointSet,
    path: &Path,
    kind: EntryPointKind,
) {
    match kind {
        EntryPointKind::LivenessRoot => entry_points.insert_liveness_root(path),
        EntryPointKind::PublicApi => entry_points.insert_public_api_file(path),
    }
}

fn contains_path(paths: &BTreeSet<PathBuf>, file: &Path) -> bool {
    let snapshot = snapshot_path(file);
    let hit = paths.contains(&snapshot) || paths.contains(&normalize_path(file));
    if !hit {
        // CI probe instrumentation (probe branch only).
        eprintln!("PROBE contains_path MISS for {}", file.display());
        eprintln!("PROBE   snapshot form = {}", snapshot.display());
        eprintln!(
            "PROBE   normalize form = {}",
            normalize_path(file).display()
        );
        for p in paths.iter() {
            eprintln!("PROBE   set entry = {}", p.display());
        }
    }
    hit
}

fn snapshot_path(path: &Path) -> PathBuf {
    canonicalize_normalized(path)
}

fn relative_path(project_root: &Path, path: &Path) -> String {
    let project_root = snapshot_path(project_root);
    let path = snapshot_path(path);
    path.strip_prefix(&project_root)
        .unwrap_or(path.as_path())
        .to_string_lossy()
        .replace('\\', "/")
}

/// Coarse signal tier for ranking inspect drill-down output. Product findings
/// are the actionable signal and surface first; test code is secondary;
/// dev-tooling/benchmark findings are genuine but low-value and sink last.
/// Ordering matters: lower value = higher priority (sorts first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SignalTier {
    Product = 0,
    Test = 1,
    Tooling = 2,
}

/// Manifest-derived classification of which workspace packages are product
/// (declare a public API / are real crates) vs dev tooling (private,
/// scripts-only packages such as benchmark harnesses). Used to rank inspect
/// drill-down so product findings surface above benchmark/tooling noise — never
/// to filter, so a misclassification only re-orders and can never hide a
/// finding.
#[derive(Debug, Clone, Default)]
pub(crate) struct ProjectRoles {
    /// Relative, '/'-normalized package directories that expose a public API
    /// (package.json main/module/exports/bin) or are real Cargo crates.
    product_dirs: Vec<String>,
    /// Relative, '/'-normalized private scripts-only package directories.
    tooling_dirs: Vec<String>,
}

/// Classify every workspace package as product vs dev-tooling from its manifest.
/// A package.json is product when it declares main/module/exports/bin and
/// tooling when it is private/scripts-only; a Cargo package (`[package]`) is
/// product. The repo-root manifest is intentionally skipped: as a monorepo
/// orchestrator its directory ("") is a prefix of everything, so root-level
/// loose files fall through to the name fallback instead.
pub(crate) fn resolve_project_roles(project_root: &Path) -> ProjectRoles {
    let project_root = snapshot_path(project_root);
    let mut roles = ProjectRoles::default();

    for manifest in collect_entry_point_manifests(&project_root) {
        let Some(dir) = manifest.parent() else {
            continue;
        };
        let rel_dir = relative_path(&project_root, dir);
        if rel_dir.is_empty() {
            // Repo-root manifest — see doc comment.
            continue;
        }
        match manifest.file_name().and_then(|name| name.to_str()) {
            Some("package.json") => match classify_package_json(&manifest) {
                Some(true) => roles.product_dirs.push(rel_dir),
                Some(false) => roles.tooling_dirs.push(rel_dir),
                None => {}
            },
            Some("Cargo.toml") => {
                if cargo_manifest_is_package(&manifest) {
                    roles.product_dirs.push(rel_dir);
                }
            }
            _ => {}
        }
    }

    // Longest (most specific) prefix wins in role_for; pre-sort longest-first.
    roles
        .product_dirs
        .sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    roles
        .tooling_dirs
        .sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    roles
}

impl ProjectRoles {
    /// Signal tier for a project-relative file path.
    pub(crate) fn role_for(&self, relative_file: &str) -> SignalTier {
        let norm = relative_file.replace('\\', "/");

        // Test code is classified by path regardless of which package it lives
        // in (a product package's tests are still test-tier).
        if is_test_path(&norm) {
            return SignalTier::Test;
        }

        let product = best_prefix_len(&self.product_dirs, &norm);
        let tooling = best_prefix_len(&self.tooling_dirs, &norm);
        match (product, tooling) {
            // Most-specific (longest) matching package dir wins. Ties favor
            // product (conservative: never sink a real finding on a tie).
            (Some(p), Some(t)) => {
                if p >= t {
                    SignalTier::Product
                } else {
                    SignalTier::Tooling
                }
            }
            (Some(_), None) => SignalTier::Product,
            (None, Some(_)) => SignalTier::Tooling,
            // No manifest owns this file — fall back to path-name hints.
            (None, None) => fallback_role_by_name(&norm),
        }
    }
}

/// Length of the longest directory prefix in `dirs` that contains `file`
/// (segment-aware: `packages/aft` does not match `packages/aft-bridge`).
fn best_prefix_len(dirs: &[String], file: &str) -> Option<usize> {
    dirs.iter()
        .filter(|dir| file == dir.as_str() || file.starts_with(&format!("{dir}/")))
        .map(String::len)
        .max()
}

fn is_test_path(norm: &str) -> bool {
    if super::job::is_test_support_file(norm) {
        return true;
    }
    if norm
        .split('/')
        .any(|segment| matches!(segment, "tests" | "__tests__" | "test"))
    {
        return true;
    }
    let file_name = norm.rsplit('/').next().unwrap_or(norm);
    file_name.contains(".test.")
        || file_name.contains(".spec.")
        || file_name.ends_with("_test.rs")
        || file_name.ends_with("_test.go")
        || file_name.starts_with("test_")
}

/// Last-resort classification for files no manifest claims (e.g. a loose
/// `scripts/foo.ts`). Only an explicit tooling-shaped path segment sinks a file;
/// everything else stays Product so real findings are never demoted by guess.
fn fallback_role_by_name(norm: &str) -> SignalTier {
    if norm.split('/').any(|segment| {
        matches!(
            segment,
            "benchmarks" | "benchmark" | "bench" | "scripts" | "examples"
        )
    }) {
        SignalTier::Tooling
    } else {
        SignalTier::Product
    }
}

fn classify_package_json(manifest: &Path) -> Option<bool> {
    let value = fs::read_to_string(manifest)
        .ok()
        .and_then(|source| serde_json::from_str::<Value>(&source).ok())?;
    let has_public_api = value.get("main").is_some()
        || value.get("module").is_some()
        || value.get("exports").is_some()
        || value.get("bin").is_some();
    Some(has_public_api)
}

fn cargo_manifest_is_package(manifest: &Path) -> bool {
    fs::read_to_string(manifest)
        .ok()
        .and_then(|source| source.parse::<toml::Value>().ok())
        .is_some_and(|value| value.get("package").is_some_and(toml::Value::is_table))
}

/// Stable-rank drill-down items (each carrying a `"file"` string) by signal tier
/// so product findings surface first, then truncate to `limit`. Ranking-only:
/// never drops an item except by the existing drill-down cap.
pub(crate) fn rank_and_truncate_items(
    mut items: Vec<Value>,
    roles: &ProjectRoles,
    limit: Option<usize>,
) -> Vec<Value> {
    items.sort_by_key(|item| {
        let file = item.get("file").and_then(Value::as_str).unwrap_or("");
        roles.role_for(file)
    });
    if let Some(limit) = limit {
        items.truncate(limit);
    }
    items
}

/// Number of items surfaced inline in the summary preview.
pub(crate) const TOP_PREVIEW_ITEMS: usize = 3;

/// A compact, already-ranked `{file, symbol}` top-N preview for the summary
/// view, so one `aft_inspect` call surfaces the highest-signal findings without
/// a separate drill-down. Shared by dead_code and unused_exports.
pub(crate) fn top_preview_symbols(items: &[Value]) -> Vec<Value> {
    items
        .iter()
        .take(TOP_PREVIEW_ITEMS)
        .map(|item| {
            serde_json::json!({
                "file": item.get("file").and_then(Value::as_str).unwrap_or(""),
                "symbol": item.get("symbol").and_then(Value::as_str).unwrap_or(""),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_scripts_seed_source_files_as_liveness_roots() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("src/runner.ts"), "export const x = 1;\n").unwrap();
        std::fs::write(root.join("scripts/build.mjs"), "console.log(1)\n").unwrap();
        // A private package (no main/module/exports/bin) with only scripts.
        std::fs::write(
            root.join("package.json"),
            r#"{
                "name": "x",
                "private": true,
                "scripts": {
                    "bench": "bun run src/runner.ts",
                    "build": "node scripts/build.mjs --flag",
                    "lint": "biome check .",
                    "missing": "bun run src/does-not-exist.ts"
                }
            }"#,
        )
        .unwrap();

        let entry_points = resolve_entry_points(root);
        // Real script targets become liveness roots.
        assert!(entry_points.is_liveness_root_file(&root.join("src/runner.ts")));
        assert!(entry_points.is_liveness_root_file(&root.join("scripts/build.mjs")));
        // Non-file tokens (binaries, flags, ".") and missing files are ignored.
        assert!(!entry_points.is_liveness_root_file(&root.join("src/does-not-exist.ts")));
        // Script roots are liveness roots, not public-API surfaces.
        assert!(!entry_points.is_public_api_file(&root.join("src/runner.ts")));
    }

    #[test]
    fn tool_config_files_seed_default_export_as_executable_roots() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        std::fs::write(
            root.join("package.json"),
            r#"{ "private": true, "scripts": { "test": "vitest" } }"#,
        )
        .unwrap();
        std::fs::write(root.join("playwright.config.ts"), "export default {}\n").unwrap();
        std::fs::write(root.join("eslint.config.mjs"), "export default []\n").unwrap();

        let entry_points = resolve_entry_points(root);
        let executable_roots = entry_points.executable_root_exports();

        for file in ["playwright.config.ts", "eslint.config.mjs"] {
            let path = root.join(file);
            assert!(entry_points.is_liveness_root_file(&path));
            assert!(!entry_points.is_public_api_file(&path));
            assert_eq!(
                executable_roots.get(&snapshot_path(&path)).cloned(),
                Some(BTreeSet::from(["default".to_string()])),
                "{file} should seed only the CLI-called default export"
            );
        }
    }

    #[test]
    fn storybook_csf_files_seed_all_exports_as_executable_roots() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        std::fs::write(root.join("package.json"), r#"{ "private": true }"#).unwrap();
        let story = root.join("src/Button.stories.tsx");
        std::fs::create_dir_all(story.parent().expect("story parent")).unwrap();
        std::fs::write(&story, "export default {}; export const Primary = {};\n").unwrap();

        let entry_points = resolve_entry_points(root);
        let executable_roots = entry_points.executable_root_exports();

        assert!(entry_points.is_liveness_root_file(&story));
        assert!(!entry_points.is_public_api_file(&story));
        assert_eq!(
            executable_roots.get(&snapshot_path(&story)).cloned(),
            Some(BTreeSet::from([EXECUTABLE_ROOT_ALL_EXPORTS.to_string()])),
            "CSF default and named story exports should be framework-called"
        );
    }

    #[test]
    fn package_mocha_require_paths_seed_root_hook_exports() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let hook = root.join("nest/hooks/mocha-init-hook.ts");
        std::fs::create_dir_all(hook.parent().expect("hook parent")).unwrap();
        std::fs::write(&hook, "export const mochaHooks = {};\n").unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{
                "private": true,
                "mocha": { "require": ["ts-node/register", "nest/hooks/mocha-init-hook.ts"] }
            }"#,
        )
        .unwrap();

        let entry_points = resolve_entry_points(root);
        let executable_roots = entry_points.executable_root_exports();

        assert!(entry_points.is_liveness_root_file(&hook));
        assert_eq!(
            executable_roots.get(&snapshot_path(&hook)).cloned(),
            Some(BTreeSet::from(["mochaHooks".to_string()])),
            "package.json mocha.require should seed the Mocha root-hook export"
        );
    }

    #[test]
    fn framework_route_files_are_executable_roots_not_public_api() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let files = [
            "app/api/route.ts",
            "server/api/hello.ts",
            "src/routes/+server.ts",
            "app/routes/home.tsx",
            "src/pages/api.ts",
        ];
        for file in files {
            let path = root.join(file);
            std::fs::create_dir_all(path.parent().expect("route parent")).unwrap();
            std::fs::write(path, "export const GET = () => null;\n").unwrap();
        }
        std::fs::write(
            root.join("package.json"),
            r#"{
                "dependencies": {
                    "next": "latest",
                    "nuxt": "latest",
                    "@remix-run/react": "latest",
                    "astro": "latest"
                },
                "devDependencies": { "@sveltejs/kit": "latest" },
                "scripts": { "dev": "vite dev" }
            }"#,
        )
        .unwrap();

        let entry_points = resolve_entry_points(root);
        let executable_roots = entry_points.executable_root_exports();
        for file in files {
            let path = root.join(file);
            assert!(
                entry_points.is_liveness_root_file(&path),
                "{file} should execute as a framework route root"
            );
            assert!(
                !entry_points.is_public_api_file(&path),
                "{file} should not become a public API file"
            );
            assert!(
                executable_roots
                    .get(&snapshot_path(&path))
                    .is_some_and(|exports| exports.contains("GET") || exports.contains("default")),
                "{file} should carry a framework export allowlist: {executable_roots:#?}"
            );
        }
    }

    #[test]
    fn sveltekit_hooks_are_executable_roots_with_hook_exports() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let hook = root.join("src/hooks.server.ts");
        std::fs::create_dir_all(hook.parent().expect("hook parent")).unwrap();
        std::fs::write(&hook, "export const handle = () => {};\n").unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{
                "devDependencies": { "@sveltejs/kit": "latest" },
                "scripts": { "dev": "vite dev" }
            }"#,
        )
        .unwrap();

        let entry_points = resolve_entry_points(root);
        let executable_roots = entry_points.executable_root_exports();
        let exports = executable_roots
            .get(&snapshot_path(&hook))
            .expect("SvelteKit hook file should be an executable root");

        assert!(entry_points.is_liveness_root_file(&hook));
        assert!(!entry_points.is_public_api_file(&hook));
        assert!(exports.contains("handle"));
        assert!(exports.contains("handleError"));
        assert!(!exports.contains("GET"));
    }

    #[test]
    fn framework_route_dev_dependency_requires_matching_script() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let route = root.join("app/api/route.ts");
        std::fs::create_dir_all(route.parent().expect("route parent")).unwrap();
        std::fs::write(&route, "export const GET = () => null;\n").unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{ "devDependencies": { "next": "latest" }, "scripts": { "docs": "vitepress dev" } }"#,
        )
        .unwrap();

        let without_script = resolve_entry_points(root);
        assert!(
            !without_script.is_liveness_root_file(&route),
            "devDependency-only Next without a next script must not create route roots"
        );

        std::fs::write(
            root.join("package.json"),
            r#"{ "devDependencies": { "next": "latest" }, "scripts": { "build": "next build" } }"#,
        )
        .unwrap();
        let with_script = resolve_entry_points(root);
        assert!(
            with_script.is_liveness_root_file(&route),
            "devDependency-only Next with a next script should create route roots"
        );
    }

    #[test]
    fn framework_route_walk_stops_at_nested_package_json() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let root_route = root.join("app/api/route.ts");
        let nested_route = root.join("packages/docs/app/api/route.ts");
        for route in [&root_route, &nested_route] {
            std::fs::create_dir_all(route.parent().expect("route parent")).unwrap();
            std::fs::write(route, "export const GET = () => null;\n").unwrap();
        }
        std::fs::write(
            root.join("package.json"),
            r#"{ "dependencies": { "next": "latest" } }"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("packages/docs")).unwrap();
        std::fs::write(
            root.join("packages/docs/package.json"),
            r#"{ "private": true }"#,
        )
        .unwrap();

        let entry_points = resolve_entry_points(root);
        assert!(entry_points.is_liveness_root_file(&root_route));
        assert!(
            !entry_points.is_liveness_root_file(&nested_route),
            "a parent framework package must not claim route files owned by a nested package"
        );
    }

    #[test]
    fn public_api_entry_pointing_at_dist_resolves_to_src_source() {
        // package.json `main`/`exports` point at built output (dist/index.js),
        // but the scanner only sees source. The SOURCE barrel (src/index.ts)
        // must be the file recognized as public-API so its re-exports are
        // suppressed — not the never-scanned dist path.
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/index.ts"), "export const x = 1;\n").unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{
                "name": "@scope/pkg",
                "main": "dist/index.js",
                "module": "dist/index.js",
                "exports": { ".": { "import": "./dist/index.js" } }
            }"#,
        )
        .unwrap();

        let entry_points = resolve_entry_points(root);
        assert!(
            entry_points.is_public_api_file(&root.join("src/index.ts")),
            "src/index.ts should be recognized as public-API via dist->src remap"
        );
    }

    #[test]
    fn package_exports_subpaths_resolve_to_public_api_source_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("src/codegenv2")).unwrap();
        std::fs::create_dir_all(root.join("src/experimental")).unwrap();
        std::fs::write(
            root.join("src/codegenv2/index.ts"),
            "export * from './paths';\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/experimental/index.ts"),
            "export const x = 1;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{
                "name": "@fixtures/protobuf-es",
                "exports": {
                    "./codegenv2": {
                        "types": "./dist/codegenv2/index.d.ts",
                        "import": "./dist/esm/codegenv2/index.js"
                    },
                    "./experimental": "./src/experimental/index.ts"
                }
            }"#,
        )
        .unwrap();

        let entry_points = resolve_entry_points(root);

        assert!(
            entry_points.is_public_api_file(&root.join("src/codegenv2/index.ts")),
            "conditional subpath exports should remap dist/esm and .d.ts targets to source"
        );
        assert!(
            entry_points.is_public_api_file(&root.join("src/experimental/index.ts")),
            "direct source subpath exports should become public API files"
        );
    }

    #[test]
    fn remap_build_output_to_src_excludes_ambiguous_lib() {
        assert_eq!(
            remap_build_output_to_src("dist/index.js").as_deref(),
            Some("src/index.js")
        );
        assert_eq!(
            remap_build_output_to_src("build/sub/mod.js").as_deref(),
            Some("src/sub/mod.js")
        );
        // `lib` is ambiguous (often a source dir) — not remapped.
        assert_eq!(remap_build_output_to_src("lib/index.js"), None);
        // Non-build top-level dirs pass through unremapped.
        assert_eq!(remap_build_output_to_src("src/index.ts"), None);
    }

    fn write_pkg(root: &Path, dir: &str, body: &str) {
        let pkg_dir = root.join(dir);
        std::fs::create_dir_all(&pkg_dir).unwrap();
        std::fs::write(pkg_dir.join("package.json"), body).unwrap();
    }

    #[test]
    fn project_roles_classify_product_vs_tooling_from_manifests() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        // Public-API package -> product.
        write_pkg(
            root,
            "packages/lib",
            r#"{ "name": "@scope/lib", "main": "dist/index.js" }"#,
        );
        // Private scripts-only package -> tooling.
        write_pkg(
            root,
            "benchmarks/perf",
            r#"{ "name": "perf", "private": true, "scripts": { "bench": "bun run src/run.ts" } }"#,
        );
        // Real Cargo crate -> product.
        let crate_dir = root.join("crates/engine");
        std::fs::create_dir_all(&crate_dir).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"engine\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();

        let roles = resolve_project_roles(root);
        assert_eq!(
            roles.role_for("packages/lib/src/foo.ts"),
            SignalTier::Product
        );
        assert_eq!(
            roles.role_for("crates/engine/src/lib.rs"),
            SignalTier::Product
        );
        assert_eq!(
            roles.role_for("benchmarks/perf/src/run.ts"),
            SignalTier::Tooling
        );
        // Tests in a product package are still test-tier.
        assert_eq!(
            roles.role_for("packages/lib/src/__tests__/foo.test.ts"),
            SignalTier::Test
        );
        // A file under no manifest falls back to name hints.
        assert_eq!(roles.role_for("scripts/release.ts"), SignalTier::Tooling);
        assert_eq!(roles.role_for("README.md"), SignalTier::Product);
    }

    #[test]
    fn rank_and_truncate_puts_product_first_then_caps() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        write_pkg(
            root,
            "packages/lib",
            r#"{ "name": "@scope/lib", "main": "dist/index.js" }"#,
        );
        write_pkg(
            root,
            "benchmarks/perf",
            r#"{ "name": "perf", "private": true, "scripts": { "x": "bun run a.ts" } }"#,
        );
        let roles = resolve_project_roles(root);

        // Input ordered tooling-first (alphabetical, the pre-fix order).
        let items = vec![
            serde_json::json!({ "file": "benchmarks/perf/src/a.ts", "symbol": "A" }),
            serde_json::json!({ "file": "benchmarks/perf/src/b.ts", "symbol": "B" }),
            serde_json::json!({ "file": "packages/lib/src/real.ts", "symbol": "Real" }),
        ];
        let ranked = rank_and_truncate_items(items, &roles, Some(2));
        // Product surfaces first and survives the cap; one tooling item kept.
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0]["symbol"], "Real");
    }
}
