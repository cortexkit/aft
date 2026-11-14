//! Content records from AFT's extractors.
//!
//! The data layer stores what parsing one content yields as a
//! [`ContentRecord`]. AFT's extractor still produces the call-graph parse
//! output it always has; this module chooses a file's language and converts
//! that output to and from the stored record without losing a field.

use std::path::Path;

use aft_store::content::{
    Body, CallSite, ContentRecord, FieldFact, Import, MethodFact, OutlineSymbol, PythonBinding,
    RecordBody, RefKind, Reference, ReturnFact, Surface, Symbol, TypeFact,
};

use crate::callgraph::python::CallBinding;
use crate::callgraph_store::join::dispatch::{
    DispatchFacts, FieldHint, MethodHint, ReturnHint, SiteHint, TypeHint,
};
use crate::callgraph_store::join::{
    self, BlobImport, BlobRef, BlobRefKind, BlobSymbol, CallgraphBlob, ConfigBlob,
    ManifestJoinError, ParseBlob,
};
use crate::inspect::scanners::todos::{self, TodoLanguage};
use crate::parser::LangId;

/// The version of the extractor whose output the records hold. Records
/// carry it, so output from another version is extracted again. Stored
/// resolutions are keyed by their records, so a new version resolves every
/// file again too, which is also how a change to what resolutions record
/// reaches stored ones. It is the call-graph producer's version with the
/// data layer's own suffix, which counts the changes since that producer
/// version: 1 added the file's outline; 2 added the names inside Rust
/// macros that could not be parsed; 3 added the method-call sites of `self`
/// calls between the functions a `macro_rules!` template defines; 4 dropped
/// the dynamic sites of JavaScript and TypeScript computed member calls
/// whose member is a string literal; 5 added C++ member functions and the
/// sites of C++ member calls whose receiver's type is declared; 6 added each
/// symbol's entry facts (where a program starts, its entry attribute),
/// which `trace_to` and `impact` label roots with; 7 marked Go `func init()`
/// as a program start, made a Rust resolution record each workspace crate
/// name it looked up rather than only the whole crate map, and made a
/// resolution record each config file it found by its path rather than one
/// key for every config file; 8 stopped parsing minified bundles, and made
/// a JavaScript workspace package lookup record the looked-up name's key
/// rather than every member's manifest; 9 stored each file's call-graph
/// slice as a record with its fragment, so publications written before it
/// are built again with those records; 10 added the facts dead-code
/// analysis reads from the syntax tree, so an analysis never parses a file
/// again; 11 stopped recording a method-call site for a Python call through
/// a dotted module import (`import pkg.mod` then `pkg.mod.f()`), whose
/// unresolved site replaced the call's resolved module binding; 12 added
/// each file's TODO markers, and records holding only the markers of the
/// files whose comments are counted but which the call graph does not parse
/// (see [`MARKERS_PREFIX`]), so counting TODOs never parses a file.
pub const EXTRACTOR_VERSION: &str = "ruled-callgraph-v14-resolver-precision+record-13";

/// The start of the language of a file whose record holds only its TODO
/// markers: a Markdown file, a minified bundle, or a C header no grammar
/// resolves. The rest names the language its comments are read as (see
/// [`TodoLanguage::name`]), such as `markers-markdown`.
pub const MARKERS_PREFIX: &str = "markers-";

/// Whether `language` (as [`language_for`] chose it) is one the call graph
/// parses: not a configuration file read raw, nor a file whose record holds
/// only its TODO markers.
#[must_use]
pub fn is_parsed_language(language: &str) -> bool {
    !language.is_empty() && language != "config" && !language.starts_with(MARKERS_PREFIX)
}

/// The scan policy every data-layer scan uses: a file a plane parses or
/// embeds is read up to the larger read limit even past the text-index
/// limit, as the inspect scanners read graph sources and today's semantic
/// index reads what it embeds, so a large source file still has its calls
/// in the graph and its chunks in meaning search.
#[must_use]
pub fn scan_policy() -> aft_store::scan::ScanPolicy {
    aft_store::scan::ScanPolicy {
        read_large: Some(parsed_or_embedded_path),
        ..aft_store::scan::ScanPolicy::default()
    }
}

