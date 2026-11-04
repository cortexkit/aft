//! Content records from AFT's extractors.
//!
//! The data layer stores what parsing one content yields as a
//! [`ContentRecord`]. AFT's extractor still produces the call-graph parse
//! output it always has; this module chooses a file's language and converts
//! that output to and from the stored record without losing a field.

use std::path::Path;

use aft_store::content::{
    Body, CallSite, ContentRecord, FieldFact, Import, MethodFact, PythonBinding, RecordBody,
    RefKind, Reference, ReturnFact, Surface, Symbol, TypeFact,
};

use crate::callgraph::python::CallBinding;
use crate::callgraph_store::join::dispatch::{
    DispatchFacts, FieldHint, MethodHint, ReturnHint, SiteHint, TypeHint,
};
use crate::callgraph_store::join::{
    BlobImport, BlobRef, BlobRefKind, BlobSymbol, CallgraphBlob, ConfigBlob, ManifestJoinError,
    ParseBlob,
};

/// The version of the extractor whose output the records hold. Records
/// carry it, so output from another version is extracted again.
pub const EXTRACTOR_VERSION: &str = crate::views::callgraph::PRODUCER;

/// The language the call graph parses the file at `path` as, `"config"`
/// for resolver configuration it reads raw, or `None` for a file the call
/// graph does not read.
#[must_use]
pub fn language_for(path: &str, source: &[u8]) -> Option<String> {
    if crate::views::assembly::is_resolution_input(path.as_bytes()) {
        return Some("config".to_string());
    }
    let path = Path::new(path);
    let language = if path.extension().is_some_and(|extension| extension == "h") {
        // A header's language comes from parsing these bytes: C, then C++,
        // then Objective-C. A header that is not text parses as nothing.
        crate::parser::detect_language_from_source(path, std::str::from_utf8(source).ok()?)?
    } else {
        crate::views::callgraph::source_language(path)?
    };
    Some(format!("{language:?}").to_lowercase())
}

/// Extracts the record of `source` in `language` (as [`language_for`]
/// chose it).
///
/// # Errors
///
/// When the extractor fails on the source.
pub fn extract(source: &[u8], language: &str) -> Result<ContentRecord, ManifestJoinError> {
    let blob = if language == "config" {
        CallgraphBlob::config(source.to_vec(), EXTRACTOR_VERSION)
    } else {
        // A source that is not valid UTF-8 cannot be parsed; like checkout
        // assembly today, it gets the call graph of an empty file.
        CallgraphBlob::extract(
            std::str::from_utf8(source).unwrap_or(""),
            language,
            EXTRACTOR_VERSION,
        )?
    };
    Ok(record_from_blob(&blob))
}

