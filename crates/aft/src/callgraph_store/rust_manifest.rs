//! Committed Rust resolver inputs used to narrow manifest-only refreshes.

use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// Optional acceleration metadata, not a new store format. Older stores remain
// readable and take the conservative refresh path until they have a baseline.
const META_KEY: &str = "rust_manifest_inputs";

#[derive(Serialize, Deserialize)]
pub(super) struct Inputs {
    pub prefixes: BTreeMap<String, String>,
    targets: BTreeMap<String, Value>,
}

#[derive(Default)]
pub(super) struct Selection {
    pub callers: BTreeSet<String>,
    pub barrels: BTreeSet<String>,
    pub reextract: BTreeSet<String>,
}

impl Inputs {
    pub fn build(root: &Path) -> Self {
        let disk = DiskFacts::new(root);
        let facts = FactPaths { root, facts: &disk };
        let mut targets = BTreeMap::new();
        let prefixes = build_workspace_crate_prefixes_with_manifests(root, &facts, |manifest| {
            let fields = facts
                .attributed_bytes(manifest)
                .map(|bytes| target_fields(&bytes))
                .unwrap_or(Value::Null);
            targets.insert(relative_path(root, manifest), fields);
        });
        Self {
            prefixes: prefixes.into_iter().collect(),
            targets,
        }
    }

    pub fn load(conn: &Connection) -> Result<Option<Self>> {
        let text: Option<String> = conn
            .query_row("SELECT v FROM meta WHERE k = ?1", [META_KEY], |row| {
                row.get(0)
            })
            .optional()?;
        // Missing or damaged hints must never make an old graph appear fresh.
        Ok(text.and_then(|text| serde_json::from_str(&text).ok()))
    }

    pub fn store(&self, conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO meta(k, v) VALUES (?1, ?2)",
            params![META_KEY, serde_json::to_string(self)?],
        )?;
        Ok(())
    }

    pub fn affected_callers(
        &self,
        conn: &Connection,
        previous: Option<&Self>,
    ) -> Result<Selection> {
        let Some(previous) = previous else {
            let callers: BTreeSet<_> = stored_rust_files(conn)?.into_iter().collect();
            return Ok(Selection {
                barrels: BTreeSet::new(),
                reextract: callers.clone(),
                callers,
            });
        };
        let names: BTreeSet<_> = self
            .prefixes
            .keys()
            .chain(previous.prefixes.keys())
            .filter(|name| {
                self.prefixes.get(*name) != previous.prefixes.get(*name)
                    || self.prefixes.get(*name).is_some_and(|prefix| {
                        let file = format!("{prefix}/lib.rs");
                        self.owner(&file) != previous.owner(&file)
                    })
            })
            .collect();
        let mut selection = Selection::default();
        // Target roots and manifest presence can change local module ownership
        // without changing a crate's name. Only files owned by those manifests
        // need that conservative refresh, not their sibling crates.
        if self.targets != previous.targets {
            for file in stored_rust_files(conn)? {
                if self.owner(&file) != previous.owner(&file) {
                    selection.barrels.insert(file.clone());
                    selection.reextract.insert(file.clone());
                    selection.callers.insert(file);
                }
            }
        }
        for name in names {
            // Include negative lookups and locally resolved paths too: a new
            // crate can take precedence over a same-named local module. Imports
            // select the whole caller because its calls may use aliases or '*'.
            let mut query = conn.prepare_cached(
                "SELECT DISTINCT r.caller_file, r.kind FROM refs r
                 JOIN files f ON f.path = r.caller_file AND f.lang = 'rust'
                 WHERE substr(ltrim(r.full_ref, ':'), 1, length(?1) + 2) = ?1 || '::'
                    OR ltrim(r.module_path, ':') = ?1
                    OR substr(ltrim(r.module_path, ':'), 1, length(?1) + 2) = ?1 || '::'
                    OR substr(ltrim(r.module_path, ':'), 1, length(?1) + 4) = ?1 || ' as '",
            )?;
            let rows = query.query_map([name], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (caller, kind) = row?;
                if kind == "reexport" {
                    selection.barrels.insert(caller.clone());
                }
                selection.callers.insert(caller);
            }
        }
        Ok(selection)
    }

    fn owner(&self, file: &str) -> Option<(&str, &Value)> {
        let mut dir = file.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
        loop {
            let manifest = if dir.is_empty() {
                "Cargo.toml".to_string()
            } else {
                format!("{dir}/Cargo.toml")
            };
            if let Some((path, fields)) = self.targets.get_key_value(&manifest) {
                return Some((path, fields));
            }
            if dir.is_empty() {
                return None;
            }
            dir = dir.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
        }
    }
}

// Match read_rust_crate_targets/rust_target_roots rather than hashing all TOML:
// dependency versions and workspace membership do not participate in these
// stored-resolver lookups. Preserve parse failure as distinct from valid TOML.
fn target_fields(bytes: &[u8]) -> Value {
    let Some(cargo) = std::str::from_utf8(bytes)
        .ok()
        .and_then(|source| toml::from_str::<toml::Value>(source).ok())
    else {
        return Value::Null;
    };
    let mut fields = json!({
        "lib": cargo.get("lib").and_then(|lib| lib.get("path")).and_then(toml::Value::as_str).unwrap_or("src/lib.rs")
    });
    for (kind, auto) in [
        ("bin", "autobins"),
        ("example", "autoexamples"),
        ("test", "autotests"),
        ("bench", "autobenches"),
    ] {
        let paths: BTreeSet<_> = cargo
            .get(kind)
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|target| target.get("path").and_then(toml::Value::as_str))
            .collect();
        fields[kind] = json!(paths);
        fields[auto] = json!(cargo
            .get("package")
            .and_then(|package| package.get(auto))
            .and_then(toml::Value::as_bool)
            .unwrap_or(true));
    }
    fields
}
