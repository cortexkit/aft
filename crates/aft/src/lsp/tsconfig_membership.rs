use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use serde_json::Value;

use crate::jsonc::strip_jsonc;
use crate::lsp::roots::find_workspace_root;

const TSCONFIG_JSON: &str = "tsconfig.json";
const MAX_EXTENDS_DEPTH: usize = 16;
const TS_JS_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "d.ts", "js", "jsx", "mjs", "cjs", "mts", "cts", "d.mts", "d.cts",
];

/// Per-inspect-call cache for TypeScript project membership decisions.
///
/// `typescript-language-server` falls back to an inferred project when AFT opens
/// a TS/JS file that is excluded from the nearest tsconfig. That inferred project
/// does not inherit the build's `types`, `paths`, or other compiler options, so
/// diagnostics can diverge from `tsc -p`. This cache resolves the nearest
/// tsconfig once and lets callers suppress diagnostics for files that the build
/// would not check.
#[derive(Default)]
pub(crate) struct TsconfigMembershipCache {
    projects: HashMap<PathBuf, Option<ResolvedTsConfig>>,
    canonical_files: HashMap<PathBuf, PathBuf>,
    file_projects: HashMap<PathBuf, Option<PathBuf>>,
    clear_generation: u64,
}

impl TsconfigMembershipCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Drop all memoized project resolutions. Called when a tsconfig-like file
    /// changes (watcher) or on `configure`, so the next membership query
    /// re-reads from disk. A wholesale clear is intentional: nearest-tsconfig
    /// resolution and `extends` chains make per-key invalidation unsound (a new
    /// nested tsconfig or an edited `extends` parent changes membership for
    /// files keyed under a different directory).
    pub(crate) fn clear(&mut self) {
        self.projects.clear();
        self.canonical_files.clear();
        self.file_projects.clear();
        self.clear_generation = self.clear_generation.wrapping_add(1);
    }

    pub(crate) fn generation(&self) -> u64 {
        self.clear_generation
    }

    pub(crate) fn should_skip_diagnostics(&mut self, file: &Path) -> bool {
        if !is_ts_js_file(file) {
            return false;
        }

        let canonical_file = self
            .canonical_files
            .entry(file.to_path_buf())
            .or_insert_with(|| canonical_or_normalized(file))
            .clone();
        let tsconfig_dir = self
            .file_projects
            .entry(canonical_file.clone())
            .or_insert_with(|| find_workspace_root(&canonical_file, &[TSCONFIG_JSON]))
            .clone();
        let Some(tsconfig_dir) = tsconfig_dir else {
            return false;
        };

        let project = self
            .projects
            .entry(tsconfig_dir.clone())
            .or_insert_with(|| load_project(&tsconfig_dir));

        match project {
            Some(project) => !project.contains_canonical(&canonical_file),
            None => false,
        }
    }
}

#[derive(Debug)]
struct ResolvedTsConfig {
    files: Vec<PathBuf>,
    include: PatternGroup,
    exclude: PatternGroup,
}

impl ResolvedTsConfig {
    fn contains_canonical(&self, file: &Path) -> bool {
        if self.files.iter().any(|member| member == file) {
            return true;
        }

        self.include.is_match(file) && !self.exclude.is_match(file)
    }
}

#[derive(Debug)]
struct PatternGroup {
    groups: Vec<OriginGlobSet>,
}

impl PatternGroup {
    fn new(groups: Vec<OriginGlobSet>) -> Self {
        Self { groups }
    }

    fn is_match(&self, file: &Path) -> bool {
        self.groups.iter().any(|group| group.is_match(file))
    }
}

#[derive(Debug)]
struct OriginGlobSet {
    origin_dir: PathBuf,
    glob_set: GlobSet,
}

impl OriginGlobSet {
    fn is_match(&self, file: &Path) -> bool {
        let Ok(relative) = file.strip_prefix(&self.origin_dir) else {
            return false;
        };
        self.glob_set.is_match(relative)
    }
}

#[derive(Debug, Clone)]
struct Field<T> {
    origin_dir: PathBuf,
    value: T,
}