/// The stored record of a parse output. Lossless apart from the extractor's
/// temporary syntax-tree index, which no reader uses after extraction.
#[must_use]
pub fn record_from_blob(blob: &CallgraphBlob) -> ContentRecord {
    match blob {
        CallgraphBlob::Config(config) => ContentRecord {
            language: config.language.clone(),
            extractor_version: config.extractor_version.clone(),
            data: RecordBody::Config {
                source: config.source.clone(),
            },
        },
        CallgraphBlob::Parse(parse) => ContentRecord {
            language: parse.language.clone(),
            extractor_version: parse.extractor_version.clone(),
            data: RecordBody::Parsed {
                surface: Box::new(Surface {
                    symbols: parse.symbols.iter().map(symbol).collect(),
                    default_export_symbol: parse.default_export_symbol.clone(),
                    exported_symbols: parse.exported_symbols.clone(),
                    callable_symbols: parse.callable_symbols.clone(),
                    entry_point_symbols: parse.entry_point_symbols.clone(),
                    go_package: parse.dispatch.go_package.clone(),
                    types: parse
                        .dispatch
                        .types
                        .iter()
                        .map(|fact| TypeFact {
                            name: fact.name.clone(),
                            bases: fact.bases.clone(),
                            interface: fact.interface,
                            closed: fact.closed,
                        })
                        .collect(),
                    methods: parse
                        .dispatch
                        .methods
                        .iter()
                        .map(|fact| MethodFact {
                            owner: fact.owner.clone(),
                            trait_name: fact.trait_name.clone(),
                            name: fact.name.clone(),
                            symbol: fact.symbol.clone(),
                            has_body: fact.has_body,
                            shape: fact.shape.clone(),
                            private: fact.private,
                        })
                        .collect(),
                    fields: parse
                        .dispatch
                        .fields
                        .iter()
                        .map(|fact| FieldFact {
                            owner: fact.owner.clone(),
                            name: fact.name.clone(),
                            ty: fact.ty.clone(),
                        })
                        .collect(),
                    returns: parse
                        .dispatch
                        .returns
                        .iter()
                        .map(|fact| ReturnFact {
                            symbol: fact.symbol.clone(),
                            ty: fact.ty.clone(),
                        })
                        .collect(),
                }),
                body: Box::new(Body {
                    imports: parse.imports.iter().map(import).collect(),
                    python_bindings: parse
                        .python_bindings
                        .iter()
                        .map(|binding| PythonBinding {
                            byte_start: binding.byte_start as u64,
                            byte_end: binding.byte_end as u64,
                            target: binding.target.clone(),
                        })
                        .collect(),
                    references: parse.refs.iter().map(reference).collect(),
                    call_sites: parse
                        .dispatch
                        .sites
                        .iter()
                        .map(|site| CallSite {
                            ordinal: site.ordinal,
                            caller: site.caller.clone(),
                            line: site.line,
                            member: site.member.clone(),
                            receiver: site.receiver.clone(),
                            dynamic: site.dynamic,
                        })
                        .collect(),
                }),
            },
        },
    }
}

/// The parse output a stored record holds.
///
/// # Errors
///
/// When a stored byte offset does not fit this platform's `usize`.
pub fn blob_from_record(record: &ContentRecord) -> Result<CallgraphBlob, ManifestJoinError> {
    let offset = |value: u64| {
        usize::try_from(value)
            .map_err(|_| ManifestJoinError::InvalidBlob(format!("offset {value} exceeds usize")))
    };
    Ok(match &record.data {
        RecordBody::Config { source } => CallgraphBlob::Config(ConfigBlob {
            extractor_version: record.extractor_version.clone(),
            language: record.language.clone(),
            source: source.clone(),
        }),
        RecordBody::Parsed { surface, body } => CallgraphBlob::Parse(ParseBlob {
            entry_point_symbols: surface.entry_point_symbols.clone(),
            dispatch: DispatchFacts {
                go_package: surface.go_package.clone(),
                types: surface
                    .types
                    .iter()
                    .map(|fact| TypeHint {
                        name: fact.name.clone(),
                        bases: fact.bases.clone(),
                        interface: fact.interface,
                        closed: fact.closed,
                    })
                    .collect(),
                methods: surface
                    .methods
                    .iter()
                    .map(|fact| MethodHint {
                        owner: fact.owner.clone(),
                        trait_name: fact.trait_name.clone(),
                        name: fact.name.clone(),
                        symbol: fact.symbol.clone(),
                        has_body: fact.has_body,
                        shape: fact.shape.clone(),
                        private: fact.private,
                    })
                    .collect(),
                sites: body
                    .call_sites
                    .iter()
                    .map(|site| SiteHint {
                        ordinal: site.ordinal,
                        caller: site.caller.clone(),
                        line: site.line,
                        member: site.member.clone(),
                        receiver: site.receiver.clone(),
                        dynamic: site.dynamic,
                    })
                    .collect(),
                fields: surface
                    .fields
                    .iter()
                    .map(|fact| FieldHint {
                        owner: fact.owner.clone(),
                        name: fact.name.clone(),
                        ty: fact.ty.clone(),
                    })
                    .collect(),
                returns: surface
                    .returns
                    .iter()
                    .map(|fact| ReturnHint {
                        symbol: fact.symbol.clone(),
                        ty: fact.ty.clone(),
                    })
                    .collect(),
            },
            extractor_version: record.extractor_version.clone(),
            language: record.language.clone(),
            ast_nodes: Vec::new(),
            symbols: surface.symbols.iter().map(blob_symbol).collect(),
            default_export_symbol: surface.default_export_symbol.clone(),
            exported_symbols: surface.exported_symbols.clone(),
            callable_symbols: surface.callable_symbols.clone(),
            imports: body
                .imports
                .iter()
                .map(|import| {
                    Ok(BlobImport {
                        ordinal: import.ordinal,
                        module_path: import.module_path.clone(),
                        names: import.names.clone(),
                        default_import: import.default_import.clone(),
                        namespace_import: import.namespace_import.clone(),
                        byte_start: offset(import.byte_start)?,
                        byte_end: offset(import.byte_end)?,
                        raw_text: import.raw_text.clone(),
                        type_only: import.type_only,
                        side_effect: import.side_effect,
                    })
                })
                .collect::<Result<_, ManifestJoinError>>()?,
            python_bindings: body
                .python_bindings
                .iter()
                .map(|binding| {
                    Ok(CallBinding {
                        byte_start: offset(binding.byte_start)?,
                        byte_end: offset(binding.byte_end)?,
                        target: binding.target.clone(),
                    })
                })
                .collect::<Result<_, ManifestJoinError>>()?,
            refs: body
                .references
                .iter()
                .map(|reference| {
                    Ok(BlobRef {
                        ordinal: reference.ordinal,
                        kind: blob_ref_kind(reference.kind),
                        caller_symbol: reference.caller_symbol.clone(),
                        short_name: reference.short_name.clone(),
                        full_ref: reference.full_ref.clone(),
                        module_path: reference.module_path.clone(),
                        line: reference.line,
                        byte_start: offset(reference.byte_start)?,
                        byte_end: offset(reference.byte_end)?,
                        path_override: reference.path_override.clone(),
                        local_name: reference.local_name.clone(),
                        requested_name: reference.requested_name.clone(),
                        namespace_alias: reference.namespace_alias.clone(),
                        wildcard: reference.wildcard,
                        import_kind: reference.import_kind.clone(),
                    })
                })
                .collect::<Result<_, ManifestJoinError>>()?,
        }),
    })
}