/// Whether a plane reads the file at `path` (relative to the root) whole:
/// [`language_for`] can choose a language for it (a resolver configuration
/// file or a source file), or today's semantic index embeds files with its
/// name or extension.
fn parsed_or_embedded_path(path: &[u8]) -> bool {
    if crate::callgraph_extractor::is_resolution_input(path) {
        return true;
    }
    let Ok(path) = std::str::from_utf8(path) else {
        return false;
    };
    let path = Path::new(path);
    path.extension().is_some_and(|extension| extension == "h")
        || crate::callgraph_extractor::source_language(path).is_some()
        || crate::semantic_index::is_semantic_indexed_extension(path)
}

/// The language the call graph parses the file at `path` as, `"config"`
/// for resolver configuration it reads raw, a [`MARKERS_PREFIX`] language
/// for a file whose record holds only its TODO markers (a Markdown file, a
/// minified bundle, a C header no grammar resolves), or `None` for a file
/// with neither.
#[must_use]
pub fn language_for(path: &str, source: &[u8]) -> Option<String> {
    if crate::callgraph_extractor::is_resolution_input(path.as_bytes()) {
        return Some("config".to_string());
    }
    let path_text = path;
    let path = Path::new(path);
    if crate::file_kind::is_minified(path_text, source) {
        return markers_language(path);
    }
    let language = if path.extension().is_some_and(|extension| extension == "h") {
        // A header's language comes from parsing these bytes: C, then C++,
        // then Objective-C. A header that is not text parses as nothing;
        // one that no grammar resolves has its comments read lexically.
        let text = std::str::from_utf8(source).ok()?;
        match crate::parser::detect_language_from_source(path, text) {
            Some(language) => language,
            None => return Some(format!("{MARKERS_PREFIX}{}", TodoLanguage::Lexical.name())),
        }
    } else {
        match crate::callgraph_extractor::source_language(path) {
            Some(language) => language,
            None => return markers_language(path),
        }
    };
    Some(format!("{language:?}").to_lowercase())
}

/// The markers-only language of the file at `path`, when its TODOs are
/// counted.
fn markers_language(path: &Path) -> Option<String> {
    let language = todos::path_todo_language(path)?;
    Some(format!("{MARKERS_PREFIX}{}", language.name()))
}

/// The record holding only the TODO markers of `source`, whose comments are
/// read as `name` (a [`MARKERS_PREFIX`] language without its prefix).
fn markers_record(source: &[u8], language: &str) -> Result<ContentRecord, ManifestJoinError> {
    let name = language.strip_prefix(MARKERS_PREFIX).unwrap_or(language);
    let todos = match TodoLanguage::from_name(name) {
        Some(TodoLanguage::Lexical) => todos::markers_lexical(source),
        Some(TodoLanguage::Tree(lang)) => {
            let tree = parse(source, lang)?;
            todos::markers_from_tree(source, lang, &tree)
        }
        None => return Err(ManifestJoinError::UnsupportedLanguage(language.to_string())),
    };
    Ok(ContentRecord {
        language: language.to_string(),
        extractor_version: EXTRACTOR_VERSION.to_string(),
        data: RecordBody::Markers,
        todos: Some(todos),
    })
}

/// Adds to `record`, when it is the configuration record of the file at
/// `path` (relative to the root) holding `source`, the TODO markers of the
/// file's comments, read as the language its name calls for (TOML for
/// `Cargo.toml`, YAML for `pnpm-workspace.yaml`). A configuration record's
/// key does not name that language, so the markers say which they were
/// read as.
///
/// # Errors
///
/// When the source cannot be parsed.
pub fn add_config_markers(
    record: &mut ContentRecord,
    path: &str,
    source: &[u8],
) -> Result<(), ManifestJoinError> {
    if !matches!(record.data, RecordBody::Config { .. }) {
        return Ok(());
    }
    if let Some(TodoLanguage::Tree(lang)) = todos::path_todo_language(Path::new(path)) {
        let tree = parse(source, lang)?;
        record.todos = Some(todos::markers_from_tree(source, lang, &tree));
    }
    Ok(())
}

/// Extracts the record of `source` in `language` (as [`language_for`]
/// chose it).
///
/// # Errors
///
/// When the extractor fails on the source.
/// A missing dynamic grammar is a typed `GrammarUnavailable` error, not a
/// successful empty record. Discovery and persisted record identities remain
/// unchanged until dynamic-language indexing is enabled.
pub fn extract(source: &[u8], language: &str) -> Result<ContentRecord, ManifestJoinError> {
    extract_with_tree(source, language).map(|(record, _)| record)
}

/// The syntax tree a content parsed to, kept so the next version of the
/// same file can be parsed incrementally by [`extract_incremental`].
#[derive(Clone)]
pub struct SyntaxTree {
    tree: tree_sitter::Tree,
    language: LangId,
}

