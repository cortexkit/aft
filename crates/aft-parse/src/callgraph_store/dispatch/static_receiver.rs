//! Additional facts for calls that may be written on a type rather than a value.

#[path = "static_receiver/bindings.rs"]
mod bindings;
#[path = "static_receiver/codec.rs"]
mod codec;
#[path = "static_receiver/extract.rs"]
mod extract;
#[path = "static_receiver/resolve.rs"]
mod resolve;

pub(super) use extract::Index;
pub(in crate::callgraph_store) use resolve::STATIC_IMPORT_PREFIX;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct StaticFacts {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<Declaration>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    scopes: Vec<Scope>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    receivers: Vec<Receiver>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    sites: Vec<(u32, u32)>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Declaration {
    pub name: String,
    scope: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    bases: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    values: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    methods: Vec<(String, String)>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
struct Scope {
    namespace: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    imports: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    aliases: Vec<(String, String)>,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
struct Receiver {
    name: String,
    scope: u32,
    owner: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    module: Option<String>,
}

/// The written receiver and its lexical type lookup context.
#[derive(Serialize)]
pub struct StaticSiteEvidence<'a> {
    pub receiver: &'a str,
    pub enclosing_type: Option<&'a str>,
    pub namespace: &'a str,
    pub imports: &'a [String],
    pub aliases: &'a [(String, String)],
    pub import_module: Option<&'a str>,
}

impl StaticFacts {
    pub fn is_empty(&self) -> bool {
        self.types.is_empty() && self.sites.is_empty()
    }

    /// Keys the exact qualified declaration lookup by language and name.
    pub fn type_key(language: &str, name: &str) -> String {
        format!("s:{language}\t{name}")
    }

    /// The short name suffices for invalidation, including lookup misses.
    /// A site trying several namespace prefixes records just one such key.
    pub(crate) fn dependency(language: &str, name: &str) -> String {
        let short = name.rsplit('.').next().unwrap_or(name);
        format!("st:{language}\t{short}")
    }

    pub fn type_keys<'a>(&'a self, language: &'a str) -> impl Iterator<Item = String> + 'a {
        self.types.iter().filter_map(move |ty| {
            matches!(language, "java" | "csharp").then(|| Self::type_key(language, &ty.name))
        })
    }

    pub(super) fn site_key(&self, ordinal: u32) -> Option<u32> {
        let index = self
            .sites
            .binary_search_by_key(&ordinal, |(site, _)| *site)
            .ok()?;
        Some(self.sites[index].1)
    }

    pub fn site_evidence(&self, ordinal: u32) -> Option<StaticSiteEvidence<'_>> {
        let receiver = &self.receivers[self.site_key(ordinal)? as usize];
        let scope = &self.scopes[receiver.scope as usize];
        Some(StaticSiteEvidence {
            receiver: &receiver.name,
            enclosing_type: receiver
                .owner
                .map(|id| self.types[id as usize].name.as_str()),
            namespace: &scope.namespace,
            imports: &scope.imports,
            aliases: &scope.aliases,
            import_module: receiver.module.as_deref(),
        })
    }

    pub(in crate::callgraph_store) fn import_receivers(
        &self,
    ) -> impl Iterator<Item = (&str, &str)> {
        self.receivers
            .iter()
            .filter_map(|receiver| Some((receiver.name.as_str(), receiver.module.as_deref()?)))
    }

    pub(super) fn projection_names(&self) -> impl Iterator<Item = &str> {
        self.types
            .iter()
            .flat_map(|ty| ty.bases.iter().map(String::as_str))
    }

    pub(crate) fn changed_keys(
        path: &str,
        language: &str,
        before: Option<&Self>,
        after: Option<&Self>,
        keys: &mut BTreeSet<String>,
    ) {
        if before == after {
            return;
        }
        let before = Self::declarations(before);
        let after = Self::declarations(after);
        for name in before.keys().chain(after.keys()) {
            if before.get(name) != after.get(name) {
                if matches!(language, "csharp" | "java") {
                    keys.insert(Self::dependency(language, name));
                } else {
                    // TS/JS base types and imported types depend on the file containing the declaration.
                    keys.insert(super::dependency::file(path));
                }
            }
        }
    }

    fn declarations(facts: Option<&Self>) -> BTreeMap<&str, Vec<(&Declaration, &Scope)>> {
        let mut out = BTreeMap::<_, Vec<_>>::new();
        if let Some(facts) = facts {
            for ty in &facts.types {
                out.entry(ty.name.as_str())
                    .or_default()
                    .push((ty, &facts.scopes[ty.scope as usize]));
            }
        }
        out
    }
}

aft_store::heap_bytes_of_fields!(StaticFacts {
    types,
    scopes,
    receivers,
    sites
});
aft_store::heap_bytes_of_fields!(Declaration {
    name,
    scope,
    bases,
    values,
    methods
});
aft_store::heap_bytes_of_fields!(Scope {
    namespace,
    imports,
    aliases
});
aft_store::heap_bytes_of_fields!(Receiver {
    name,
    scope,
    owner,
    module
});
