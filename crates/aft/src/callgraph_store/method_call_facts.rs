//! What method-call resolution reads about one file: its [`FileFacts`].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::callgraph_store::join::{
    self,
    dispatch::{dependency, DispatchFacts, FieldHint, MethodHint, ReturnHint, TypeHint},
    BlobImport, ParseBlob,
};

/// The import fields method-call resolution reads.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ImportFact {
    module_path: String,
    names: Vec<String>,
    default_import: Option<String>,
}

/// Everything method-call resolution reads about one file, apart from its
/// call sites. Positions are left out, so an edit that only moves code keeps
/// a file's facts, and the resolutions that read them, unchanged.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct FileFacts {
    pub(crate) language: String,
    #[serde(default)]
    go_package: Option<String>,
    types: Vec<TypeHint>,
    methods: Vec<MethodHint>,
    fields: Vec<FieldHint>,
    returns: Vec<ReturnHint>,
    imports: Vec<ImportFact>,
    default_export_symbol: Option<String>,
    /// Target file of each resolved import, by module path.
    pub(crate) import_targets: BTreeMap<String, String>,
    /// Target file and symbol of each Rust receiver type name.
    pub(crate) type_targets: BTreeMap<String, (String, String)>,
}

#[derive(Default, PartialEq)]
struct NamedFacts<'a> {
    types: Vec<&'a TypeHint>,
    target: Option<&'a (String, String)>,
    imports: Vec<(&'a str, Vec<&'a str>, bool, Option<&'a String>)>,
}