/// The text the extractor parses for `source`. A source that is not valid
/// UTF-8 cannot be parsed; like checkout assembly today, it gets the call
/// graph of an empty file.
fn parsed_text(source: &[u8]) -> &str {
    std::str::from_utf8(source).unwrap_or("")
}

/// Parses `source` as `lang` for extraction. Every parse of this module
/// goes through here.
fn parse(source: &[u8], lang: LangId) -> Result<tree_sitter::Tree, ManifestJoinError> {
    #[cfg(test)]
    note_parsed_for_test(source);
    join::single_walk::parse_source(parsed_text(source), lang)
}

/// The contents extraction has parsed, for tests that check what a reader
/// leaves unparsed. Every parse in this module goes through [`parse`], or
/// through [`reparse`], which notes its parses too.
#[cfg(test)]
static PARSED_FOR_TEST: std::sync::Mutex<Vec<aft_store::ids::ContentKey>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn note_parsed_for_test(source: &[u8]) {
    PARSED_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(aft_store::ids::ContentKey::of(source));
}

/// How many times extraction has parsed `source`'s content so far.
#[cfg(test)]
pub(crate) fn parses_of_for_test(source: &[u8]) -> usize {
    let key = aft_store::ids::ContentKey::of(source);
    PARSED_FOR_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .filter(|parsed| **parsed == key)
        .count()
}

/// Extracts the record of `source` in `language`, as [`extract`] does, and
/// returns the syntax tree it parsed, or `None` for a configuration input,
/// which is stored raw and never parsed.
///
/// # Errors
///
/// When the extractor fails on the source.
pub fn extract_with_tree(
    source: &[u8],
    language: &str,
) -> Result<(ContentRecord, Option<SyntaxTree>), ManifestJoinError> {
    if language == "config" {
        let blob = CallgraphBlob::config(source.to_vec(), EXTRACTOR_VERSION);
        return Ok((record_from_blob(&blob), None));
    }
    if language.starts_with(MARKERS_PREFIX) {
        return Ok((markers_record(source, language)?, None));
    }
    let lang = join::language_id(language)
        .ok_or_else(|| ManifestJoinError::UnsupportedLanguage(language.to_string()))?;
    let tree = SyntaxTree {
        tree: parse(source, lang)?,
        language: lang,
    };
    let record = extract_parsed(source, language, &tree)?;
    Ok((record, Some(tree)))
}

/// Parses `source` again after an edit, reusing the tree of the version it
/// was edited from: `previous` is the tree [`extract_with_tree`] or this
/// function returned for `previous_source`. The edit is taken to be the one
/// span between the texts' common prefix and common suffix, so tree-sitter
/// reparses only what that span touches. The tree equals a fresh parse of
/// `source`. Returns `None` for a configuration input.
///
/// # Errors
///
/// When the language is not one the call graph parses, or parsing fails.
pub fn reparse(
    previous_source: &[u8],
    previous: &SyntaxTree,
    source: &[u8],
    language: &str,
) -> Result<Option<SyntaxTree>, ManifestJoinError> {
    if language == "config" || language.starts_with(MARKERS_PREFIX) {
        return Ok(None);
    }
    let lang = join::language_id(language)
        .ok_or_else(|| ManifestJoinError::UnsupportedLanguage(language.to_string()))?;
    #[cfg(test)]
    note_parsed_for_test(source);
    let text = parsed_text(source);
    if previous.language != lang {
        // A header can parse as another language after an edit; its old
        // tree is of no use to the new grammar.
        return Ok(Some(SyntaxTree {
            tree: join::single_walk::parse_source(text, lang)?,
            language: lang,
        }));
    }
    let tree = join::single_walk::reparse_source(
        parsed_text(previous_source),
        &previous.tree,
        text,
        lang,
    )?;
    Ok(Some(SyntaxTree {
        tree,
        language: lang,
    }))
}

