use super::{cache_location_error_for_map, parse_cache_dir, ConfigTier, RawCacheDir};
use crate::{feature_config, jsonc::strip_jsonc};
use serde_json::{Map, Value};

/// One file's parsed JSON and retired-key translations, before selecting
/// the active editor's overrides or merging user and project settings.
pub(crate) struct ParsedConfigDocument {
    pub(super) translated: Option<(Map<String, Value>, feature_config::DocumentTranslation)>,
    pub(super) removed_keys: Vec<String>,
    pub(super) cache_location_error: Option<String>,
    pub(super) cache_choice_after_rejection: Option<RawCacheDir>,
}

impl ParsedConfigDocument {
    pub(crate) fn new(tier: &ConfigTier) -> Self {
        let is_user = tier.tier == "user";
        let mut map = match parse_config_object(tier) {
            Ok(map) => map,
            Err(error) => {
                return Self {
                    translated: None,
                    removed_keys: Vec::new(),
                    cache_location_error: is_user.then_some(error),
                    cache_choice_after_rejection: None,
                };
            }
        };
        let removed_keys = feature_config::removed_config_key_errors_for_file(&map, is_user);
        let cache_location_error = is_user
            .then(|| cache_location_error_for_map(tier, &map))
            .flatten();
        // A rejected candidate still carries the cache intent from the original
        // user document so caches cannot silently fall back to the storage root.
        let cache_choice_after_rejection = is_user
            .then(|| parse_cache_dir(map.get("storage")?.get("cache_dir")?.clone()).ok())
            .flatten();
        let translation = feature_config::translate_document(
            &mut map,
            if is_user {
                feature_config::DocumentTier::User
            } else {
                feature_config::DocumentTier::Project
            },
        );
        Self {
            translated: Some((map, translation)),
            removed_keys,
            cache_location_error,
            cache_choice_after_rejection,
        }
    }
}

pub(super) fn parse_config_object(tier: &ConfigTier) -> Result<Map<String, Value>, String> {
    let value = serde_json::from_str::<Value>(&strip_jsonc(&tier.doc))
        .map_err(|error| format!("{} does not parse: {error}", tier.source))?;
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(format!("{} is not a JSON object", tier.source)),
    }
}