#[derive(Debug, Clone, Default)]
struct RawTsConfig {
    extends: Vec<String>,
    files: Option<Vec<String>>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    out_dir: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ResolvedFields {
    files: Option<Field<Vec<String>>>,
    include: Option<Field<Vec<String>>>,
    exclude: Option<Field<Vec<String>>>,
    out_dir: Option<Field<String>>,
}

fn load_project(tsconfig_dir: &Path) -> Option<ResolvedTsConfig> {
    let tsconfig_path = tsconfig_dir.join(TSCONFIG_JSON);
    let mut visiting = HashSet::new();
    match resolve_tsconfig_fields(&tsconfig_path, 0, &mut visiting) {
        Ok(fields) => build_resolved_config(tsconfig_dir, fields),
        Err(message) => {
            crate::slog_warn!(
                "[inspect:diagnostics] unable to resolve {}: {message}",
                tsconfig_path.display()
            );
            None
        }
    }
}

fn resolve_tsconfig_fields(
    tsconfig_path: &Path,
    depth: usize,
    visiting: &mut HashSet<PathBuf>,
) -> Result<ResolvedFields, String> {
    if depth > MAX_EXTENDS_DEPTH {
        return Err(format!(
            "tsconfig extends depth exceeded {MAX_EXTENDS_DEPTH} at {}",
            tsconfig_path.display()
        ));
    }

    let tsconfig_path = canonical_or_normalized(tsconfig_path);
    if !visiting.insert(tsconfig_path.clone()) {
        return Err(format!(
            "tsconfig extends cycle involving {}",
            tsconfig_path.display()
        ));
    }

    let raw = parse_tsconfig(&tsconfig_path)?;
    let origin_dir = tsconfig_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut resolved = ResolvedFields::default();
    for extends in &raw.extends {
        let parent = resolve_extends_path(&origin_dir, extends).ok_or_else(|| {
            format!(
                "unsupported or missing tsconfig extends '{extends}' from {}",
                tsconfig_path.display()
            )
        })?;
        let fields = resolve_tsconfig_fields(&parent, depth + 1, visiting)?;
        if fields.files.is_some() {
            resolved.files = fields.files;
        }
        if fields.include.is_some() {
            resolved.include = fields.include;
        }
        if fields.exclude.is_some() {
            resolved.exclude = fields.exclude;
        }
        if fields.out_dir.is_some() {
            resolved.out_dir = fields.out_dir;
        }
    }

    if let Some(files) = raw.files {
        resolved.files = Some(Field {
            origin_dir: origin_dir.clone(),
            value: files,
        });
    }
    if let Some(include) = raw.include {
        resolved.include = Some(Field {
            origin_dir: origin_dir.clone(),
            value: include,
        });
    }
    if let Some(exclude) = raw.exclude {
        resolved.exclude = Some(Field {
            origin_dir: origin_dir.clone(),
            value: exclude,
        });
    }
    if let Some(out_dir) = raw.out_dir {
        resolved.out_dir = Some(Field {
            origin_dir,
            value: out_dir,
        });
    }

    visiting.remove(&tsconfig_path);
    Ok(resolved)
}

fn parse_tsconfig(tsconfig_path: &Path) -> Result<RawTsConfig, String> {
    let source = fs::read_to_string(tsconfig_path)
        .map_err(|err| format!("read {}: {err}", tsconfig_path.display()))?;
    let stripped = strip_jsonc(&source);
    let value = serde_json::from_str::<Value>(&stripped)
        .map_err(|err| format!("parse {}: {err}", tsconfig_path.display()))?;

    Ok(RawTsConfig {
        extends: match value.get("extends") {
            Some(Value::String(single)) => vec![single.clone()],
            Some(Value::Array(array)) => array
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        },
        files: string_array_field(&value, "files"),
        include: string_array_field(&value, "include"),
        exclude: string_array_field(&value, "exclude"),
        out_dir: value
            .get("compilerOptions")
            .and_then(|compiler_options| string_field(compiler_options, "outDir")),
    })
}

fn build_resolved_config(tsconfig_dir: &Path, fields: ResolvedFields) -> Option<ResolvedTsConfig> {
    let files = fields
        .files
        .as_ref()
        .map(|field| {
            field
                .value
                .iter()
                .map(|file| canonical_or_normalized(&field.origin_dir.join(file)))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let include = if let Some(field) = fields.include.as_ref() {
        PatternGroup::new(vec![compile_origin_globs(&field.origin_dir, &field.value)?])
    } else if fields.files.is_some() {
        // TypeScript semantics: when `files` is specified and `include` is
        // absent, ONLY the listed files are members — `include` does NOT default
        // to `**/*`. Defaulting it here made a files-only tsconfig match every
        // file, so build-excluded files leaked into the status-bar / aft_inspect
        // diagnostic counts. An empty include matches nothing; `files` membership
        // is still handled by the explicit list in ResolvedTsConfig::contains.
        PatternGroup::new(Vec::new())
    } else {
        PatternGroup::new(vec![compile_origin_globs(
            tsconfig_dir,
            &["**/*".to_string()],
        )?])
    };

    let exclude = if let Some(field) = fields.exclude.as_ref() {
        PatternGroup::new(vec![compile_origin_globs(&field.origin_dir, &field.value)?])
    } else {
        let mut defaults = vec![
            "node_modules".to_string(),
            "bower_components".to_string(),
            "jspm_packages".to_string(),
        ];
        if let Some(out_dir) = fields.out_dir.as_ref() {
            defaults.push(path_relative_to(
                tsconfig_dir,
                &out_dir.origin_dir.join(&out_dir.value),
            ));
        }
        PatternGroup::new(vec![compile_origin_globs(tsconfig_dir, &defaults)?])
    };

    Some(ResolvedTsConfig {
        files,
        include,
        exclude,
    })
}

fn compile_origin_globs(origin_dir: &Path, patterns: &[String]) -> Option<OriginGlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let Some(pattern) = ts_pattern_to_glob(pattern) else {
            continue;
        };
        match glob(&pattern) {
            Ok(glob) => {
                builder.add(glob);
            }
            Err(err) => {
                crate::slog_warn!(
                    "[inspect:diagnostics] invalid tsconfig glob '{}' from {}: {err}",
                    pattern,
                    origin_dir.display()
                );
                continue;
            }
        }
    }

    let glob_set = match builder.build() {
        Ok(glob_set) => glob_set,
        Err(err) => {
            crate::slog_warn!(
                "[inspect:diagnostics] failed to build tsconfig glob set from {}: {err}",
                origin_dir.display()
            );
            return None;
        }
    };

    Some(OriginGlobSet {
        origin_dir: canonical_or_normalized(origin_dir),
        glob_set,
    })
}

fn glob(pattern: &str) -> Result<Glob, globset::Error> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(true)
        .build()
}

