use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::compress::toml_filter::{parse_filter, FilterSource, TomlFilter};
use crate::context::AppContext;
use crate::harness::Harness;
use crate::protocol::{RawRequest, Response};

#[derive(Debug, Serialize)]
struct FilterEntry {
    name: String,
    source: String,
    source_path: Option<String>,
    matches: Vec<String>,
    description: Option<String>,
    content: String,
    trusted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub fn handle_list_filters(req: &RawRequest, ctx: &AppContext) -> Response {
    let config = ctx.config();
    let storage_dir = config.storage_dir.clone();
    let project_root = config.project_root.clone();
    drop(config);

    let trusted_projects = storage_dir
        .as_ref()
        .map(|dir| crate::compress::trust::list_trusted(dir))
        .unwrap_or_default();
    let harness = ctx.harness.lock().clone().unwrap_or(Harness::Opencode);
    let user_dir = storage_dir.as_ref().map(|dir| {
        crate::compress::repair_legacy_user_filter_dir(dir, harness.clone());
        crate::compress::user_filter_dir(dir, harness)
    });
    let project_dir = project_root
        .as_ref()
        .map(|root| crate::compress::project_filter_dir(root));
    let project_trusted = match (&storage_dir, &project_root) {
        (Some(storage), Some(root)) => {
            crate::compress::trust::is_project_trusted(Some(storage), root)
        }
        _ => false,
    };

    let mut filters = Vec::new();
    for (name, content) in crate::compress::builtin_filters::ALL {
        match parse_filter(name, content, FilterSource::Builtin) {
            Ok(filter) => filters.push(entry_from_filter(filter, (*content).to_string(), None)),
            Err(error) => filters.push(FilterEntry {
                name: (*name).to_string(),
                source: "builtin_invalid".to_string(),
                source_path: None,
                matches: vec![(*name).to_string()],
                description: None,
                content: (*content).to_string(),
                trusted: None,
                error: Some(error),
            }),
        }
    }

    if let Some(dir) = user_dir.as_deref() {
        filters.extend(read_filter_dir(
            dir,
            "user_invalid",
            |path| FilterSource::User {
                path: path.to_path_buf(),
            },
            None,
        ));
    }

    if let Some(dir) = project_dir.as_deref() {
        filters.extend(read_filter_dir(
            dir,
            "project_invalid",
            |path| FilterSource::Project {
                path: path.to_path_buf(),
            },
            Some(project_trusted),
        ));
    }

    Response::success(
        &req.id,
        serde_json::json!({
            "filters": filters,
            "user_dir": user_dir.map(|path| path.display().to_string()),
            "project_dir": project_dir.map(|path| path.display().to_string()),
            "trusted_projects": trusted_projects,
        }),
    )
}

fn read_filter_dir<F>(
    dir: &Path,
    invalid_source: &str,
    source_for: F,
    trusted: Option<bool>,
) -> Vec<FilterEntry>
where
    F: Fn(&Path) -> FilterSource,
{
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("toml"))
        .collect();
    paths.sort();

    let mut filters = Vec::new();
    for path in paths {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                filters.push(invalid_entry(
                    name,
                    invalid_source,
                    &path,
                    String::new(),
                    error.to_string(),
                    trusted,
                ));
                continue;
            }
        };
        match parse_filter(&name, &content, source_for(&path)) {
            Ok(filter) => filters.push(entry_from_filter(filter, content, trusted)),
            Err(error) => filters.push(invalid_entry(
                name,
                invalid_source,
                &path,
                content,
                error,
                trusted,
            )),
        }
    }
    filters
}

fn entry_from_filter(
    filter: TomlFilter,
    content: String,
    trusted_override: Option<bool>,
) -> FilterEntry {
    let (source, source_path, trusted) = match filter.source {
        FilterSource::Builtin => ("builtin".to_string(), None, None),
        FilterSource::User { path } => ("user".to_string(), Some(path.display().to_string()), None),
        FilterSource::Project { path } => (
            "project".to_string(),
            Some(path.display().to_string()),
            trusted_override,
        ),
    };
    FilterEntry {
        name: filter.name,
        source,
        source_path,
        matches: filter.matches,
        description: filter.description,
        content,
        trusted,
        error: None,
    }
}

fn invalid_entry(
    name: String,
    source: &str,
    path: &Path,
    content: String,
    error: String,
    trusted: Option<bool>,
) -> FilterEntry {
    FilterEntry {
        name,
        source: source.to_string(),
        source_path: Some(path.display().to_string()),
        matches: Vec::new(),
        description: None,
        content,
        trusted,
        error: Some(error),
    }
}