fn symbol(symbol: &BlobSymbol) -> Symbol {
    Symbol {
        is_test: symbol.is_test,
        ordinal: symbol.ordinal,
        name: symbol.name.clone(),
        scoped_name: symbol.scoped_name.clone(),
        kind: symbol.kind.clone(),
        exported: symbol.exported,
        is_default_export: symbol.is_default_export,
        start_line: symbol.start_line,
        start_col: symbol.start_col,
        end_line: symbol.end_line,
        end_col: symbol.end_col,
        signature: symbol.signature.clone(),
    }
}

fn blob_symbol(symbol: &Symbol) -> BlobSymbol {
    BlobSymbol {
        is_test: symbol.is_test,
        ordinal: symbol.ordinal,
        name: symbol.name.clone(),
        scoped_name: symbol.scoped_name.clone(),
        kind: symbol.kind.clone(),
        exported: symbol.exported,
        is_default_export: symbol.is_default_export,
        start_line: symbol.start_line,
        start_col: symbol.start_col,
        end_line: symbol.end_line,
        end_col: symbol.end_col,
        signature: symbol.signature.clone(),
    }
}

fn import(import: &BlobImport) -> Import {
    Import {
        ordinal: import.ordinal,
        module_path: import.module_path.clone(),
        names: import.names.clone(),
        default_import: import.default_import.clone(),
        namespace_import: import.namespace_import.clone(),
        byte_start: import.byte_start as u64,
        byte_end: import.byte_end as u64,
        raw_text: import.raw_text.clone(),
        type_only: import.type_only,
        side_effect: import.side_effect,
    }
}

fn reference(reference: &BlobRef) -> Reference {
    Reference {
        ordinal: reference.ordinal,
        kind: ref_kind(reference.kind),
        caller_symbol: reference.caller_symbol.clone(),
        short_name: reference.short_name.clone(),
        full_ref: reference.full_ref.clone(),
        module_path: reference.module_path.clone(),
        line: reference.line,
        byte_start: reference.byte_start as u64,
        byte_end: reference.byte_end as u64,
        path_override: reference.path_override.clone(),
        local_name: reference.local_name.clone(),
        requested_name: reference.requested_name.clone(),
        namespace_alias: reference.namespace_alias.clone(),
        wildcard: reference.wildcard,
        import_kind: reference.import_kind.clone(),
    }
}