fn ts_pattern_to_glob(pattern: &str) -> Option<String> {
    let trimmed = pattern.trim().replace('\\', "/");
    if trimmed.is_empty() {
        return None;
    }

    let trimmed = trimmed.trim_start_matches("./").trim_end_matches('/');
    if trimmed.is_empty() {
        return Some("**/*".to_string());
    }

    if !has_wildcard(trimmed) && Path::new(trimmed).extension().is_none() {
        return Some(format!("{trimmed}/**/*"));
    }

    Some(trimmed.to_string())
}

fn resolve_extends_path(origin_dir: &Path, extends: &str) -> Option<PathBuf> {
    let raw = extends.trim();
    if raw.is_empty() {
        return None;
    }

    let raw_path = Path::new(raw);
    if raw_path.is_absolute()
        || raw.starts_with("./")
        || raw.starts_with("../")
        || raw == "."
        || raw == ".."
    {
        let base = if raw_path.is_absolute() {
            raw_path.to_path_buf()
        } else {
            origin_dir.join(raw_path)
        };
        return find_extends_candidate(&base);
    }

    // TypeScript resolves package extends with nodeNextJsonConfigResolver:
    // https://github.com/microsoft/TypeScript/blob/v5.9.3/src/compiler/commandLineParser.ts#L3612-L3644
    // Reject traversal before joining an untrusted specifier.
    let normalized = raw.replace('\\', "/");
    if normalized
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
        || normalized.starts_with('/')
    {
        return None;
    }
    let mut directory = origin_dir;
    loop {
        let node_modules = directory.join("node_modules");
        // A symlinked node_modules must not turn an ancestor lookup into a
        // read outside that ancestor's tree.
        if node_modules.exists()
            && !canonical_or_normalized(&node_modules)
                .starts_with(canonical_or_normalized(directory))
        {
            directory = directory.parent()?;
            continue;
        }
        let base = node_modules.join(&normalized);
        if let Some(found) = find_package_extends_candidate(&base, &normalized, &node_modules) {
            return Some(found);
        }
        directory = directory.parent()?;
    }
}

fn find_extends_candidate(base: &Path) -> Option<PathBuf> {
    extends_candidates(base)
        .into_iter()
        .find(|candidate| candidate.is_file())
        .map(|candidate| canonical_or_normalized(&candidate))
}