/// Extracts the record of `source` in `language` from `tree`, the tree
/// [`extract_with_tree`] or [`reparse`] returned for this same source.
///
/// # Errors
///
/// When the extractor fails on the source, or `tree` was parsed as another
/// language.
pub fn extract_parsed(
    source: &[u8],
    language: &str,
    tree: &SyntaxTree,
) -> Result<ContentRecord, ManifestJoinError> {
    if language == "config" {
        return Ok(record_from_blob(&CallgraphBlob::config(
            source.to_vec(),
            EXTRACTOR_VERSION,
        )));
    }
    if language.starts_with(MARKERS_PREFIX) {
        return markers_record(source, language);
    }
    if join::language_id(language) != Some(tree.language) {
        return Err(ManifestJoinError::Parse(format!(
            "a {:?} syntax tree cannot be extracted as {language}",
            tree.language
        )));
    }
    let (blob, outline) = CallgraphBlob::extract_tree_with_outline(
        parsed_text(source),
        language,
        EXTRACTOR_VERSION,
        &tree.tree,
    )?;
    let mut record = record_from_blob(&blob);
    // The comments' markers come from the same tree, so the count never
    // parses the file again.
    record.todos = Some(todos::markers_from_tree(source, tree.language, &tree.tree));
    if let RecordBody::Parsed { body, .. } = &mut record.data {
        body.outline = Some(outline.iter().map(outline_symbol).collect());
        body.dead_code = Some(
            crate::inspect::scanners::dead_code::StoredFileFacts::encode_for_record(
                source,
                tree.language,
                &tree.tree,
            ),
        );
    }
    Ok(record)
}

/// Extracts the record of `source` after an edit from `previous_source`,
/// reparsing incrementally from `previous`, the tree that version parsed
/// to. The record equals [`extract`]'s for `source`; the returned tree
/// serves the next edit.
///
/// # Errors
///
/// When the extractor fails on the source.
pub fn extract_incremental(
    previous_source: &[u8],
    previous: &SyntaxTree,
    source: &[u8],
    language: &str,
) -> Result<(ContentRecord, Option<SyntaxTree>), ManifestJoinError> {
    let Some(tree) = reparse(previous_source, previous, source, language)? else {
        return extract_with_tree(source, language);
    };
    let record = extract_parsed(source, language, &tree)?;
    Ok((record, Some(tree)))
}

/// The stored record of a parse output. Lossless apart from the extractor's
/// temporary syntax-tree index, which no reader uses after extraction. Parse
/// output carries no outline, so the record holds none; [`extract_parsed`]
/// adds it.
#[must_use]
pub fn record_from_blob(blob: &CallgraphBlob) -> ContentRecord {
    match blob {
        CallgraphBlob::Config(config) => ContentRecord {
            language: config.language.clone(),
            extractor_version: config.extractor_version.clone(),
            data: RecordBody::Config {
                source: config.source.clone(),
            },
            todos: None,
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
                    outline: None,
                    dead_code: None,
                }),
            },
            todos: None,
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
        RecordBody::Markers => {
            return Err(ManifestJoinError::InvalidBlob(format!(
                "a {} record holds no parse",
                record.language
            )))
        }
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
        program_start: symbol.program_start,
        entry_attribute: symbol.entry_attribute.clone(),
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
        program_start: symbol.program_start,
        entry_attribute: symbol.entry_attribute.clone(),
    }
}

/// The stored form of one symbol of the parser's table.
#[must_use]
pub fn outline_symbol(symbol: &crate::symbols::Symbol) -> OutlineSymbol {
    OutlineSymbol {
        name: symbol.name.clone(),
        kind: outline_kind_name(&symbol.kind).to_string(),
        start_line: symbol.range.start_line,
        start_col: symbol.range.start_col,
        end_line: symbol.range.end_line,
        end_col: symbol.range.end_col,
        signature: symbol.signature.clone(),
        scope_chain: symbol.scope_chain.clone(),
        exported: symbol.exported,
        parent: symbol.parent.clone(),
    }
}

/// The parser's symbol a stored outline symbol holds, or `None` when its
/// kind is not one this build knows.
#[must_use]
pub fn symbol_from_outline(symbol: &OutlineSymbol) -> Option<crate::symbols::Symbol> {
    Some(crate::symbols::Symbol {
        name: symbol.name.clone(),
        kind: outline_kind(&symbol.kind)?,
        range: crate::symbols::Range {
            start_line: symbol.start_line,
            start_col: symbol.start_col,
            end_line: symbol.end_line,
            end_col: symbol.end_col,
        },
        signature: symbol.signature.clone(),
        scope_chain: symbol.scope_chain.clone(),
        exported: symbol.exported,
        parent: symbol.parent.clone(),
    })
}

/// The name a record stores a parser symbol kind under: its serialized
/// name.
fn outline_kind_name(kind: &crate::symbols::SymbolKind) -> &'static str {
    use crate::symbols::SymbolKind;
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Kernel => "kernel",
        SymbolKind::Class => "class",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Interface => "interface",
        SymbolKind::Enum => "enum",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Variable => "variable",
        SymbolKind::Heading => "heading",
        SymbolKind::FileSummary => "file_summary",
    }
}

