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

    /// Like [`Self::changed_keys`], but when only some of a non-Go file's
    /// methods change, names only the method names whose methods changed.
    ///
    /// That is enough because of what a resolution that found this file
    /// through a method name reads without recording this file's key: the
    /// methods of that name, and the file's other facts (language, types,
    /// Go package). When any of those other facts change, every method name
    /// is named, as before. A Go resolution also reads the file's other
    /// methods (an interface's required set), so a Go file names every
    /// method name.
    pub(crate) fn changed_keys_by_method(
        path: &str,
        old: Option<&Self>,
        new: Option<&Self>,
        keys: &mut BTreeSet<String>,
    ) {
        let (Some(old), Some(new)) = (old, new) else {
            return Self::changed_keys(path, old, new, keys);
        };
        let without_methods = |facts: &Self| Self {
            methods: Vec::new(),
            ..facts.clone()
        };
        if old.language == "go"
            || new.language == "go"
            || without_methods(old) != without_methods(new)
        {
            return Self::changed_keys(path, Some(old), Some(new), keys);
        }
        if old.methods == new.methods {
            return;
        }
        keys.insert(dependency::file(path));
        keys.insert(dependency::directory(path));
        let by_name = |facts: &'_ Self| {
            let mut names = BTreeMap::<String, Vec<MethodHint>>::new();
            for method in &facts.methods {
                names
                    .entry(method.name.clone())
                    .or_default()
                    .push(method.clone());
            }
            names
        };
        let (old, new) = (by_name(old), by_name(new));
        for name in old.keys().chain(new.keys()) {
            if old.get(name) != new.get(name) {
                keys.insert(dependency::method(name));
            }
        }
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