fn find_package_extends_candidate(
    base: &Path,
    specifier: &str,
    node_modules: &Path,
) -> Option<PathBuf> {
    // TypeScript checks package.json's `tsconfig` field before tsconfig.json:
    // https://github.com/microsoft/TypeScript/blob/v5.9.3/src/compiler/moduleNameResolver.ts#L2481-L2544
    // Scoped names have two segments; unscoped names have one.
    let parts: Vec<_> = specifier.split('/').collect();
    let package_parts = if parts.first()?.starts_with('@') {
        2
    } else {
        1
    };
    if parts.len() == package_parts {
        let package_dir = parts
            .iter()
            .take(package_parts)
            .fold(node_modules.to_path_buf(), |dir, part| dir.join(part));
        let package_json = package_dir.join("package.json");
        if canonical_or_normalized(&package_dir).starts_with(canonical_or_normalized(node_modules))
            && canonical_or_normalized(&package_json)
                .starts_with(canonical_or_normalized(node_modules))
        {
            if let Ok(source) = fs::read_to_string(package_json) {
                if let Ok(value) = serde_json::from_str::<Value>(&source) {
                    if let Some(field) = string_field(&value, "tsconfig") {
                        let target = package_dir.join(field);
                        if let Some(found) = find_extends_candidate(&target) {
                            if found.starts_with(canonical_or_normalized(node_modules)) {
                                return Some(found);
                            }
                        }
                    }
                }
            }
        }
    }
    find_extends_candidate(base)
        .filter(|found| found.starts_with(canonical_or_normalized(node_modules)))
}