fn ref_kind(kind: BlobRefKind) -> RefKind {
    match kind {
        BlobRefKind::Construction => RefKind::Construction,
        BlobRefKind::CallbackRef => RefKind::CallbackRef,
        BlobRefKind::Call => RefKind::Call,
        BlobRefKind::ValueRef => RefKind::ValueRef,
        BlobRefKind::Import => RefKind::Import,
        BlobRefKind::Module => RefKind::Module,
        BlobRefKind::Reexport => RefKind::Reexport,
        BlobRefKind::ExportAlias => RefKind::ExportAlias,
    }
}

fn blob_ref_kind(kind: RefKind) -> BlobRefKind {
    match kind {
        RefKind::Construction => BlobRefKind::Construction,
        RefKind::CallbackRef => BlobRefKind::CallbackRef,
        RefKind::Call => BlobRefKind::Call,
        RefKind::ValueRef => BlobRefKind::ValueRef,
        RefKind::Import => BlobRefKind::Import,
        RefKind::Module => BlobRefKind::Module,
        RefKind::Reexport => BlobRefKind::Reexport,
        RefKind::ExportAlias => BlobRefKind::ExportAlias,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_header_language_comes_from_its_own_bytes() {
        // Bytes no other test parses, so no earlier parse can have chosen
        // their language.
        let cpp = b"// a_header_language_comes_from_its_own_bytes\nnamespace demo { class Widget { public: int size() const; }; }\n";
        let c = b"// a_header_language_comes_from_its_own_bytes\nstruct point { int x; int y; };\nint area(struct point p);\n";
        assert_eq!(
            language_for("include/widget.h", cpp).as_deref(),
            Some("cpp")
        );
        assert_eq!(language_for("include/point.h", c).as_deref(), Some("c"));
        assert_eq!(language_for("include/binary.h", &[0xff, 0xfe, 0x00]), None);
    }

    #[test]
    fn records_round_trip_every_field_of_real_parses() {
        let sources = [
            (
                "src/lib.rs",
                "use std::path::PathBuf;\npub struct Store { path: PathBuf }\nimpl Store {\n    pub fn open() -> Self { Store { path: PathBuf::new() } }\n    fn join(&self) { self.path.join(\"x\"); }\n}\n#[test]\nfn works() { Store::open(); }\n",
            ),
            (
                "web/app.ts",
                "import { load, type Config } from './config';\nexport default class App { run(c: Config) { return load(c).start(); } }\nexport const make = () => new App();\n",
            ),
            (
                "tool/main.py",
                "from pkg.mod import helper\nimport os\nclass Tool(Base):\n    def run(self):\n        return helper(os.getcwd())\n",
            ),
            (
                "pkg/go.go",
                "package pkg\nimport \"fmt\"\ntype T struct{}\nfunc (t T) Show() { fmt.Println(t) }\n",
            ),
            ("Cargo.toml", "[package]\nname = \"x\"\n"),
        ];
        for (path, source) in sources {
            let language = language_for(path, source.as_bytes()).unwrap();
            let record = extract(source.as_bytes(), &language).unwrap();
            let (surface, body) = record.encode();
            let decoded = ContentRecord::decode(&surface, &body).unwrap();
            assert_eq!(decoded, record, "{path}");
            let mut original = if language == "config" {
                CallgraphBlob::config(source.as_bytes().to_vec(), EXTRACTOR_VERSION)
            } else {
                CallgraphBlob::extract(source, &language, EXTRACTOR_VERSION).unwrap()
            };
            if let CallgraphBlob::Parse(parse) = &mut original {
                parse.ast_nodes.clear();
            }
            assert_eq!(blob_from_record(&decoded).unwrap(), original, "{path}");
        }
        assert_eq!(language_for("README.md", b"# x"), None);
    }
}
