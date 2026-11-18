//! Collect the actual mutation operands, including paths hidden in patch text.

use super::*;
use crate::patch::parser::{parse_patch, Hunk};

pub(super) fn operands(request: &RawRequest) -> Result<Vec<String>, Response> {
    let params = &request.params;
    if let Some(patch) = params["patch_text"].as_str() {
        let hunks = parse_patch(patch)
            .map_err(|reason| Response::error(&request.id, errors::INVALID_REQUEST, reason))?;
        let mut paths = Vec::new();
        for hunk in hunks {
            match hunk {
                Hunk::Add { path, .. } | Hunk::Delete { path } => paths.push(path),
                Hunk::Update {
                    path, move_path, ..
                } => {
                    paths.push(path);
                    paths.extend(move_path);
                }
            }
        }
        return Ok(paths);
    }
    // Use the translated fields so collection coercion and public aliases
    // cannot hide a path from the same check the native command will receive.
    Ok(["file", "destination", "files", "paths"]
        .into_iter()
        .flat_map(|key| {
            let value = &params[key];
            value
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_else(|| std::slice::from_ref(value))
        })
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect())
}

pub(super) fn check(
    ctx: &AppContext,
    id: &str,
    session: &str,
    paths: &[String],
) -> Result<(), Response> {
    for path in paths {
        let input = Path::new(path);
        if path.contains("://") {
            return Err(confinement::refusal(id, input));
        }
        ctx.validate_read_path(id, session, input)?;
        ctx.validate_write_location(id, input)?;
    }
    Ok(())
}