fn extends_candidates(base: &Path) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    candidates.push(base.to_path_buf());

    if base.extension().and_then(|extension| extension.to_str()) != Some("json") {
        let mut with_json = base.as_os_str().to_os_string();
        with_json.push(".json");
        candidates.push(PathBuf::from(with_json));
    }

    if base.extension().is_none() {
        candidates.push(base.join(TSCONFIG_JSON));
    }

    candidates
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn string_array_field(value: &Value, key: &str) -> Option<Vec<String>> {
    let array = value.get(key)?.as_array()?;
    Some(
        array
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

fn has_wildcard(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?')
}

fn is_ts_js_file(path: &Path) -> bool {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    TS_JS_EXTENSIONS
        .iter()
        .any(|extension| filename.ends_with(&format!(".{extension}")))
}

fn path_relative_to(base: &Path, path: &Path) -> String {
    let normalized_base = canonical_or_normalized(base);
    let normalized_path = canonical_or_normalized(path);
    normalized_path
        .strip_prefix(&normalized_base)
        .unwrap_or(&normalized_path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn canonical_or_normalized(path: &Path) -> PathBuf {
    // Both branches must agree on verbatim form: bare canonicalize returns
    // \?\-prefixed paths on Windows while the lexical fallback is clean, so
    // membership decisions flipped with filesystem luck. Route the canonical
    // branch through the same verbatim-stripping normalizer.
    crate::inspect::job::canonicalize_normalized(path)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::TsconfigMembershipCache;

    fn write(path: &std::path::Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn skips_file_excluded_by_nearest_tsconfig() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("tsconfig.json"),
            r#"{
              "include": ["src/**/*.ts"],
              "exclude": ["src/**/*.test.ts"],
            }"#,
        );
        let test_file = root.join("src/foo.test.ts");
        let src_file = root.join("src/foo.ts");
        write(&test_file, "test('x', () => {});\n");
        write(&src_file, "export const x = 1;\n");

        let mut cache = TsconfigMembershipCache::new();
        assert!(cache.should_skip_diagnostics(&test_file));
        assert!(!cache.should_skip_diagnostics(&src_file));
    }

    #[test]
    fn files_are_not_subject_to_exclude() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("tsconfig.json"),
            r#"{
              "files": ["src/foo.test.ts"],
              "exclude": ["src/**/*.test.ts"]
            }"#,
        );
        let test_file = root.join("src/foo.test.ts");
        write(&test_file, "export const x = 1;\n");

        let mut cache = TsconfigMembershipCache::new();
        assert!(!cache.should_skip_diagnostics(&test_file));
    }

    #[test]
    fn files_only_tsconfig_excludes_unlisted_files() {
        // `files` present + `include` absent → ONLY the listed files are members;
        // `include` must NOT default to **/*. An unlisted sibling should be
        // skipped (not counted in diagnostics).
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("tsconfig.json"),
            r#"{
              "files": ["src/entry.ts"]
            }"#,
        );
        let listed = root.join("src/entry.ts");
        let unlisted = root.join("src/other.ts");
        write(&listed, "export const a = 1;\n");
        write(&unlisted, "export const b = 2;\n");

        let mut cache = TsconfigMembershipCache::new();
        // Listed file is a member (not skipped).
        assert!(!cache.should_skip_diagnostics(&listed));
        // Unlisted file is NOT a member → skipped.
        assert!(cache.should_skip_diagnostics(&unlisted));
    }

    #[test]
    fn bare_package_extends_filters_outside_include() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("node_modules/@tsconfig/bun/tsconfig.json"),
            "{}\n",
        );
        write(
            &root.join("tsconfig.json"),
            r#"{"extends":"@tsconfig/bun/tsconfig.json","include":["src"]}"#,
        );
        let included = root.join("src/a.ts");
        let excluded = root.join("scripts/b.ts");
        write(&included, "export const a = 1;\n");
        write(&excluded, "export const b = 2;\n");
        let mut cache = TsconfigMembershipCache::new();
        assert!(!cache.should_skip_diagnostics(&included));
        assert!(cache.should_skip_diagnostics(&excluded));
    }

    #[test]
    fn bare_package_extends_searches_parent_node_modules() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("node_modules/config/tsconfig.json"), "{}\n");
        write(
            &root.join("packages/pkg/tsconfig.json"),
            r#"{"extends":"config","include":["src"]}"#,
        );
        let excluded = root.join("packages/pkg/scripts/b.ts");
        write(&excluded, "export const b = 2;\n");
        assert!(TsconfigMembershipCache::new().should_skip_diagnostics(&excluded));
    }

    #[test]
    fn package_json_tsconfig_field_precedes_default() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("node_modules/config/package.json"),
            r#"{"tsconfig":"configs/base.json"}"#,
        );
        write(
            &root.join("node_modules/config/configs/base.json"),
            r#"{"files":["src/a.ts"]}"#,
        );
        write(
            &root.join("node_modules/config/tsconfig.json"),
            r#"{"include":["scripts"]}"#,
        );
        write(&root.join("tsconfig.json"), r#"{"extends":"config"}"#);
        let included = root.join("node_modules/config/configs/src/a.ts");
        let excluded = root.join("scripts/b.ts");
        write(&included, "export const a = 1;\n");
        write(&excluded, "export const b = 2;\n");
        let fields =
            super::resolve_tsconfig_fields(&root.join("tsconfig.json"), 0, &mut Default::default())
                .unwrap();
        let project = super::build_resolved_config(root, fields).unwrap();
        // The inherited files list is relative to its own config, not the child.
        assert!(project.contains_canonical(&super::canonical_or_normalized(&included)));
        assert!(!project.contains_canonical(&super::canonical_or_normalized(&excluded)));
    }

    #[test]
    fn array_extends_later_parent_overrides_earlier_fields() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("node_modules/first/tsconfig.json"),
            r#"{"files":["a.ts"]}"#,
        );
        write(
            &root.join("node_modules/second/tsconfig.json"),
            r#"{"files":["b.ts"]}"#,
        );
        write(
            &root.join("tsconfig.json"),
            r#"{"extends":["first","second"]}"#,
        );
        let a = root.join("node_modules/first/a.ts");
        let b = root.join("node_modules/second/b.ts");
        write(&a, "export const a = 1;\n");
        write(&b, "export const b = 2;\n");
        let fields =
            super::resolve_tsconfig_fields(&root.join("tsconfig.json"), 0, &mut Default::default())
                .unwrap();
        let project = super::build_resolved_config(root, fields).unwrap();
        assert!(!project.contains_canonical(&super::canonical_or_normalized(&a)));
        assert!(project.contains_canonical(&super::canonical_or_normalized(&b)));
    }

    #[test]
    fn missing_bare_package_retains_warning_and_fallback() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("tsconfig.json"),
            r#"{"extends":"not-installed","include":["src"]}"#,
        );
        let excluded = root.join("scripts/b.ts");
        write(&excluded, "export const b = 2;\n");
        let error =
            super::resolve_tsconfig_fields(&root.join("tsconfig.json"), 0, &mut Default::default())
                .unwrap_err();
        assert!(
            error.contains("unsupported or missing tsconfig extends 'not-installed'"),
            "{error}"
        );
        assert!(!TsconfigMembershipCache::new().should_skip_diagnostics(&excluded));
    }

    #[test]
    fn bare_package_windows_separators_and_traversal() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("node_modules/@tsconfig/bun/tsconfig.json"),
            "{}\n",
        );
        assert!(super::resolve_extends_path(root, "@tsconfig\\bun\\tsconfig.json").is_some());
        assert!(super::resolve_extends_path(root, "@tsconfig\\..\\outside").is_none());
    }

    #[test]
    fn malformed_tsconfig_falls_through() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        write(&root.join("tsconfig.json"), "{ not valid jsonc");
        let file = root.join("src/foo.test.ts");
        write(&file, "export const x = 1;\n");

        let mut cache = TsconfigMembershipCache::new();
        assert!(!cache.should_skip_diagnostics(&file));
    }
}
