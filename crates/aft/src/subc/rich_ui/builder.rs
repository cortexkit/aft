//! Deterministic manifest assembly. Production stays unpublished until the
//! reviewed templates and the phone's canonicalization vectors are pinned.

use serde_json::{json, Value};

use super::{canonical, operations, schema};

pub(crate) const CHUNK_BYTES: usize = 131_072;
pub(crate) const MANIFEST_BYTES: usize = 1_048_576;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BuildError {
    Shape,
    Operations,
    Views,
    Templates,
    DefinitionDigests,
    SourceBytes,
    ManifestBytes,
    Schema,
    Number,
}

pub(crate) struct Manifest {
    pub(crate) value: Value,
    pub(crate) bytes: Vec<u8>,
    pub(crate) digest: String,
    pub(crate) templates: Vec<Template>,
}

pub(crate) struct Template {
    pub(crate) value: Value,
    pub(crate) bytes: Vec<u8>,
    pub(crate) digest: String,
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    canonical::manifest_digest(bytes)
}

pub(crate) fn production_manifest() -> Option<Manifest> {
    None
}

pub(crate) fn template(value: Value) -> Result<Template, BuildError> {
    let source = value["source"].as_str().ok_or(BuildError::Shape)?;
    if source.len() > 65_536 {
        return Err(BuildError::SourceBytes);
    }
    let bytes = canonical::canonicalize_manifest(&value).map_err(|_| BuildError::Number)?;
    Ok(Template {
        value,
        digest: digest(&bytes),
        bytes,
    })
}

/// Only published inputs enter this function. Host configuration and the crate
/// version are deliberately not inputs: neither may change a phone's pin.
pub(crate) fn build(
    catalog: Value,
    mut operations: Vec<Value>,
    mut views: Vec<Value>,
    mut templates: Vec<Template>,
) -> Result<Manifest, BuildError> {
    if operations.len() > 64 {
        return Err(BuildError::Operations);
    }
    if views.len() > 64 {
        return Err(BuildError::Views);
    }
    if templates.len() > 128 {
        return Err(BuildError::Templates);
    }
    for operation in &operations {
        for key in [
            "args",
            if operation["kind"] == "page" {
                "item"
            } else {
                "result"
            },
        ] {
            schema::check_schema(&operation[key]).map_err(|_| BuildError::Schema)?;
        }
    }
    for view in &mut views {
        let pins = view["definitionDigests"]
            .as_array_mut()
            .ok_or(BuildError::Shape)?;
        if pins.iter().any(|pin| !pin.is_string()) {
            return Err(BuildError::Shape);
        }
        pins.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
        pins.dedup();
        if pins.len() > 16 {
            return Err(BuildError::DefinitionDigests);
        }
        if let Some(reads) = view.pointer_mut("/page/reads") {
            sort_reads(reads)?;
        }
    }
    operations.sort_by(|a, b| {
        a["id"]
            .as_str()
            .cmp(&b["id"].as_str())
            .then_with(|| a["version"].as_u64().cmp(&b["version"].as_u64()))
    });
    views.sort_by(|a, b| a["tool"].as_str().cmp(&b["tool"].as_str()));
    templates.sort_by(|a, b| a.digest.cmp(&b.digest));
    let descriptors: Vec<_> = templates
        .iter()
        .map(|t| {
            json!({
                "digest": t.digest, "profile": t.value["profile"],
                "bytes": t.bytes.len(), "chunks": t.bytes.len().div_ceil(CHUNK_BYTES),
            })
        })
        .collect();
    let value = json!({
        "schema": "ckios.richui.manifest/1", "provider": {"module": "aft"},
        "providerVersion": operations::RICH_UI_PROVIDER_VERSION, "dialects": ["1.0"],
        "catalog": catalog, "operations": operations, "views": views, "templates": descriptors,
    });
    let bytes = canonical::canonicalize_manifest(&value).map_err(|_| BuildError::Number)?;
    if bytes.len() > MANIFEST_BYTES {
        return Err(BuildError::ManifestBytes);
    }
    Ok(Manifest {
        digest: digest(&bytes),
        value,
        bytes,
        templates,
    })
}

fn sort_reads(value: &mut Value) -> Result<(), BuildError> {
    let reads = value.as_array_mut().ok_or(BuildError::Shape)?;
    reads.sort_by(|a, b| {
        a["id"]
            .as_str()
            .cmp(&b["id"].as_str())
            .then_with(|| a["version"].as_u64().cmp(&b["version"].as_u64()))
    });
    Ok(())
}

#[cfg(test)]
mod tests;