fn grouped<'a, T, K: Ord>(items: &'a [T], key: impl Fn(&'a T) -> K) -> BTreeMap<K, Vec<&'a T>> {
    let mut groups = BTreeMap::<K, Vec<&T>>::new();
    for item in items {
        groups.entry(key(item)).or_default().push(item);
    }
    groups
}

fn changed_groups<K: Ord, V: PartialEq>(
    old: &BTreeMap<K, V>,
    new: &BTreeMap<K, V>,
    mut emit: impl FnMut(&K),
) {
    for key in old.keys().chain(new.keys()) {
        if old.get(key) != new.get(key) {
            emit(key);
        }
    }
}

impl FileFacts {
    pub(crate) fn new(parse: &ParseBlob) -> Self {
        Self {
            language: parse.language.clone(),
            go_package: parse.dispatch.go_package.clone(),
            types: parse.dispatch.types.clone(),
            methods: parse.dispatch.methods.clone(),
            fields: parse.dispatch.fields.clone(),
            returns: parse.dispatch.returns.clone(),
            imports: if parse.language == "python" {
                join::dispatch::python_reexports(parse)
                    .into_iter()
                    .map(|(module_path, names)| ImportFact {
                        module_path,
                        names,
                        default_import: None,
                    })
                    .collect()
            } else {
                parse
                    .imports
                    .iter()
                    .map(|import| ImportFact {
                        module_path: import.module_path.clone(),
                        names: import.names.clone(),
                        default_import: import.default_import.clone(),
                    })
                    .collect()
            },
            default_export_symbol: parse.default_export_symbol.clone(),
            import_targets: BTreeMap::new(),
            type_targets: BTreeMap::new(),
        }
    }

    /// A parse blob carrying only what [`Resolver`] reads. Fields the resolver
    /// never reads (symbols, references, AST nodes, import positions, sites)
    /// are left empty.
    pub(crate) fn resolver_parse(&self) -> ParseBlob {
        ParseBlob {
            entry_point_symbols: Vec::new(),
            dispatch: DispatchFacts {
                go_package: self.go_package.clone(),
                types: self.types.clone(),
                methods: self.methods.clone(),
                sites: Vec::new(),
                fields: self.fields.clone(),
                returns: self.returns.clone(),
            },
            extractor_version: String::new(),
            language: self.language.clone(),
            ast_nodes: Vec::new(),
            symbols: Vec::new(),
            default_export_symbol: self.default_export_symbol.clone(),
            exported_symbols: Vec::new(),
            callable_symbols: Vec::new(),
            python_bindings: Vec::new(),
            imports: self
                .imports
                .iter()
                .map(|import| BlobImport {
                    ordinal: 0,
                    module_path: import.module_path.clone(),
                    names: import.names.clone(),
                    default_import: import.default_import.clone(),
                    namespace_import: None,
                    byte_start: 0,
                    byte_end: 0,
                    raw_text: String::new(),
                    type_only: false,
                    side_effect: false,
                })
                .collect(),
            refs: Vec::new(),
        }
    }

    /// The keys of every resolution that may read this file's facts, when the
    /// facts change from `old` to `new` (either side may be absent).
    pub(crate) fn changed_keys(
        path: &str,
        old: Option<&Self>,
        new: Option<&Self>,
        keys: &mut BTreeSet<String>,
    ) {
        if old == new {
            return;
        }
        keys.insert(dependency::file(path));
        keys.insert(dependency::directory(path));
        for facts in old.into_iter().chain(new) {
            // A resolution that found this file through a method name recorded
            // only that name, so every method name of the file is affected.
            keys.extend(
                facts
                    .methods
                    .iter()
                    .map(|method| dependency::method(&method.name)),
            );
            if facts.language == "go" {
                keys.insert(dependency::GO_INTERFACES.to_string());
            }
        }
    }

    /// Diff name-addressed reads when the file keeps its identity and language.
    /// Go interface checks read whole method sets, so Go retains the coarse diff.
    pub(crate) fn changed_keys_by_method(
        path: &str,
        old: Option<&Self>,
        new: Option<&Self>,
        keys: &mut BTreeSet<String>,
    ) {
        if old == new {
            return;
        }
        let (Some(old), Some(new)) = (old, new) else {
            return Self::changed_keys(path, old, new, keys);
        };
        if old.language == "go" || old.language != new.language || old.go_package != new.go_package
        {
            return Self::changed_keys(path, Some(old), Some(new), keys);
        }
        let mut named = Vec::new();
        changed_groups(
            &grouped(&old.methods, |m| m.name.as_str()),
            &grouped(&new.methods, |m| m.name.as_str()),
            |name| named.push(dependency::method(name)),
        );
        let (old_named, new_named) = (old.named_facts(), new.named_facts());
        changed_groups(&old_named, &new_named, |name| {
            named.push(dependency::type_name(path, name));
        });
        if old.default_export_symbol != new.default_export_symbol {
            named.push(dependency::type_name(path, "@default"));
        }
        changed_groups(
            &grouped(&old.types, |t| t.name.as_str()),
            &grouped(&new.types, |t| t.name.as_str()),
            |name| named.push(dependency::directory_type(path, name)),
        );
        changed_groups(
            &grouped(&old.fields, |f| (f.owner.as_str(), f.name.as_str())),
            &grouped(&new.fields, |f| (f.owner.as_str(), f.name.as_str())),
            |(owner, name)| named.push(dependency::field(path, owner, name)),
        );
        changed_groups(
            &grouped(&old.returns, |r| r.symbol.as_str()),
            &grouped(&new.returns, |r| r.symbol.as_str()),
            |symbol| named.push(dependency::return_hint(path, symbol)),
        );
        // The facts differ, so something a resolution may read changed. If no
        // name above accounts for it (for example the target of an import that
        // binds no local name), fall back to the whole-file keys rather than
        // invalidate nothing.
        if named.is_empty() {
            return Self::changed_keys(path, Some(old), Some(new), keys);
        }
        keys.extend(named);
    }

    fn named_facts(&self) -> BTreeMap<&str, NamedFacts<'_>> {
        let mut names = BTreeMap::<&str, NamedFacts<'_>>::new();
        for ty in &self.types {
            names.entry(&ty.name).or_default().types.push(ty);
        }
        for (name, target) in &self.type_targets {
            names.entry(name).or_default().target = Some(target);
        }
        for import in &self.imports {
            let mut locals = BTreeMap::<&str, Vec<&str>>::new();
            for specifier in &import.names {
                locals
                    .entry(crate::imports::specifier_local_name(specifier))
                    .or_default()
                    .push(specifier);
            }
            if let Some(default) = &import.default_import {
                locals.entry(default).or_default();
            }
            for (name, specifiers) in locals {
                names.entry(name).or_default().imports.push((
                    &import.module_path,
                    specifiers,
                    import.default_import.as_deref() == Some(name),
                    self.import_targets.get(&import.module_path),
                ));
            }
        }
        names
    }
}
aft_store::heap_bytes_of_fields!(ImportFact {
    module_path,
    names,
    default_import,
});
aft_store::heap_bytes_of_fields!(FileFacts {
    language,
    go_package,
    types,
    methods,
    fields,
    returns,
    imports,
    default_export_symbol,
    import_targets,
    type_targets,
});