fn outline_kind(name: &str) -> Option<crate::symbols::SymbolKind> {
    use crate::symbols::SymbolKind;
    Some(match name {
        "function" => SymbolKind::Function,
        "kernel" => SymbolKind::Kernel,
        "class" => SymbolKind::Class,
        "method" => SymbolKind::Method,
        "struct" => SymbolKind::Struct,
        "interface" => SymbolKind::Interface,
        "enum" => SymbolKind::Enum,
        "type_alias" => SymbolKind::TypeAlias,
        "variable" => SymbolKind::Variable,
        "heading" => SymbolKind::Heading,
        "file_summary" => SymbolKind::FileSummary,
        _ => return None,
    })
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
        BlobRefKind::MacroMention => RefKind::MacroMention,
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
        RefKind::MacroMention => BlobRefKind::MacroMention,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn grammar_unavailable_survives_the_content_record_boundary() {
        let directory = tempfile::tempdir().unwrap();
        let config = crate::config::Config {
            storage_dir: Some(directory.path().into()),
            ..Default::default()
        };
        let registry = crate::grammar::configure(&config);
        let source = b"local function f() return 1 end";
        let classified = super::language_for("f.luau", source);
        assert_ne!(
            classified.as_deref(),
            Some("luau"),
            "S2 must not enable dynamic discovery"
        );
        let error = super::extract_with_tree(source, "luau")
            .err()
            .expect("no record without a grammar");
        let super::ManifestJoinError::GrammarUnavailable(error) = error else {
            panic!("grammar availability was flattened")
        };
        assert_eq!(
            error.state,
            if cfg!(feature = "wasm-grammars") {
                crate::grammar::GrammarState::NotDownloaded
            } else {
                crate::grammar::GrammarState::SupportDisabled
            }
        );
        assert_eq!(error.language, "luau");
        assert_eq!(error.retry_after_ms, None);
        assert_eq!(error.cause, None);
        assert!(registry.stored().is_err());
        assert!(
            !directory.path().join("grammars").exists(),
            "index extraction must not download"
        );
    }
    use super::*;

    /// The record's version names the call-graph producer it extracts with,
    /// so a producer bump cannot leave records of its old output under the
    /// new name; the suffix keeps them apart from views' blobs of the same
    /// producer, which hold no outline.
    #[test]
    fn the_extractor_version_follows_the_call_graph_producer() {
        let producer = crate::callgraph_extractor::PRODUCER;
        assert!(
            EXTRACTOR_VERSION.starts_with(producer) && EXTRACTOR_VERSION != producer,
            "{EXTRACTOR_VERSION} does not extend {producer}"
        );
    }

    /// Every stored kind name is the parser's serialized name, and reads
    /// back as the same kind.
    #[test]
    fn outline_kinds_round_trip_under_their_serialized_names() {
        use crate::symbols::SymbolKind;
        for kind in [
            SymbolKind::Function,
            SymbolKind::Kernel,
            SymbolKind::Class,
            SymbolKind::Method,
            SymbolKind::Struct,
            SymbolKind::Interface,
            SymbolKind::Enum,
            SymbolKind::TypeAlias,
            SymbolKind::Variable,
            SymbolKind::Heading,
            SymbolKind::FileSummary,
        ] {
            let name = outline_kind_name(&kind);
            assert_eq!(serde_json::to_value(&kind).unwrap(), name);
            assert_eq!(outline_kind(name), Some(kind));
        }
        assert_eq!(outline_kind("macro"), None);
    }

    /// Files of many languages: the repository's test fixtures and a sample
    /// of AFT's own sources, among them files with `#[cfg(unix)]` and
    /// `#[cfg(windows)]` twins and Rust impl methods.
    pub(crate) fn sample_files() -> Vec<std::path::PathBuf> {
        let crate_dir = crate::test_support::manifest_dir();
        let fixtures = crate_dir.join("tests/fixtures");
        let mut files = Vec::new();
        for dir in [fixtures.clone(), fixtures.join("callgraph")] {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_file() {
                    files.push(path);
                }
            }
        }
        for source in [
            "../aft-store/src/stat.rs",
            "../aft-store/src/content/mod.rs",
            "src/effective_path.rs",
            "src/format.rs",
            "src/symbols.rs",
            "src/data_layer/runtime.rs",
            "src/data_layer/content.rs",
        ] {
            files.push(crate_dir.join(source));
        }
        files.sort();
        files
    }

    /// How many symbols carry each name.
    fn name_counts<'a>(
        names: impl Iterator<Item = &'a str>,
    ) -> std::collections::HashMap<&'a str, usize> {
        let mut counts = std::collections::HashMap::new();
        for name in names {
            *counts.entry(name).or_default() += 1;
        }
        counts
    }

    /// A record's outline is exactly the table a parse of the file gives
    /// outline, while the record's call-graph symbols are not: the call
    /// graph keeps one of each scoped name, so platform twins collapse.
    #[test]
    fn stored_outlines_equal_the_parsers_symbol_table() {
        let mut compared = 0;
        let mut collapsed = Vec::new();
        for path in sample_files() {
            let bytes = std::fs::read(&path).unwrap();
            let name = path.display().to_string();
            let Some(language) = language_for(&name, &bytes) else {
                continue;
            };
            if !is_parsed_language(&language) {
                continue;
            }
            let record = extract(&bytes, &language).unwrap();
            let RecordBody::Parsed { surface, body } = &record.data else {
                panic!("{name}: a parsed language gives a parsed record");
            };
            let stored = body
                .outline
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: no outline"))
                .iter()
                .map(|symbol| symbol_from_outline(symbol).unwrap())
                .collect::<Vec<_>>();
            let parsed = crate::parser::FileParser::new()
                .extract_symbols(&path)
                .unwrap();
            assert_eq!(stored, parsed, "{name}");
            let (surface_part, body_part) = record.encode();
            assert_eq!(
                ContentRecord::decode(&surface_part, &body_part).unwrap(),
                record,
                "{name}"
            );
            let in_surface = name_counts(surface.symbols.iter().map(|s| s.name.as_str()));
            if name_counts(parsed.iter().map(|s| s.name.as_str()))
                .iter()
                .any(|(symbol, n)| in_surface.get(symbol).copied().unwrap_or(0) < *n)
            {
                collapsed.push(name);
            }
            compared += 1;
        }
        assert!(compared >= 40, "only {compared} files compared");
        assert!(
            collapsed.iter().any(|name| name.ends_with("stat.rs")),
            "the platform twins of stat.rs should outnumber the call graph's symbols: {collapsed:?}"
        );
    }

    #[test]
    fn files_meaning_search_embeds_are_read_past_the_text_index_limit() {
        struct Kinds(std::sync::Mutex<Vec<(Vec<u8>, aft_store::scan::ContentKind)>>);
        impl aft_store::scan::ContentSink for Kinds {
            fn read(
                &self,
                path: &[u8],
                _key: aft_store::ids::ContentKey,
                kind: aft_store::scan::ContentKind,
                _bytes: &[u8],
            ) {
                self.0.lock().unwrap().push((path.to_vec(), kind));
            }
        }
        let dir = tempfile::tempdir().unwrap();
        // YAML is embedded by meaning search, and not parsed by the call
        // graph: only the semantic extension test admits it.
        let line = "key: value with some words in it\n";
        let text_of = |bytes: usize| line.repeat(bytes / line.len() + 1);
        let large = text_of(2 << 20);
        let huge = text_of(5 << 20);
        std::fs::write(dir.path().join("large.yaml"), &large).unwrap();
        std::fs::write(dir.path().join("huge.yaml"), &huge).unwrap();
        // A 2 MiB file with no plane reading it stays unread.
        std::fs::write(dir.path().join("large.txt"), &large).unwrap();
        let sink = Kinds(std::sync::Mutex::new(Vec::new()));
        let scanned = aft_store::scan::scan(dir.path(), &scan_policy(), None, &sink).unwrap();
        let record = |path: &[u8]| scanned.state.get(path).unwrap().clone();
        let read = record(b"large.yaml");
        assert_eq!(read.kind, aft_store::checkout::PathKind::Large);
        assert_eq!(
            read.content,
            aft_store::ids::ContentKey::of(large.as_bytes()),
            "a Large file is keyed by its real bytes"
        );
        assert_eq!(
            record(b"huge.yaml").kind,
            aft_store::checkout::PathKind::Oversize,
            "past the 4 MiB read limit a file is not read"
        );
        assert_eq!(
            record(b"large.txt").kind,
            aft_store::checkout::PathKind::Oversize
        );
        assert_eq!(
            *sink.0.lock().unwrap(),
            vec![(b"large.yaml".to_vec(), aft_store::scan::ContentKind::Large)]
        );
    }
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
        // Markdown is not parsed by the call graph: its record holds only
        // its TODO markers, and converts to no call-graph parse.
        let markdown = language_for("README.md", b"# x").unwrap();
        assert_eq!(markdown, "markers-markdown");
        assert!(!is_parsed_language(&markdown));
        let record = extract(b"# x", &markdown).unwrap();
        assert_eq!(record.data, RecordBody::Markers);
        assert!(blob_from_record(&record).is_err());
    }

    #[test]
    fn incremental_records_equal_full_extractions() {
        let base =
            "use std::fmt;\nfn caller() {\n    helper(1);\n}\nfn helper(x: u8) -> u8 { x }\n";
        // Each version is edited from the one before it.
        let versions = [
            // Insert a call inside a body.
            "use std::fmt;\nfn caller() {\n    helper(1);\n    other();\n}\nfn helper(x: u8) -> u8 { x }\n",
            // Delete a whole item.
            "use std::fmt;\nfn caller() {\n    helper(1);\n    other();\n}\n",
            // Edit at the start, in a multi-byte character.
            "// é\nuse std::fmt;\nfn caller() {\n    helper(1);\n    other();\n}\n",
            // Leave the file unparsable, then repair it.
            "// é\nuse std::fmt;\nfn caller() {\n    helper(1\n    other();\n}\n",
            "// é\nuse std::fmt;\nfn caller() {\n    helper(1);\n    other();\n}\nmod m;\n",
            // The same text again.
            "// é\nuse std::fmt;\nfn caller() {\n    helper(1);\n    other();\n}\nmod m;\n",
            "",
            "fn again() { helper(); }",
        ];
        let (_, tree) = extract_with_tree(base.as_bytes(), "rust").unwrap();
        let mut previous = (base, tree.unwrap());
        for version in versions {
            let (record, tree) = extract_incremental(
                previous.0.as_bytes(),
                &previous.1,
                version.as_bytes(),
                "rust",
            )
            .unwrap();
            assert_eq!(
                record,
                extract(version.as_bytes(), "rust").unwrap(),
                "{version:?}"
            );
            let tree = tree.unwrap();
            let (_, fresh) = extract_with_tree(version.as_bytes(), "rust").unwrap();
            assert_eq!(
                tree.tree.root_node().to_sexp(),
                fresh.unwrap().tree.root_node().to_sexp(),
                "{version:?}"
            );
            previous = (version, tree);
        }
        // A tree of another language is not reused.
        let (_, typescript) = extract_with_tree(b"f();", "typescript").unwrap();
        let (record, _) =
            extract_incremental(b"f();", &typescript.unwrap(), b"f();", "rust").unwrap();
        assert_eq!(record, extract(b"f();", "rust").unwrap());
        // Configuration inputs have no tree.
        let (record, tree) = extract_with_tree(b"[package]\n", "config").unwrap();
        assert!(tree.is_none());
        assert_eq!(record, extract(b"[package]\n", "config").unwrap());
    }

    /// How deep the deeply nested inputs below nest. The Swift compiler's
    /// own test suite holds a file of about 50,000 nested parentheses.
    const NESTING: usize = 20_000;

    /// `open`, then `inner`, then `close`, each of the brackets repeated
    /// [`NESTING`] times.
    fn nested(open: &str, inner: &str, close: &str) -> String {
        format!("{}{inner}{}", open.repeat(NESTING), close.repeat(NESTING))
    }

    /// Extracts `source`, named `path`, on a thread with a 2 MiB stack, and
    /// fails the test if extraction errs. Store scans extract on the file
    /// walker's threads, whose stacks are 2 MiB; a walk that recursed once
    /// per tree level overflowed one on a deeply nested file, and a stack
    /// overflow aborts the whole process rather than failing one file.
    /// Extraction must hold its depth on the heap (or stop descending at a
    /// fixed bound), so a stack this small still copes with any nesting.
    fn extract_on_a_small_stack(path: &'static str, source: String) {
        let language =
            language_for(path, source.as_bytes()).unwrap_or_else(|| panic!("{path}: no language"));
        std::thread::Builder::new()
            .name(format!("deep-{path}"))
            .stack_size(2 << 20)
            .spawn(move || {
                extract(source.as_bytes(), &language)
                    .unwrap_or_else(|error| panic!("{path}: {error}"));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// One test per language, so a stack overflow names the language that
    /// caused it.
    macro_rules! deep_nesting_tests {
        ($($name:ident: $path:literal => $source:expr;)*) => {$(
            #[test]
            fn $name() {
                extract_on_a_small_stack($path, $source);
            }
        )*};
    }

    mod deeply_nested_files_extract_on_a_small_stack {
        use super::{extract_on_a_small_stack, nested};

        deep_nesting_tests! {
            swift: "deep.swift" => format!("import Foundation\nlet x = {}\n", nested("(", "1", ")"));
            kotlin: "deep.kt" => format!("import a.b\nval x = {}\n", nested("(", "1", ")"));
            csharp: "deep.cs" => format!("using System;\nclass C {{ int x = {}; }}\n", nested("(", "1", ")"));
            c_expression: "deep.c" => format!("#include <stdio.h>\nint x = {};\n", nested("(", "1", ")"));
            c_preprocessor: "deep_if.c" => nested("#ifdef A\n", "#include <stdio.h>\n", "#endif\n");
            cpp: "deep.cpp" => format!("#include <vector>\nint x = {};\n", nested("(", "1", ")"));
            cuda: "deep.cu" => format!("#include <cuda.h>\nint x = {};\n", nested("(", "1", ")"));
            metal: "deep.metal" => format!("#include <metal_stdlib>\nint x = {};\n", nested("(", "1", ")"));
            objc: "deep.m" => format!("#import <Foundation/Foundation.h>\nint x = {};\n", nested("(", "1", ")"));
            typescript: "deep.ts" => format!("import {{ a }} from './a';\nconst x = {};\n", nested("(", "1", ")"));
            tsx: "deep.tsx" => format!("import {{ a }} from './a';\nconst x = {};\n", nested("(", "1", ")"));
            javascript: "deep.js" => format!("import {{ a }} from './a';\nconst x = {};\n", nested("(", "1", ")"));
            python: "deep.py" => format!("import os\nx = {}\n", nested("(", "1", ")"));
            rust: "deep.rs" => format!("use std::fmt;\nfn f() {{ let x = {}; }}\n", nested("(", "1", ")"));
            go: "deep.go" => format!("package p\nimport \"fmt\"\nvar x = {}\n", nested("(", "1", ")"));
            zig: "deep.zig" => format!("const std = @import(\"std\");\nconst x = {};\n", nested("(", "1", ")"));
            bash: "deep.sh" => format!("source ./a.sh\n{}\n", nested("( ", "true", " )"));
            html: "deep.html" => nested("<div>", "<h1>x</h1>", "</div>");
            html_heading: "deep_heading.html" => format!("<h1>{}</h1>\n", nested("<b>", "x", "</b>"));
            // No Markdown case: the Markdown grammar's external scanner
            // aborts on deeply nested block quotes while parsing, because
            // its saved state outgrows tree-sitter's fixed buffer. That is a
            // parser limit, separate from the walks this test covers.
            solidity: "deep.sol" => format!("import \"./a.sol\";\ncontract C {{ function f() public {{ uint x = {}; }} }}\n", nested("(", "1", ")"));
            scss: "deep.scss" => format!("@use 'a';\n{}\n", nested("a { ", "color: red;", " }"));
            vue: "deep.vue" => format!("<template>{}</template>\n<script>\nimport a from './a';\nconst x = {};\n</script>\n", nested("<div>", "x", "</div>"), nested("(", "1", ")"));
            json: "deep.json" => nested("[", "1", "]");
            scala: "deep.scala" => format!("import a.b\nobject O {{ val x = {} }}\n", nested("(", "1", ")"));
            java: "deep.java" => format!("import a.b;\nclass C {{ int x = {}; }}\n", nested("(", "1", ")"));
            ruby: "deep.rb" => format!("require 'a'\nx = {}\n", nested("(", "1", ")"));
            php_expression: "deep.php" => format!("<?php\nuse A\\B;\n$x = {};\n", nested("(", "1", ")"));
            php_namespaces: "deep_ns.php" => format!("<?php\n{}\n", nested("namespace A { ", "use A\\B;", " }"));
            lua: "deep.lua" => format!("local a = require('a')\nlocal x = {}\n", nested("(", "1", ")"));
            perl: "deep.pl" => format!("use strict;\nmy $x = {};\n", nested("(", "1", ")"));
            yaml: "deep.yaml" => format!("x: {}\n", nested("[", "1", "]"));
            pascal: "deep.pas" => format!("program P;\nuses SysUtils;\nbegin\n  x := {};\nend.\n", nested("(", "1", ")"));
            r: "deep.R" => format!("library(a)\nx <- {}\n", nested("(", "1", ")"));
            groovy: "deep.groovy" => format!("import a.b\ndef x = {}\n", nested("(", "1", ")"));
            toml: "deep.toml" => format!("x = {}\n", nested("[", "1", "]"));
        }
    }
}
