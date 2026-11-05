//! Source-independent method linking for manifest views. Extraction records only
//! syntactic receiver evidence; joining never reparses or opens checkout files.
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tree_sitter::Node;

use super::super::rust_bindings::{Initializer, RustBindings};
use super::syntax_index::{Direct, Nav};
use super::{BlobRef, BlobRefKind, BlobSymbol, ManifestJoinError, ParseBlob};
use crate::parser::LangId;

/// Named imports still bound at Python module scope, already checked during
/// extraction for assignments, conflicting aliases and local-only imports.
pub(crate) fn python_reexports(parse: &ParseBlob) -> Vec<(String, Vec<String>)> {
    parse
        .refs
        .iter()
        .filter(|reference| reference.kind == BlobRefKind::Reexport && !reference.wildcard)
        .filter_map(|reference| {
            let module = reference.module_path.clone()?;
            let mut names =
                crate::callgraph_store::parse_reexport_names(reference.full_ref.as_deref()?)
                    .into_iter()
                    .map(|(local, source)| format!("{source} as {local}"))
                    .collect::<Vec<_>>();
            names.sort();
            Some((module, names))
        })
        .collect()
}

#[path = "dispatch/typescript.rs"]
mod typescript;

/// Complete the graph builder's owned symbols without changing navigation or
/// semantic-search symbol tables supplied by the shared parser.
pub(crate) fn complete_typescript_symbols<'tree>(
    source: &str,
    lang: LangId,
    root: Node<'tree>,
    symbols: &mut Vec<crate::symbols::Symbol>,
    nav: impl Nav<'tree>,
) -> Vec<Node<'tree>> {
    if matches!(lang, LangId::TypeScript | LangId::Tsx) {
        let nodes = nav.reverse_named_preorder(root);
        typescript::complete_symbols(&nodes, source, symbols, nav);
        nodes
    } else {
        Vec::new()
    }
}

/// Build compact, temporary TypeScript object facts from the supplied syntax
/// tree and attributed call sites, avoiding a second graph extraction.
pub(crate) fn typescript_objects_from_data(
    source: &str,
    lang: LangId,
    nodes: &[Node<'_>],
    data: &crate::callgraph::FileCallData,
) -> Option<ParseBlob> {
    if !matches!(lang, LangId::TypeScript | LangId::Tsx) {
        return None;
    }
    let mut profile = crate::stage_timer::StageTimer::new("typescript_facts");
    let mut refs = Vec::new();
    for (caller, calls) in &data.calls_by_symbol {
        for call in calls.iter().filter(|call| {
            call.kind == crate::callgraph::CallSiteKind::Call && call.full_callee.contains('.')
        }) {
            refs.push(BlobRef {
                ordinal: refs.len() as u32,
                kind: BlobRefKind::Call,
                caller_symbol: Some(caller.clone()),
                short_name: Some(call.callee_name.clone()),
                full_ref: Some(call.full_callee.clone()),
                module_path: None,
                line: call.line,
                byte_start: call.byte_start,
                byte_end: call.byte_end,
                path_override: None,
                local_name: None,
                requested_name: None,
                namespace_alias: None,
                wildcard: false,
                import_kind: None,
            });
        }
    }
    let mut parse = ParseBlob {
        entry_point_symbols: Vec::new(),
        dispatch: DispatchFacts::default(),
        extractor_version: String::new(),
        language: if lang == LangId::Tsx {
            "tsx"
        } else {
            "typescript"
        }
        .into(),
        ast_nodes: Vec::new(),
        symbols: super::blob_symbols(data),
        default_export_symbol: data.default_export_symbol.clone(),
        exported_symbols: data.exported_symbols.clone(),
        callable_symbols: Vec::new(),
        imports: super::blob_imports(data),
        python_bindings: Vec::new(),
        refs,
    };
    profile.finish(
        "inputs",
        format_args!("nodes={} refs={}", nodes.len(), parse.refs.len()),
    );
    // This pass links calls only to methods of object literals and single
    // object-literal returns. Class and parameter types cannot supply those
    // bindings, so do not compute and discard them.
    let objects = typescript::Objects::new_objects_only(nodes, source, &parse, Direct);
    profile.finish("index", format_args!("nodes={}", nodes.len()));
    let mut facts = DispatchFacts::default();
    objects.extract(&mut facts);
    facts.methods.extend_from_slice(objects.methods());
    profile.finish(
        "facts",
        format_args!(
            "methods={} types={}",
            facts.methods.len(),
            facts.types.len()
        ),
    );
    let call_kinds = crate::calls::call_node_kinds(lang);
    let calls = nodes
        .iter()
        .filter(|node| call_kinds.contains(&node.kind()))
        .map(|node| ((node.start_byte(), node.end_byte()), *node))
        .collect::<HashMap<_, _>>();
    profile.finish("calls", format_args!("calls={}", calls.len()));
    for reference in &parse.refs {
        let Some(&call) = calls.get(&(reference.byte_start, reference.byte_end)) else {
            continue;
        };
        let Some(callee) = field(call, &["function", "name"]) else {
            continue;
        };
        if field(callee, &["property"]).is_none() {
            continue;
        }
        let Some(receiver) = field(callee, &["object"]) else {
            continue;
        };
        let ty = if text(receiver, source) == "this" {
            Direct
                .enclosing(call, FUNCTIONS)
                .filter(|function| function.kind() == "method_definition")
                .and_then(|function| typescript::object_member(function, source, Direct))
                .map(|(object, _)| typescript::object_identity(object))
        } else {
            objects.object_receiver(receiver, call)
        };
        if !ty.as_ref().is_some_and(|ty| {
            ty.starts_with("@object:") || ty.starts_with("@return:") || ty.starts_with("@field:")
        }) {
            continue;
        }
        facts.sites.push(SiteHint {
            ordinal: reference.ordinal,
            caller: reference.caller_symbol.clone(),
            line: reference.line,
            member: field(callee, &["property"]).map(|member| text(member, source).to_string()),
            receiver: ty,
            dynamic: false,
        });
    }
    if facts.methods.is_empty() && facts.sites.is_empty() && facts.returns.is_empty() {
        profile.finish("sites", format_args!("sites=0"));
        return None;
    }
    profile.finish("sites", format_args!("sites={}", facts.sites.len()));
    let site_ordinals = facts
        .sites
        .iter()
        .map(|site| site.ordinal)
        .collect::<HashSet<_>>();
    parse
        .refs
        .retain(|reference| site_ordinals.contains(&reference.ordinal));
    // Site hints already own the binding information. The retained join data
    // needs symbol positions and ref offsets, not signatures or caller copies.
    for symbol in &mut parse.symbols {
        symbol.signature = None;
    }
    for reference in &mut parse.refs {
        reference.caller_symbol = None;
        reference.short_name = None;
        reference.full_ref = None;
    }
    parse.dispatch = facts;
    Some(parse)
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DispatchFacts {
    /// The package clause distinguishes co-located external test packages.
    /// Older facts omit it; absence must not authorize a concrete binding.
    #[serde(default)]
    pub go_package: Option<String>,
    pub types: Vec<TypeHint>,
    pub methods: Vec<MethodHint>,
    pub sites: Vec<SiteHint>,
    #[serde(default)]
    pub fields: Vec<FieldHint>,
    #[serde(default)]
    pub returns: Vec<ReturnHint>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FieldHint {
    pub owner: String,
    pub name: String,
    pub ty: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReturnHint {
    pub symbol: String,
    pub ty: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TypeHint {
    pub name: String,
    pub bases: Vec<String>,
    pub interface: bool,
    pub closed: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MethodHint {
    pub owner: String,
    pub trait_name: Option<String>,
    pub name: String,
    pub symbol: String,
    pub has_body: bool,
    pub shape: String,
    pub private: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SiteHint {
    pub ordinal: u32,
    pub caller: Option<String>,
    pub line: u32,
    pub member: Option<String>,
    pub receiver: Option<String>,
    pub dynamic: bool,
}

fn text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    source.get(node.byte_range()).unwrap_or_default()
}
fn field<'a>(node: Node<'a>, names: &[&str]) -> Option<Node<'a>> {
    names.iter().find_map(|name| node.child_by_field_name(name))
}
fn descendants(node: Node<'_>) -> Vec<Node<'_>> {
    let mut result = Vec::new();
    let mut cursor = node.walk();
    let mut depth = 0;
    loop {
        result.push(cursor.node());
        // Preserve reverse named-child preorder: parameter shapes and duplicate
        // span tie-breaking already depend on this order in persisted blobs.
        if cursor.goto_last_child() {
            while !cursor.node().is_named() && cursor.goto_previous_sibling() {}
            if cursor.node().is_named() {
                depth += 1;
                continue;
            }
            cursor.goto_parent();
        }
        loop {
            if depth == 0 {
                return result;
            }
            let mut previous = cursor.goto_previous_sibling();
            while previous && !cursor.node().is_named() {
                previous = cursor.goto_previous_sibling();
            }
            if previous {
                break;
            }
            cursor.goto_parent();
            depth -= 1;
        }
    }
}
fn method_shape(node: Node<'_>, source: &str, language: &str) -> String {
    let parameters = field(node, &["parameters"])
        .map(|p| {
            descendants(p)
                .into_iter()
                .filter(|n| n.kind() == "parameter_declaration")
                .filter_map(|n| field(n, &["type"]).map(|t| text(t, source)))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    let result = field(node, &["result", "return_type"])
        .map(|n| text(n, source).split_whitespace().collect::<String>())
        .unwrap_or_default();
    let receiver = if language == "rust" {
        if super::super::rust_bindings::has_self_parameter(node, source) {
            "self;"
        } else {
            "static;"
        }
    } else {
        ""
    };
    format!("{receiver}{parameters}->{result}")
}

fn rust_receiver_shape(shape: &str) -> Option<bool> {
    // Older method shapes lack receiver information. A missing prefix means
    // unknown, not a static method.
    if shape.starts_with("self;") {
        Some(true)
    } else if shape.starts_with("static;") {
        Some(false)
    } else {
        None
    }
}

pub(crate) fn associated_reference(full: &str) -> bool {
    super::super::parse_method_dispatch(full).is_some_and(|(_, _, _, associated)| associated)
}
fn type_name(raw: &str) -> String {
    raw.trim()
        .trim_start_matches(':')
        .trim()
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim_start_matches('*')
        .trim_start_matches("dyn ")
        .trim_start_matches("impl ")
        .split(['<', '[', '?'])
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}
const TYPES: &[&str] = &[
    "class_declaration",
    "class_definition",
    "interface_declaration",
    "trait_item",
    "struct_item",
    "type_spec",
    "object_declaration",
];
const FUNCTIONS: &[&str] = &[
    "function_declaration",
    "function_definition",
    "function_item",
    "method_definition",
    "method_signature",
    "method_spec",
    "method_elem",
    "method_declaration",
    "function_signature_item",
    "arrow_function",
    "function_expression",
    "generator_function",
    "lambda_expression",
];
fn named<'tree>(node: Node<'tree>, source: &str, nav: impl Nav<'tree>) -> Option<String> {
    if let Some((_, name)) = typescript::object_member(node, source, nav) {
        return Some(name);
    }
    field(node, &["name"])
        .or_else(|| {
            node.named_children(&mut node.walk()).find(|n| {
                ["identifier", "simple_identifier", "type_identifier"].contains(&n.kind())
            })
        })
        .map(|name| text(name, source).to_string())
}
fn owner<'tree>(
    node: Node<'tree>,
    source: &str,
    language: &str,
    nav: impl Nav<'tree>,
) -> Option<(String, Option<String>)> {
    if typescript::supported(language) {
        if let Some((object, _)) = typescript::object_member(node, source, nav) {
            return Some((typescript::object_identity(object), None));
        }
        if matches!(nav.kind(node), "function_expression" | "generator_function") {
            return None;
        }
    }
    if language == "rust" {
        if let Some(implementation) = nav.enclosing(node, &["impl_item"]) {
            return Some((
                type_name(text(field(implementation, &["type"])?, source)),
                field(implementation, &["trait"]).map(|n| type_name(text(n, source))),
            ));
        }
    }
    if language == "go" {
        if let Some(method) = nav.enclosing(node, &["method_declaration"]) {
            let receiver = field(method, &["receiver"])?;
            let parameter = descendants(receiver)
                .into_iter()
                .find(|n| n.kind() == "parameter_declaration")?;
            return Some((type_name(text(field(parameter, &["type"])?, source)), None));
        }
    }
    nav.enclosing(node, TYPES)
        .and_then(|n| named(n, source, nav))
        .map(|n| (n, None))
}

/// Complete member declarations omitted by the legacy callable-only extractor.
/// Interface declarations and trait default bodies need stable node identities
/// even when the old graph never considered them callable.
fn complete_members<'tree>(
    source: &str,
    lang: LangId,
    all: &[Node<'tree>],
    parse: &mut ParseBlob,
    spans: &SpanOrdinals,
    rust_bindings: Option<&RustBindings>,
    nav: impl Nav<'tree>,
) -> Result<(), ManifestJoinError> {
    // Symbol starts may include documentation. End positions still identify the
    // same member, and an indexed lookup avoids scanning all symbols per node.
    let mut identities = HashMap::new();
    for symbol in &parse.symbols {
        identities
            .entry((symbol.end_line, symbol.end_col, symbol.name.clone()))
            .or_insert_with(|| symbol.scoped_name.clone());
    }
    let mut scoped_names = parse
        .symbols
        .iter()
        .map(|s| s.scoped_name.clone())
        .collect::<HashSet<_>>();
    let mut callers = symbol_callers(parse);
    for &node in all {
        if !FUNCTIONS.contains(&nav.kind(node)) {
            continue;
        }
        let Some((owner, _)) = owner(node, source, &parse.language, nav) else {
            continue;
        };
        let Some(name) = named(node, source, nav) else {
            continue;
        };
        let position = (
            node.start_position().row as u32,
            node.start_position().column as u32,
        );
        let identity = (
            node.end_position().row as u32,
            node.end_position().column as u32,
            name.clone(),
        );
        let rust_symbol = rust_bindings.and_then(|index| index.symbol_at(node.start_byte()));
        if let Some(symbol) = rust_symbol {
            if scoped_names.contains(symbol) {
                callers
                    .entry(position)
                    .or_insert_with(|| symbol.to_string());
                continue;
            }
        }
        if let Some(symbol) = identities.get(&identity) {
            callers.entry(position).or_insert_with(|| symbol.clone());
            continue;
        }
        let mut scoped_name = rust_symbol
            .or_else(|| {
                rust_bindings
                    .and_then(|index| index.declaration_at(node.start_byte()))
                    .map(|declaration| declaration.symbol.as_str())
            })
            .map(str::to_string)
            .unwrap_or_else(|| format!("{owner}::{name}"));
        let ordinal = spans
            .get(&(node.start_byte(), node.end_byte()))
            .copied()
            .unwrap_or_default();
        if scoped_names.contains(&scoped_name) {
            scoped_name = format!("{scoped_name}@{ordinal}");
        }
        scoped_names.insert(scoped_name.clone());
        identities.insert(identity, scoped_name.clone());
        callers
            .entry(position)
            .or_insert_with(|| scoped_name.clone());
        parse.symbols.push(BlobSymbol {
            is_test: None,
            ordinal,
            name,
            scoped_name: scoped_name.clone(),
            kind: "method".into(),
            exported: false,
            is_default_export: false,
            start_line: node.start_position().row as u32,
            start_col: node.start_position().column as u32,
            end_line: node.end_position().row as u32,
            end_col: node.end_position().column as u32,
            signature: Some(
                text(node, source)
                    .split('{')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            ),
        });
        if field(node, &["body"]).is_some() {
            parse.callable_symbols.push(scoped_name);
        }
    }
    let mut call_ordinals = parse
        .refs
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == BlobRefKind::Call)
        .map(|(index, r)| (r.ordinal, index))
        .collect::<HashMap<_, _>>();
    for &call in all
        .iter()
        .filter(|n| crate::calls::call_node_kinds(lang).contains(&nav.kind(**n)))
    {
        let callee = field(call, &["function", "name"]).or_else(|| {
            if parse.language == "kotlin" {
                call.named_child(0)
            } else {
                None
            }
        });
        let Some(callee) = callee else {
            continue;
        };
        let Some(&ordinal) = spans.get(&(call.start_byte(), call.end_byte())) else {
            continue;
        };
        if let Some(&index) = call_ordinals.get(&ordinal) {
            if typescript::supported(&parse.language) {
                if let Some(function) = nav.enclosing(call, FUNCTIONS) {
                    if typescript::object_member(function, source, nav).is_some() {
                        if let Some(caller) = callers.get(&(
                            function.start_position().row as u32,
                            function.start_position().column as u32,
                        )) {
                            parse.refs[index].caller_symbol = Some(caller.clone());
                        }
                    }
                }
            }
            continue;
        }
        let caller = nav.enclosing(call, FUNCTIONS).and_then(|function| {
            callers
                .get(&(
                    function.start_position().row as u32,
                    function.start_position().column as u32,
                ))
                .cloned()
        });
        if caller.is_none() {
            continue;
        }
        call_ordinals.insert(ordinal, parse.refs.len());
        let full = text(callee, source).to_string();
        let short = full
            .rsplit(['.', ':'])
            .next()
            .unwrap_or_default()
            .to_string();
        parse.refs.push(BlobRef {
            ordinal,
            kind: BlobRefKind::Call,
            caller_symbol: caller,
            short_name: Some(short),
            full_ref: Some(full),
            module_path: None,
            line: call.start_position().row as u32 + 1,
            byte_start: call.start_byte(),
            byte_end: call.end_byte(),
            path_override: None,
            local_name: None,
            requested_name: None,
            namespace_alias: None,
            wildcard: false,
            import_kind: None,
        });
    }
    parse.refs.sort_by_key(|r| (r.ordinal, r.kind));
    parse.symbols.sort_by_key(|s| s.ordinal);
    parse.callable_symbols.sort();
    parse.callable_symbols.dedup();
    Ok(())
}

/// All source reads occur here, before immutable blob publication.
pub fn extract(
    source: &str,
    lang: LangId,
    root: Node<'_>,
    parse: &mut ParseBlob,
) -> Result<DispatchFacts, ManifestJoinError> {
    extract_with(source, lang, root, parse, Direct)
}

/// [`extract`], walking and looking up with `nav`.
pub(crate) fn extract_with<'tree>(
    source: &str,
    lang: LangId,
    root: Node<'tree>,
    parse: &mut ParseBlob,
    nav: impl Nav<'tree>,
) -> Result<DispatchFacts, ManifestJoinError> {
    let all = nav.reverse_named_preorder(root);
    // Member completion adds symbols but never syntax nodes, so both passes
    // share one span index.
    let spans = exact_ordinals(parse);
    // Member identities and receiver facts consult the same immutable source.
    // Keep one lexical index for both passes and release it with this file.
    let rust_bindings = (lang == LangId::Rust).then(|| RustBindings::new_with(source, root, nav));
    complete_members(
        source,
        lang,
        &all,
        parse,
        &spans,
        rust_bindings.as_ref(),
        nav,
    )?;
    let language = parse.language.as_str();
    let index_receivers = language == "rust";
    #[cfg(test)]
    let index_receivers = index_receivers && !UNINDEXED_RECEIVERS.with(|mode| mode.get());
    let receiver_index = index_receivers
        .then(|| RustReceiverIndex::new(&all, source, rust_bindings.as_ref().unwrap(), nav));
    let rust_bindings = rust_bindings.as_ref();
    let go_index = (language == "go")
        .then(|| crate::callgraph_store::go_dispatch::SourceIndex::new(root, source, false));
    let callers = symbol_callers(parse);
    let mut symbols_by_name_line = HashMap::new();
    for symbol in &parse.symbols {
        symbols_by_name_line
            .entry((symbol.name.as_str(), symbol.start_line))
            .or_insert(symbol);
    }
    let mut functions = HashMap::<(u32, u32), Node<'_>>::new();
    let mut functions_by_end = HashMap::new();
    for &node in all.iter().filter(|n| FUNCTIONS.contains(&nav.kind(**n))) {
        functions_by_end.insert(node.end_byte(), node);
        let key = (
            node.end_position().row as u32,
            node.end_position().column as u32,
        );
        functions
            .entry(key)
            .and_modify(|previous| {
                if node.end_byte() - node.start_byte() < previous.end_byte() - previous.start_byte()
                {
                    *previous = node;
                }
            })
            .or_insert(node);
    }
    let mut facts = DispatchFacts::default();
    facts.go_package = go_index.as_ref().and_then(|index| index.package.clone());
    let ts_objects =
        typescript::supported(language).then(|| typescript::Objects::new(&all, source, parse, nav));
    if let Some(objects) = &ts_objects {
        objects.extract(&mut facts);
    }
    if language == "rust" {
        for node in &all {
            if nav.kind(*node) == "field_declaration" {
                if let (Some(container), Some(name), Some(ty)) = (
                    nav.enclosing(*node, &["struct_item"]),
                    field(*node, &["name"]),
                    field(*node, &["type"]),
                ) {
                    if let Some(owner) = named(container, source, nav) {
                        facts.fields.push(FieldHint {
                            owner: crate::callgraph::rust_module_qualified_name(
                                container, source, &owner, nav,
                            ),
                            name: text(name, source).to_string(),
                            ty: type_name(text(ty, source)),
                        });
                    }
                }
            }
        }
        for symbol in &parse.symbols {
            if let Some(&node) = rust_bindings
                .as_ref()
                .and_then(|index| index.declaration(&symbol.scoped_name))
                .and_then(|declaration| functions_by_end.get(&declaration.bytes.end))
            {
                if let Some(ty) = field(node, &["return_type"]) {
                    let mut ty = type_name(text(ty, source));
                    if ty == "Self" {
                        ty = rust_bindings
                            .as_ref()
                            .and_then(|index| index.declaration_at(node.start_byte()))
                            .and_then(|declaration| declaration.owner.as_ref())
                            .map(|owner| type_name(owner))
                            .or_else(|| owner(node, source, language, nav).map(|(owner, _)| owner))
                            .unwrap_or(ty);
                    }
                    facts.returns.push(ReturnHint {
                        symbol: symbol.scoped_name.clone(),
                        ty,
                    });
                }
            }
        }
    }
    for node in &all {
        let kind = nav.kind(*node);
        if !TYPES.contains(&kind) {
            continue;
        }
        if language == "go" && nav.enclosing(*node, FUNCTIONS).is_some() {
            continue;
        }
        let Some(mut name) = named(*node, source, nav) else {
            continue;
        };
        if language == "rust" {
            name = crate::callgraph::rust_module_qualified_name(*node, source, &name, nav);
        }
        let body = field(*node, &["body", "type"]);
        let interface = kind.contains("interface")
            || kind == "trait_item"
            || body.is_some_and(|n| n.kind() == "interface_type")
            || text(*node, source)
                .split('{')
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .any(|w| w == "abstract")
            || (language == "python" && text(*node, source).contains("@abstractmethod"));
        let mut bases = Vec::new();
        for child in node.named_children(&mut node.walk()) {
            if language != "python"
                && [
                    "class_heritage",
                    "superclasses",
                    "super_interfaces",
                    "superclass",
                    "delegation_specifiers",
                ]
                .contains(&child.kind())
            {
                for base in descendants(child) {
                    if ["type_identifier", "identifier", "user_type"].contains(&base.kind()) {
                        bases.push(type_name(text(base, source)));
                    }
                }
            }
        }
        if language == "python" {
            if let Some(arguments) = field(*node, &["superclasses"]) {
                bases.extend(
                    arguments
                        .named_children(&mut arguments.walk())
                        .filter(|n| n.kind() != "keyword_argument")
                        .map(|n| type_name(text(n, source))),
                );
            }
        }
        if language == "go" {
            if let Some(body) = body {
                for member in descendants(body) {
                    if member.kind() == "field_declaration" && field(member, &["name"]).is_none() {
                        if let Some(t) = field(member, &["type"]) {
                            bases.push(type_name(text(t, source)));
                        }
                    }
                }
            }
        }
        bases.retain(|name| !name.is_empty());
        if language != "python" {
            bases.sort();
            bases.dedup();
        }
        let header = text(*node, source).split('{').next().unwrap_or_default();
        facts.types.push(TypeHint {
            name,
            bases,
            interface,
            closed: header
                .split_whitespace()
                .any(|w| w == "final" || w == "sealed"),
        });
    }
    for symbol in &parse.symbols {
        let node = functions
            .get(&(symbol.end_line, symbol.end_col))
            .or_else(|| {
                (language == "rust")
                    .then(|| {
                        rust_bindings
                            .as_ref()
                            .and_then(|index| index.declaration(&symbol.scoped_name))
                            .and_then(|declaration| functions_by_end.get(&declaration.bytes.end))
                    })
                    .flatten()
            });
        let Some(&node) = node else {
            continue;
        };
        if !["method", "function"].contains(&symbol.kind.as_str()) {
            continue;
        }
        let Some((mut owner, mut trait_name)) = owner(node, source, language, nav) else {
            continue;
        };
        if ts_objects.as_ref().is_some_and(|objects| {
            typescript::object_member(node, source, nav).is_some_and(|(object, name)| {
                name != symbol.name || !objects.unambiguous_member(object, &name)
            })
        }) {
            continue;
        }
        if language == "rust" {
            if let Some(declared) = rust_bindings
                .as_ref()
                .and_then(|index| index.declaration_at(node.start_byte()))
                .and_then(|declaration| declaration.owner.as_ref())
            {
                owner = type_name(declared);
            }
            if let Some(name) = &trait_name {
                let qualified =
                    crate::callgraph::rust_module_qualified_name(node, source, name, nav);
                if facts
                    .types
                    .iter()
                    .any(|ty| ty.interface && ty.name == qualified)
                {
                    trait_name = Some(qualified);
                }
            }
        }
        // TypeScript dispatch uses declaration owners, not structural method
        // signatures (the latter are compared only for Go interface matching).
        facts.methods.push(MethodHint {
            owner,
            trait_name,
            name: symbol.name.clone(),
            symbol: symbol.scoped_name.clone(),
            has_body: field(node, &["body"]).is_some(),
            shape: if typescript::supported(language) {
                String::new()
            } else {
                method_shape(node, source, language)
            },
            private: text(node, source)
                .split('{')
                .next()
                .unwrap_or_default()
                .split_whitespace()
                .any(|w| ["private", "protected", "pub(crate)"].contains(&w)),
        });
    }
    // Some interface grammars expose method signatures without callable symbol
    // metadata. Their declaration still has to be a possible exact target.
    let mut method_names = facts
        .methods
        .iter()
        .map(|m| (m.owner.clone(), m.name.clone(), m.trait_name.clone()))
        .collect::<HashSet<_>>();
    for node in &all {
        if ![
            "method_signature",
            "function_signature_item",
            "method_spec",
            "method_declaration",
        ]
        .contains(&nav.kind(*node))
        {
            continue;
        }
        let Some((mut owner, trait_name)) = owner(*node, source, language, nav) else {
            continue;
        };
        if language == "rust" {
            if let Some(declared) = rust_bindings
                .as_ref()
                .and_then(|index| index.declaration_at(node.start_byte()))
                .and_then(|declaration| declaration.owner.as_ref())
            {
                owner = type_name(declared);
            }
        }
        let Some(name) = named(*node, source, nav) else {
            continue;
        };
        if method_names.contains(&(owner.clone(), name.clone(), trait_name.clone())) {
            continue;
        }
        if let Some(symbol) =
            symbols_by_name_line.get(&(name.as_str(), node.start_position().row as u32))
        {
            let symbol = symbol.scoped_name.clone();
            method_names.insert((owner.clone(), name.clone(), trait_name.clone()));
            facts.methods.push(MethodHint {
                owner,
                trait_name,
                name,
                symbol,
                has_body: field(*node, &["body"]).is_some(),
                shape: if typescript::supported(language) {
                    String::new()
                } else {
                    method_shape(*node, source, language)
                },
                private: text(*node, source)
                    .split('{')
                    .next()
                    .unwrap_or_default()
                    .split_whitespace()
                    .any(|w| ["private", "protected", "pub(crate)"].contains(&w)),
            });
        }
    }
    if language == "rust" {
        let mut type_indices = HashMap::new();
        for (index, ty) in facts.types.iter().enumerate() {
            type_indices.entry(ty.name.clone()).or_insert(index);
        }
        // Trait implementations supply both concrete lookup and default-body
        // inheritance; trait receivers still fan out to every implementation.
        for implementation in all.iter().filter(|n| nav.kind(**n) == "impl_item") {
            if let (Some(ty), Some(trait_node)) = (
                field(*implementation, &["type"]),
                field(*implementation, &["trait"]),
            ) {
                let ty = crate::callgraph::rust_module_qualified_name(
                    *implementation,
                    source,
                    &type_name(text(ty, source)),
                    nav,
                );
                let mut trait_name = type_name(text(trait_node, source));
                let qualified = crate::callgraph::rust_module_qualified_name(
                    *implementation,
                    source,
                    &trait_name,
                    nav,
                );
                if type_indices.contains_key(&qualified) {
                    trait_name = qualified;
                }
                if let Some(t) = type_indices.get(&ty).map(|&i| &mut facts.types[i]) {
                    if !t.bases.contains(&trait_name) {
                        t.bases.push(trait_name);
                    }
                }
            }
        }
    }
    let call_kinds = crate::calls::call_node_kinds(lang);
    let calls = all
        .iter()
        .copied()
        .filter(|n| call_kinds.contains(&nav.kind(*n)))
        .collect::<Vec<_>>();
    let call_ranges = calls
        .iter()
        .map(|n| (n.start_byte(), n.end_byte()))
        .collect::<Vec<_>>();
    let references = parse
        .refs
        .iter()
        .enumerate()
        .filter(|(_, r)| r.kind == BlobRefKind::Call)
        .collect::<Vec<_>>();
    let queries = references
        .iter()
        .map(|(_, r)| (r.byte_start, r.byte_end))
        .collect::<Vec<_>>();
    let targets = super::extraction_index::range_minima(
        &call_ranges,
        &queries,
        super::extraction_index::LookupWork::Dispatch,
    );
    let namespace_imports = parse
        .imports
        .iter()
        .filter_map(|i| i.namespace_import.as_deref())
        .collect::<HashSet<_>>();
    let local_rust_types = facts
        .types
        .iter()
        .map(|ty| ty.name.as_str())
        .collect::<HashSet<_>>();
    // `macro_rules!` definitions, whose templates hold calls no call node
    // covers, with the reference keys those calls' sites are named by.
    let templates = all
        .iter()
        .copied()
        .filter(|node| language == "rust" && nav.kind(*node) == "macro_definition")
        .collect::<Vec<_>>();
    let reference_keys = if templates.is_empty() {
        Vec::new()
    } else {
        parse.reference_keys()
    };
    for ((position, reference), target) in references.into_iter().zip(targets) {
        let Some(call) = target.map(|i| calls[i]) else {
            if let Some(site) = template_self_site(
                reference,
                reference_keys.get(position).copied(),
                &templates,
                &parse.symbols,
            ) {
                facts.sites.push(site);
            }
            continue;
        };
        let Some(callee) = field(call, &["function", "name", "macro"]).or_else(|| {
            if language == "kotlin" {
                call.named_child(0)
            } else {
                None
            }
        }) else {
            continue;
        };
        let callee = if language == "rust" && callee.kind() == "generic_function" {
            field(callee, &["function"]).unwrap_or(callee)
        } else {
            callee
        };
        let associated = language == "rust"
            && callee.kind() == "scoped_identifier"
            && field(callee, &["name"]).is_some_and(|name| text(name, source) == "new");
        if associated {
            let Some(path) = field(callee, &["path"]) else {
                continue;
            };
            let owner = type_name(text(path, source));
            if owner == "Self"
                || local_rust_types.contains(owner.as_str())
                || !owner
                    .rsplit("::")
                    .next()
                    .is_some_and(|name| name.chars().next().is_some_and(char::is_uppercase))
            {
                continue;
            }
        }
        let dynamic = syntactic_dynamic(callee, source, language);
        // `obj["name"]()` spells its member with a string literal: it is a
        // call of `name` as written, which ordinary resolution links by that
        // name, as today's database did. A site would replace that link.
        if !dynamic
            && matches!(language, "javascript" | "typescript" | "tsx")
            && callee.kind() == "subscript_expression"
        {
            continue;
        }
        let receiver_node = field(
            callee,
            &["object", "value", "operand", "expression", "receiver"],
        )
        .or_else(|| {
            if callee.kind() == "navigation_expression" {
                callee.named_child(0)
            } else {
                None
            }
        });
        // Java method invocations carry their receiver on the invocation itself.
        let receiver_node = receiver_node.or_else(|| field(call, &["object"]));
        let receiver_node =
            receiver_node.or_else(|| associated.then(|| field(callee, &["path"])).flatten());
        if !dynamic && receiver_node.is_none() {
            // Token-tree calls already extracted by calls.rs have no receiver
            // expression in the outer syntax tree. Retain the written member
            // as uncertain evidence instead of dropping a real macro caller.
            if language == "rust"
                && call.kind() == "macro_invocation"
                && reference
                    .full_ref
                    .as_deref()
                    .is_some_and(|name| name.contains('.'))
            {
                facts.sites.push(SiteHint {
                    ordinal: reference.ordinal,
                    caller: reference.caller_symbol.clone(),
                    line: reference.line,
                    member: reference.short_name.clone(),
                    receiver: None,
                    dynamic: false,
                });
            }
            continue;
        }
        if receiver_node.is_some_and(|n| namespace_imports.contains(text(n, source)))
            && go_index
                .as_ref()
                .and_then(|index| index.receiver(call.start_byte(), call.end_byte()))
                .is_none()
        {
            // A namespace-qualified function is a static import binding, not an
            // unknown object receiver. Preserve the ordinary manifest resolver.
            continue;
        }
        let receiver = if let Some(index) = &go_index {
            index
                .receiver(call.start_byte(), call.end_byte())
                .map(str::to_string)
        } else {
            receiver_node.and_then(|receiver| {
                if let Some(objects) = &ts_objects {
                    if !matches!(text(receiver, source), "this" | "super") {
                        return objects.receiver(receiver, call);
                    }
                }
                if associated {
                    return Some(text(receiver, source).to_string());
                }
                receiver_type(
                    receiver,
                    call,
                    source,
                    language,
                    &facts,
                    receiver_index.as_ref(),
                    nav,
                )
            })
        };
        facts.sites.push(SiteHint {
            ordinal: reference.ordinal,
            caller: if language == "python" {
                // Defaults and annotations execute in the defining scope; use
                // the caller recorded during extraction, not the enclosing function.
                reference.caller_symbol.clone()
            } else {
                nav.enclosing(call, FUNCTIONS)
                    .and_then(|function| {
                        callers
                            .get(&(
                                function.start_position().row as u32,
                                function.start_position().column as u32,
                            ))
                            .cloned()
                    })
                    .or_else(|| reference.caller_symbol.clone())
            },
            line: reference.line,
            member: if dynamic {
                None
            } else {
                field(callee, &["property", "field", "name"])
                    .or_else(|| {
                        if language == "kotlin" {
                            callee.named_children(&mut callee.walk()).last()
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        if language == "java" {
                            field(call, &["name"])
                        } else {
                            None
                        }
                    })
                    .map(|n| text(n, source).trim_start_matches('.').to_string())
                    .or_else(|| reference.short_name.clone())
            },
            receiver,
            dynamic,
        });
    }
    // Computed calls may have no extracted callee name and hence no BlobRef.
    let mut site_ordinals = facts
        .sites
        .iter()
        .map(|s| s.ordinal)
        .collect::<HashSet<_>>();
    let callable_symbols = parse
        .symbols
        .iter()
        .filter(|s| ["method", "function"].contains(&s.kind.as_str()))
        .collect::<Vec<_>>();
    let symbol_lines = callable_symbols
        .iter()
        .map(|s| (s.start_line as usize, s.end_line as usize))
        .collect::<Vec<_>>();
    let call_lines = calls
        .iter()
        .map(|n| (n.start_position().row, n.end_position().row))
        .collect::<Vec<_>>();
    let dynamic_callers = super::extraction_index::range_minima(
        &symbol_lines,
        &call_lines,
        super::extraction_index::LookupWork::Dispatch,
    );
    for (call, caller) in calls.iter().zip(dynamic_callers) {
        let Some(callee) = field(*call, &["function", "name"]).or_else(|| {
            if language == "kotlin" {
                call.named_child(0)
            } else {
                None
            }
        }) else {
            continue;
        };
        if !syntactic_dynamic(callee, source, language) {
            continue;
        }
        let ordinal = spans
            .get(&(call.start_byte(), call.end_byte()))
            .copied()
            .unwrap_or_default();
        if !site_ordinals.insert(ordinal) {
            continue;
        }
        let caller = caller.map(|i| callable_symbols[i].scoped_name.clone());
        facts.sites.push(SiteHint {
            ordinal,
            caller,
            line: call.start_position().row as u32 + 1,
            member: None,
            receiver: None,
            dynamic: true,
        });
    }
    if language == "python" {
        // A nested body has one syntax ordinal but multiple containing callers.
        // Keep each owner, including defining-scope owners of header expressions.
        facts
            .sites
            .sort_by(|a, b| (a.ordinal, &a.caller).cmp(&(b.ordinal, &b.caller)));
        facts
            .sites
            .dedup_by(|a, b| a.ordinal == b.ordinal && a.caller == b.caller);
    } else {
        facts.sites.sort_by_key(|s| s.ordinal);
        facts.sites.dedup_by_key(|s| s.ordinal);
    }
    facts.methods.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    facts.methods.dedup();
    Ok(facts)
}

/// The ordinal of the first syntax node with each byte span.
type SpanOrdinals = rustc_hash::FxHashMap<(usize, usize), u32>;

fn exact_ordinals(parse: &ParseBlob) -> SpanOrdinals {
    let mut spans = SpanOrdinals::default();
    for node in &parse.ast_nodes {
        spans
            .entry((node.byte_start, node.byte_end))
            .or_insert(node.ordinal);
    }
    spans
}

fn symbol_callers(parse: &ParseBlob) -> HashMap<(u32, u32), String> {
    let mut callers = HashMap::new();
    for symbol in &parse.symbols {
        callers
            .entry((symbol.start_line, symbol.start_col))
            .or_insert_with(|| symbol.scoped_name.clone());
    }
    callers
}
/// The receiver a method-call site records for `self.name(...)` written in
/// a function that a `macro_rules!` template defines, followed by the
/// scoped name of the one function `name` the same template defines. The
/// template's functions are stamped into whatever impl invokes the macro,
/// so `self` there has no type the file names, but `self.name` can only be
/// the template's own `name`. Resolved by that name, as today's database
/// matched it.
pub(crate) const TEMPLATE_SELF_RECEIVER: &str = "@template-self:";

/// The method-call site of `reference` when it is a `self.name(...)` call
/// written inside a function that a `macro_rules!` template among
/// `templates` defines, and the same template defines exactly one other
/// function `name`. Such a call has no call node in the file's syntax tree
/// (a template stays tokens until the macro is expanded); calls.rs finds it
/// by parsing the template. The site is named by the reference's `key`.
fn template_self_site(
    reference: &BlobRef,
    key: Option<u32>,
    templates: &[Node<'_>],
    symbols: &[BlobSymbol],
) -> Option<SiteHint> {
    // A call outside every item the template defines has the macro, `name!`,
    // as its caller; it is a macro-body call, not a method call.
    let caller = reference
        .caller_symbol
        .as_deref()
        .filter(|caller| !caller.ends_with('!'))?;
    let member = reference.full_ref.as_deref()?.strip_prefix("self.")?;
    if member.is_empty() || !member.chars().all(|ch| ch == '_' || ch.is_alphanumeric()) {
        return None;
    }
    let template = templates.iter().find(|template| {
        template.start_byte() <= reference.byte_start && reference.byte_end <= template.end_byte()
    })?;
    let (first, last) = (
        template.start_position().row as u32,
        template.end_position().row as u32,
    );
    let mut defined = symbols.iter().filter(|symbol| {
        symbol.name == member
            && matches!(symbol.kind.as_str(), "function" | "method")
            && symbol.scoped_name != caller
            && !symbol.scoped_name.ends_with('!')
            && first <= symbol.start_line
            && symbol.end_line <= last
    });
    let target = defined.next()?;
    if defined.next().is_some() {
        return None;
    }
    Some(SiteHint {
        ordinal: key?,
        caller: Some(caller.to_string()),
        line: reference.line,
        member: Some(member.to_string()),
        receiver: Some(format!("{TEMPLATE_SELF_RECEIVER}{}", target.scoped_name)),
        dynamic: false,
    })
}

fn syntactic_dynamic(callee: Node<'_>, source: &str, language: &str) -> bool {
    match language {
        "javascript" | "typescript" | "tsx" => {
            callee.kind() == "subscript_expression"
                && crate::calls::extract_computed_member_name(&callee, source).is_none()
        }
        "python" => {
            callee.kind() == "call"
                && field(callee, &["function"]).is_some_and(|n| text(n, source) == "getattr")
        }
        _ => false,
    }
}

/// Rust receiver inference repeatedly consults the same function bodies and
/// constructor declarations. Borrow subtree slices from the file's existing
/// preorder instead of collecting another descendant vector for each receiver.
struct RustReceiverIndex<'nodes, 'tree> {
    nodes: &'nodes [Node<'tree>],
    functions: HashMap<usize, (usize, usize)>,
    constructors: HashSet<String>,
    bindings: &'nodes RustBindings,
}

impl<'nodes, 'tree> RustReceiverIndex<'nodes, 'tree> {
    fn new(
        nodes: &'nodes [Node<'tree>],
        source: &str,
        bindings: &'nodes RustBindings,
        nav: impl Nav<'tree>,
    ) -> Self {
        #[cfg(test)]
        super::extraction_work::note(|work| {
            work.receiver_index_passes += 1;
            work.receiver_index_nodes += nodes.len();
        });
        let mut index = Self {
            nodes,
            functions: HashMap::new(),
            constructors: HashSet::new(),
            bindings,
        };
        let mut ancestors = Vec::<(Node<'tree>, usize, usize)>::new();
        for (position, &node) in nodes.iter().enumerate() {
            // Named-child counts close subtrees without parent searches or
            // ambiguous byte containment at identical and zero-width spans.
            while ancestors
                .last()
                .is_some_and(|(_, _, remaining)| *remaining == 0)
            {
                let (ancestor, start, _) = ancestors.pop().unwrap();
                if FUNCTIONS.contains(&nav.kind(ancestor)) {
                    index.functions.insert(ancestor.id(), (start, position));
                }
            }
            if let Some((_, _, remaining)) = ancestors.last_mut() {
                *remaining -= 1;
            }
            ancestors.push((node, position, node.named_child_count()));
            if nav.kind(node) == "function_item"
                && named(node, source, nav).as_deref() == Some("new")
            {
                if let Some((owner, _)) = owner(node, source, "rust", nav) {
                    if field(node, &["return_type"])
                        .is_some_and(|ty| ["Self", owner.as_str()].contains(&text(ty, source)))
                    {
                        index.constructors.insert(owner);
                    }
                }
            }
        }
        for (ancestor, start, _) in ancestors {
            if FUNCTIONS.contains(&nav.kind(ancestor)) {
                index.functions.insert(ancestor.id(), (start, nodes.len()));
            }
        }
        index
    }

    fn function_nodes(&self, function: Node<'tree>) -> Option<&'nodes [Node<'tree>]> {
        let &(start, end) = self.functions.get(&function.id())?;
        self.nodes.get(start..end)
    }
}

#[cfg(test)]
thread_local! {
    static UNINDEXED_RECEIVERS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn unindexed_constructor_present(
    function: Node<'_>,
    source: &str,
    language: &str,
    ty: &str,
) -> Option<bool> {
    let constructor_nodes = descendants(Direct.enclosing(function, &["source_file"])?);
    #[cfg(test)]
    super::extraction_work::note(|work| {
        work.receiver_constructor_scans += 1;
        work.receiver_constructor_nodes += constructor_nodes.len();
    });
    Some(constructor_nodes.into_iter().any(|node| {
        node.kind() == "function_item"
            && named(node, source, Direct).as_deref() == Some("new")
            && owner(node, source, language, Direct).is_some_and(|(owner, _)| owner == ty)
            && field(node, &["return_type"])
                .is_some_and(|result| ["Self", ty].contains(&text(result, source)))
    }))
}

#[allow(clippy::too_many_arguments)]
fn receiver_type<'tree, N: Nav<'tree>>(
    receiver: Node<'tree>,
    call: Node<'tree>,
    source: &str,
    language: &str,
    facts: &DispatchFacts,
    rust_index: Option<&RustReceiverIndex<'_, 'tree>>,
    nav: N,
) -> Option<String> {
    #[cfg(test)]
    super::extraction_work::note(|work| work.receiver_type_calls += 1);
    let value = text(receiver, source);
    let function = nav.enclosing(call, FUNCTIONS)?;
    #[cfg(test)]
    let unindexed = (language == "rust" && rust_index.is_none()).then(|| {
        let mut root = call;
        while let Some(parent) = root.parent() {
            root = parent;
        }
        super::extraction_work::note(|work| {
            work.receiver_function_nodes += descendants(function).len()
        });
        RustBindings::new(source, root)
    });
    let rust_bindings = rust_index.map(|index| index.bindings);
    #[cfg(test)]
    let rust_bindings = rust_bindings.or(unindexed.as_ref());
    let current_owner = rust_bindings
        .and_then(|index| {
            index
                .symbol_at(function.start_byte())
                .and_then(|symbol| index.declaration(symbol))
        })
        .and_then(|declaration| declaration.owner.as_ref())
        .map(|owner| type_name(owner))
        .or_else(|| owner(function, source, language, nav).map(|(o, _)| o));
    if language == "rust" {
        if receiver.kind() == "call_expression" {
            return field(receiver, &["function"]).map(|n| format!("@return:{}", text(n, source)));
        }
        if receiver.kind() == "field_expression" {
            let base = receiver_type(
                field(receiver, &["value"])?,
                call,
                source,
                language,
                facts,
                rust_index,
                nav,
            )?;
            let member = text(field(receiver, &["field"])?, source);
            return Some(format!("@field:{base}|{member}"));
        }
    }
    match language {
        "typescript" | "tsx" | "javascript" | "java" | "csharp" | "kotlin" if value == "this" => {
            if current_owner
                .as_ref()
                .is_some_and(|name| name.starts_with("@object:"))
                && function.kind() != "method_definition"
            {
                return None;
            }
            return current_owner;
        }
        "python" if value == "self" => return current_owner,
        "python" if value == "cls" => {
            let decorated = nav
                .parent(function)
                .filter(|p| nav.kind(*p) == "decorated_definition")?;
            if text(decorated, source).contains("@classmethod") {
                return current_owner;
            }
            return None;
        }
        "rust" if value == "self" => return current_owner,
        "typescript" | "tsx" | "javascript" | "java" | "csharp" | "kotlin" if value == "super" => {
            return facts
                .types
                .iter()
                .find(|t| Some(&t.name) == current_owner.as_ref())?
                .bases
                .first()
                .cloned()
        }
        "python" if value == "super()" => {
            // Preserve the lexical class: super searches its MRO after that
            // class, rather than dispatching on an open base-class receiver.
            return current_owner.map(|name| format!("@super:{name}"));
        }
        "go" => {
            if let Some(receiver) = field(function, &["receiver"]) {
                for parameter in descendants(receiver) {
                    if parameter.kind() == "parameter_declaration"
                        && field(parameter, &["name"]).is_some_and(|n| text(n, source) == value)
                    {
                        return field(parameter, &["type"]).map(|n| type_name(text(n, source)));
                    }
                }
            }
        }
        _ => {}
    }
    if ![
        "typescript",
        "tsx",
        "javascript",
        "python",
        "rust",
        "go",
        "java",
        "csharp",
        "kotlin",
    ]
    .contains(&language)
    {
        return None;
    }
    if !["identifier", "simple_identifier", "self"].contains(&receiver.kind()) {
        return None;
    }
    if language == "rust" {
        let bindings = rust_bindings?;
        let binding = bindings.binding(value, call.start_byte())?;
        if let Some(annotation) = &binding.annotation {
            let annotation_name = type_name(annotation);
            if let Some(bound) = bindings.bound(&annotation_name, call.start_byte()) {
                return Some(type_name(bound.split('+').next().unwrap_or_default()));
            }
            if bindings.type_parameter(&annotation_name, call.start_byte())
                || annotation.contains('<') && bindings.generic_callable(call.start_byte())
                || annotation_name.contains("::")
                    && !annotation_name
                        .split("::")
                        .next()
                        .is_some_and(|name| name.chars().next().is_some_and(char::is_lowercase))
            {
                return None;
            }
            return Some(annotation_name);
        }
        return match binding.initializer.as_ref()? {
            Initializer::Nominal(name, _) => Some(type_name(name)),
            Initializer::Call { callee, .. } => {
                if let Some((ty, "new")) = callee.split_once("::") {
                    if bindings.declaration(callee).is_some_and(|declaration| {
                        declaration.return_type.as_deref() == Some("Self")
                            && declaration.owner.as_deref() == Some(ty)
                    }) {
                        return Some(type_name(ty));
                    }
                }
                let expression = if let Some(member) = callee
                    .strip_prefix("self.")
                    .or_else(|| callee.strip_prefix("Self::"))
                {
                    format!("{}::{member}", current_owner?)
                } else {
                    callee.clone()
                };
                Some(format!("@return:{expression}"))
            }
        };
    }
    // Only declarations in the enclosing function are evidence. Assignments
    // invalidate constructor inference; arbitrary return-value inference is absent.
    let indexed_nodes = rust_index.and_then(|index| index.function_nodes(function));
    let mut nodes: Cow<'_, [Node<'tree>]> = match indexed_nodes {
        Some(nodes) => Cow::Borrowed(nodes),
        None => Cow::Owned(nav.reverse_named_preorder(function)),
    };
    if ["java", "csharp", "kotlin"].contains(&language) {
        if let Some(class) = nav.enclosing(function, TYPES) {
            nodes
                .to_mut()
                .extend(nav.reverse_named_preorder(class).into_iter().filter(|n| {
                    nav.enclosing(*n, FUNCTIONS).is_none()
                        && [
                            "field_declaration",
                            "variable_declarator",
                            "property_declaration",
                        ]
                        .contains(&nav.kind(*n))
                }));
        }
    }
    #[cfg(test)]
    if indexed_nodes.is_none() {
        super::extraction_work::note(|work| work.receiver_function_nodes += nodes.len());
    }
    for declaration in nodes.iter() {
        if ![
            "required_parameter",
            "optional_parameter",
            "typed_parameter",
            "typed_default_parameter",
            "parameter",
            "parameter_declaration",
            "formal_parameter",
            "variable_declarator",
            "variable_declaration",
            "let_declaration",
            "var_spec",
            "short_var_declaration",
            "assignment",
            "local_variable_declaration",
            "property_declaration",
        ]
        .contains(&nav.kind(*declaration))
        {
            continue;
        }
        let binding = field(*declaration, &["name", "pattern", "left"]).or_else(|| {
            if language == "kotlin" {
                return declaration
                    .named_children(&mut declaration.walk())
                    .find(|n| n.kind() == "simple_identifier");
            }
            if language == "python" && declaration.kind() == "typed_parameter" {
                declaration.named_child(0)
            } else {
                None
            }
        });
        let Some(binding) = binding else {
            continue;
        };
        let binding_text = text(binding, source).trim();
        if binding_text != value && !binding_text.strip_suffix(':').is_some_and(|s| s == value) {
            continue;
        }
        if language == "python" && nav.enclosing(*declaration, FUNCTIONS) != Some(function) {
            continue;
        }
        if let Some(annotation) = field(*declaration, &["type"])
            .or_else(|| {
                nav.parent(*declaration)
                    .filter(|p| {
                        [
                            "variable_declaration",
                            "local_variable_declaration",
                            "field_declaration",
                        ]
                        .contains(&nav.kind(*p))
                    })
                    .and_then(|p| field(p, &["type"]))
            })
            .or_else(|| {
                if language == "kotlin" {
                    declaration
                        .named_children(&mut declaration.walk())
                        .find(|n| n.kind() == "user_type")
                } else {
                    None
                }
            })
        {
            if language != "javascript"
                && !["var", "val"].contains(&text(annotation, source).trim())
            {
                let raw_annotation = text(annotation, source);
                let annotation = type_name(raw_annotation);
                if let Some(parameter) = nodes.iter().find(|n| {
                    ["type_parameter", "type_parameter_declaration"].contains(&nav.kind(**n))
                        && named(**n, source, nav).as_deref() == Some(&annotation)
                }) {
                    if language == "rust" {
                        // A bound identifies the called contract, not its eventual
                        // implementation. Keep that declaration exact and let the
                        // resolver mark implementation fan-out as dispatch.
                        return field(*parameter, &["bounds"]).map(|bounds| {
                            type_name(text(bounds, source).split('+').next().unwrap_or_default())
                        });
                    }
                    return None;
                }
                if language == "rust" {
                    // A generic nominal instantiation or associated type is not
                    // resolved by these syntactic hints. Trait objects and opaque
                    // parameters still name a known contract and retain its type.
                    if raw_annotation.contains('<')
                        && nodes.iter().any(|n| nav.kind(*n) == "type_parameter")
                        || annotation.contains("::")
                            && !annotation
                                .split("::")
                                .next()
                                .is_some_and(|s| s.chars().next().is_some_and(char::is_lowercase))
                    {
                        return None;
                    }
                }
                return Some(annotation);
            }
        }
        // TS annotations are type_annotation children rather than fields on some bindings.
        if language == "typescript" || language == "tsx" {
            if let Some(annotation) = declaration
                .named_children(&mut declaration.walk())
                .find(|n| n.kind() == "type_annotation")
            {
                let annotation = type_name(text(annotation, source));
                if nodes.iter().any(|n| {
                    nav.kind(*n) == "type_parameter"
                        && named(*n, source, nav).as_deref() == Some(&annotation)
                }) {
                    return None;
                }
                return Some(annotation);
            }
        }
        if declaration.start_byte() > call.start_byte() {
            continue;
        }
        let assignments = nodes
            .iter()
            .filter(|n| {
                [
                    "assignment_expression",
                    "augmented_assignment_expression",
                    "assignment",
                    "augmented_assignment",
                    "assignment_statement",
                    "short_var_declaration",
                    "update_expression",
                ]
                .contains(&nav.kind(**n))
                    && field(**n, &["left", "argument"])
                        .is_some_and(|lhs| text(lhs, source).trim() == value)
                    && (language != "python" || nav.enclosing(**n, FUNCTIONS) == Some(function))
            })
            .count();
        if (language == "python" || language == "go") && assignments > 1
            || !["python", "go"].contains(&language) && assignments > 0
        {
            return None;
        }
        let initializer = field(*declaration, &["value", "right"]).or_else(|| {
            if language == "csharp" {
                descendants(*declaration)
                    .into_iter()
                    .find(|n| n.kind() == "object_creation_expression")
            } else {
                None
            }
        })?;
        let initializer = if initializer.kind() == "expression_list" {
            initializer.named_child(0)?
        } else {
            initializer
        };
        match language {
            "typescript" | "tsx" | "javascript" | "java" | "csharp" | "kotlin"
                if initializer.kind() == "new_expression"
                    || initializer.kind() == "object_creation_expression" =>
            {
                return field(initializer, &["constructor", "type"])
                    .map(|n| type_name(text(n, source)))
            }
            "python" if initializer.kind() == "call" => {
                let constructor = field(initializer, &["function"])?;
                let candidate = text(constructor, source);
                if facts.types.iter().any(|t| t.name == candidate) {
                    return Some(candidate.to_string());
                }
            }
            "go" => {
                if let Some(literal) = descendants(initializer)
                    .into_iter()
                    .find(|n| n.kind() == "composite_literal")
                {
                    return field(literal, &["type"]).map(|n| type_name(text(n, source)));
                }
            }
            "rust" => {
                if initializer.kind() == "identifier"
                    && text(initializer, source)
                        .chars()
                        .next()
                        .is_some_and(char::is_uppercase)
                {
                    return Some(text(initializer, source).to_string());
                }
                if initializer.kind() == "struct_expression" {
                    return field(initializer, &["name"]).map(|n| type_name(text(n, source)));
                }
                if initializer.kind() == "call_expression" {
                    let name = text(field(initializer, &["function"])?, source);
                    let Some((ty, method)) = name.split_once("::") else {
                        return Some(format!("@return:{name}"));
                    };
                    if method != "new" {
                        return Some(format!("@return:{name}"));
                    }
                    let valid = match (rust_index, indexed_nodes) {
                        (Some(index), Some(_)) => index.constructors.contains(ty),
                        _ => unindexed_constructor_present(function, source, language, ty)?,
                    };
                    if valid {
                        return Some(ty.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Target {
    pub file: String,
    pub symbol: String,
    pub provenance: &'static str,
}
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Resolution {
    pub targets: BTreeSet<Target>,
    pub protected: BTreeSet<(String, String)>,
    pub unresolved: usize,
    pub external: usize,
    pub dynamic: usize,
}

/// Same-named methods in path order, each file's in file order.
type NamedMethods<'a> = std::rc::Rc<Vec<(String, &'a ParseBlob, &'a MethodHint)>>;

/// What the method-call resolver reads about a checkout's files, when they
/// are loaded on demand instead of all held in its maps. Each answer must
/// equal what those maps give for the same files.
pub(crate) trait DispatchFiles<'a> {
    /// `path`'s facts, or `None` when it has none.
    fn file(&'a self, path: &str) -> Option<&'a ParseBlob>;
    /// Every file with a method named `name`, in path order. May list a few
    /// that have none; the resolver checks each file's facts.
    fn files_with_method(&self, name: &str) -> Vec<String>;
    /// Every file with facts directly in `directory` (`""` for the root), in
    /// path order.
    fn files_in(&self, directory: &str) -> Vec<String>;
    /// Every Go file whose interfaces require a method named `name`, in path
    /// order. May list a few that do not.
    fn go_files_requiring(&self, name: &str) -> Vec<String>;
    /// The target file of `module` as `file` imports it.
    fn import_target(&self, file: &str, module: &str) -> Option<String>;
    /// The target file and symbol of `file`'s receiver type `name`.
    fn type_target(&self, file: &str, name: &str) -> Option<(String, String)>;
    /// Whether `file`'s reference `ordinal` is an associated-function call.
    fn associated_site(&self, file: &str, ordinal: u32) -> bool;
}

/// The ruled resolver operates on a complete manifest's immutable hints. Unknown
/// receivers protect members by written name and expose interface candidates as
/// name-only edges, never as proof of a particular implementation.
pub struct Resolver<'a> {
    pub files: BTreeMap<String, &'a ParseBlob>,
    pub import_targets: BTreeMap<(String, String), String>,
    pub type_targets: BTreeMap<(String, String), (String, String)>,
    associated_sites: HashSet<(String, u32)>,
    /// When set, every file's facts come from here instead of the maps above,
    /// which stay empty.
    source: Option<&'a (dyn DispatchFiles<'a> + 'a)>,
    /// Every method of every file, grouped by method name and in file order,
    /// built on first use. An open or interface receiver then checks only the
    /// same-named methods instead of rescanning the whole manifest per site.
    methods_by_name: std::cell::OnceCell<HashMap<&'a str, NamedMethods<'a>>>,
    /// With a `source`, the same groups, each built the first time its name
    /// is asked for.
    methods_loaded: std::cell::RefCell<HashMap<String, NamedMethods<'a>>>,
    /// Inputs read since the last [`Resolver::take_dependencies`], as
    /// [`dependency`] keys. A resolution can change only when one of them does.
    dependencies: std::cell::RefCell<BTreeSet<String>>,
    #[cfg(test)]
    file_visits: std::cell::Cell<usize>,
}

/// Keys naming what one resolution read. A resolution reads nothing else, so
/// it is unchanged while every key it recorded names unchanged inputs:
/// - [`dependency::file`]: one file's resolver facts (language, Go package, types,
///   methods, fields, returns, imports, default export, and its import and
///   receiver type targets), or that the path has no facts;
/// - [`dependency::method`]: which files of the manifest declare a method of
///   that name, and their facts;
/// - [`dependency::directory`]: the facts of every file in that directory;
/// - [`dependency::GO_INTERFACES`]: the types and methods of every Go file.
pub mod dependency {
    pub fn file(path: &str) -> String {
        format!("r:{path}")
    }
    pub fn method(name: &str) -> String {
        format!("m:{name}")
    }
    pub fn directory(file: &str) -> String {
        format!(
            "d:{}",
            std::path::Path::new(file)
                .parent()
                .map(|parent| parent.to_string_lossy())
                .unwrap_or_default()
        )
    }
    pub const GO_INTERFACES: &str = "g:";
}
impl<'a> Resolver<'a> {
    pub fn new(files: BTreeMap<String, &'a ParseBlob>) -> Self {
        let associated_sites = files
            .iter()
            .filter(|(_, parse)| parse.language == "rust")
            .flat_map(|(file, parse)| {
                parse
                    .refs
                    .iter()
                    .filter(|reference| {
                        reference
                            .full_ref
                            .as_deref()
                            .is_some_and(associated_reference)
                    })
                    .map(move |reference| (file.clone(), reference.ordinal))
            })
            .collect();
        Self {
            files,
            import_targets: BTreeMap::new(),
            type_targets: BTreeMap::new(),
            associated_sites,
            source: None,
            methods_by_name: std::cell::OnceCell::new(),
            methods_loaded: Default::default(),
            dependencies: Default::default(),
            #[cfg(test)]
            file_visits: std::cell::Cell::new(0),
        }
    }
    pub fn note_associated_site(&mut self, file: &str, ordinal: u32) {
        self.associated_sites.insert((file.to_string(), ordinal));
    }
    /// A resolver over `files` with targets and associated-function sites a
    /// caller keeps between runs, so a run over a few files does not copy
    /// them. [`Resolver::into_parts`] gives them back.
    pub fn with_parts(
        files: BTreeMap<String, &'a ParseBlob>,
        import_targets: BTreeMap<(String, String), String>,
        type_targets: BTreeMap<(String, String), (String, String)>,
        associated_sites: HashSet<(String, u32)>,
    ) -> Self {
        Self {
            files,
            import_targets,
            type_targets,
            associated_sites,
            source: None,
            methods_by_name: std::cell::OnceCell::new(),
            methods_loaded: Default::default(),
            dependencies: Default::default(),
            #[cfg(test)]
            file_visits: std::cell::Cell::new(0),
        }
    }
    /// A resolver reading every file's facts from `source`, on demand.
    pub(crate) fn over(source: &'a (dyn DispatchFiles<'a> + 'a)) -> Self {
        let mut resolver = Self::with_parts(
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            HashSet::new(),
        );
        resolver.source = Some(source);
        resolver
    }
    /// The import targets, type targets and associated-function sites given
    /// to [`Resolver::with_parts`].
    pub fn into_parts(
        self,
    ) -> (
        BTreeMap<(String, String), String>,
        BTreeMap<(String, String), (String, String)>,
        HashSet<(String, u32)>,
    ) {
        (
            self.import_targets,
            self.type_targets,
            self.associated_sites,
        )
    }
    fn associated_site(&self, file: &str, ordinal: u32) -> bool {
        match self.source {
            Some(source) => source.associated_site(file, ordinal),
            None => self.associated_sites.contains(&(file.to_string(), ordinal)),
        }
    }
    /// The language of `file`, a file being resolved.
    fn caller_language(&self, file: &str) -> String {
        self.lookup(file)
            .expect("resolved file has facts")
            .language
            .clone()
    }
    /// `path`'s facts without recording a dependency.
    fn lookup(&self, path: &str) -> Option<&'a ParseBlob> {
        match self.source {
            Some(source) => source.file(path),
            None => self.files.get(path).copied(),
        }
    }
    /// Every file with facts directly in `file`'s directory, in path order.
    fn directory_files(&self, file: &str) -> Vec<(String, &'a ParseBlob)> {
        let directory = std::path::Path::new(file).parent();
        match self.source {
            Some(source) => {
                let name = directory
                    .map(|parent| parent.to_string_lossy().into_owned())
                    .unwrap_or_default();
                source
                    .files_in(&name)
                    .into_iter()
                    .filter_map(|path| source.file(&path).map(|parse| (path, parse)))
                    .collect()
            }
            None => self
                .files
                .iter()
                .filter(|(f, _)| std::path::Path::new(f).parent() == directory)
                .map(|(f, p)| (f.clone(), *p))
                .collect(),
        }
    }
    /// Whether some Go file declares an interface requiring a method named
    /// `name` with `shape`.
    fn go_interface_requires(&self, name: &str, shape: &str) -> bool {
        let requires = |p: &ParseBlob| {
            p.language == "go"
                && p.dispatch.types.iter().any(|t| {
                    t.interface
                        && p.dispatch.methods.iter().any(|required| {
                            required.owner == t.name
                                && required.name == name
                                && required.shape == shape
                        })
                })
        };
        match self.source {
            Some(source) => source
                .go_files_requiring(name)
                .iter()
                .filter_map(|path| source.file(path))
                .any(requires),
            None => self.files.values().any(|p| requires(p)),
        }
    }
    fn import_target(&self, file: &str, module: &str) -> Option<String> {
        match self.source {
            Some(source) => source.import_target(file, module),
            None => self
                .import_targets
                .get(&(file.to_string(), module.to_string()))
                .cloned(),
        }
    }
    /// Every key recorded since the previous call.
    pub fn take_dependencies(&self) -> BTreeSet<String> {
        std::mem::take(&mut *self.dependencies.borrow_mut())
    }
    fn depend(&self, key: String) {
        self.dependencies.borrow_mut().insert(key);
    }
    fn file(&self, path: &str) -> Option<&'a ParseBlob> {
        self.depend(dependency::file(path));
        self.lookup(path)
    }
    /// The facts of a file the caller already found through [`Self::file`]
    /// or a recorded lookup.
    fn known_file(&self, path: &str) -> &'a ParseBlob {
        self.file(path).expect("resolved file has facts")
    }
    fn type_target(&self, file: &str, name: &str) -> Option<(String, String)> {
        self.depend(dependency::file(file));
        match self.source {
            Some(source) => source.type_target(file, name),
            None => self
                .type_targets
                .get(&(file.to_string(), name.to_string()))
                .cloned(),
        }
    }
    fn methods_named(&self, member: &str) -> NamedMethods<'a> {
        self.depend(dependency::method(member));
        if let Some(source) = self.source {
            if let Some(found) = self.methods_loaded.borrow().get(member) {
                return std::rc::Rc::clone(found);
            }
            let mut methods = Vec::new();
            for file in source.files_with_method(member) {
                self.note_file_visit();
                let Some(parse) = source.file(&file) else {
                    continue;
                };
                for method in parse.dispatch.methods.iter().filter(|m| m.name == member) {
                    methods.push((file.clone(), parse, method));
                }
            }
            let methods = std::rc::Rc::new(methods);
            self.methods_loaded
                .borrow_mut()
                .insert(member.to_string(), std::rc::Rc::clone(&methods));
            return methods;
        }
        let index = self.methods_by_name.get_or_init(|| {
            let mut index: HashMap<&'a str, Vec<(String, &'a ParseBlob, &'a MethodHint)>> =
                HashMap::new();
            for (file, parse) in &self.files {
                self.note_file_visit();
                let parse: &'a ParseBlob = parse;
                for method in &parse.dispatch.methods {
                    index.entry(method.name.as_str()).or_default().push((
                        file.clone(),
                        parse,
                        method,
                    ));
                }
            }
            index
                .into_iter()
                .map(|(name, methods)| (name, std::rc::Rc::new(methods)))
                .collect()
        });
        index.get(member).cloned().unwrap_or_default()
    }
    fn project_type(&self, file: &str, name: &str) -> Option<(String, &TypeHint)> {
        self.project_type_at_depth(file, name, 0)
    }
    fn note_file_visit(&self) {
        #[cfg(test)]
        self.file_visits.set(self.file_visits.get() + 1);
    }
    fn project_type_at_depth(
        &self,
        file: &str,
        name: &str,
        depth: usize,
    ) -> Option<(String, &TypeHint)> {
        if depth > 16 {
            return None;
        }
        if let Some(expression) = name.strip_prefix("@field:") {
            let (base, member) = expression.rsplit_once('|')?;
            let (base_file, ty) = self.project_type_at_depth(file, base, depth + 1)?;
            let field = self
                .known_file(&base_file)
                .dispatch
                .fields
                .iter()
                .find(|f| f.owner == ty.name && f.name == member)?;
            return self.project_type_at_depth(&base_file, &field.ty, depth + 1);
        }
        if let Some(expression) = name.strip_prefix("@return:") {
            let local;
            let target;
            let (target_file, symbol) = if let Some(found) = self.type_target(file, expression) {
                target = found;
                (&target.0, &target.1)
            } else {
                self.file(file)?
                    .dispatch
                    .returns
                    .iter()
                    .find(|r| r.symbol == expression)?;
                local = (file.to_string(), expression.to_string());
                (&local.0, &local.1)
            };
            let returned = self
                .file(target_file)?
                .dispatch
                .returns
                .iter()
                .find(|r| &r.symbol == symbol)?;
            return self.project_type_at_depth(target_file, &returned.ty, depth + 1);
        }
        let parse = self.file(file)?;
        if parse.language == "go" && parse.dispatch.go_package.is_none() {
            return None;
        }
        if let Some(t) = parse.dispatch.types.iter().find(|t| t.name == name) {
            return Some((file.to_string(), t));
        }
        if let Some((target_file, symbol)) = self.type_target(file, name) {
            let (target_file, symbol) = (&target_file, &symbol);
            if let Some(ty) = self
                .file(target_file)?
                .dispatch
                .types
                .iter()
                .find(|t| &t.name == symbol)
            {
                return Some((target_file.clone(), ty));
            }
        }
        if ["go", "java", "csharp"].contains(&parse.language.as_str()) {
            self.depend(dependency::directory(file));
            let in_directory = self.directory_files(file);
            let mut candidates = in_directory
                .iter()
                .filter(|(f, p)| {
                    self.note_file_visit();
                    p.language == parse.language
                        && (parse.language != "go"
                            || (p.dispatch.go_package == parse.dispatch.go_package
                                && (file.ends_with("_test.go") || !f.ends_with("_test.go"))))
                })
                .flat_map(|(f, p)| {
                    p.dispatch
                        .types
                        .iter()
                        .filter(move |t| t.name == name)
                        .map(move |t| (f.clone(), t))
                });
            if let Some(candidate) = candidates.next() {
                if candidates.next().is_none() {
                    return Some(candidate);
                }
            }
        }
        // Relative imports are resolved only to manifest members. Library imports
        // are not allowed to bind a coincidentally same-named project type.
        for import in &parse.imports {
            if !import
                .names
                .iter()
                .any(|n| n.split_whitespace().last() == Some(name))
                && import.default_import.as_deref() != Some(name)
            {
                continue;
            }
            let imported_name = import
                .names
                .iter()
                .find(|n| n.split_whitespace().last() == Some(name))
                .and_then(|n| n.split_whitespace().next())
                .unwrap_or(name);
            // The import targets belong to the importing file's facts, which
            // this lookup already recorded.
            if let Some(target) = self.import_target(file, &import.module_path) {
                if let Some(candidate) = self.file(&target) {
                    let wanted = if import.default_import.as_deref() == Some(name) {
                        candidate
                            .default_export_symbol
                            .as_deref()
                            .unwrap_or(imported_name)
                    } else {
                        imported_name
                    };
                    if let Some(t) = candidate.dispatch.types.iter().find(|t| t.name == wanted) {
                        return Some((target.clone(), t));
                    }
                }
            }
            if !import.module_path.starts_with('.') {
                return None;
            }
            let parent = std::path::Path::new(file).parent()?;
            let mut parts = Vec::new();
            let joined = parent.join(&import.module_path);
            for c in joined.components() {
                match c {
                    std::path::Component::ParentDir => {
                        parts.pop();
                    }
                    std::path::Component::Normal(p) => parts.push(p.to_string_lossy().to_string()),
                    _ => {}
                }
            }
            let base = parts.join("/");
            // Probe the few names the import can denote, in manifest order,
            // instead of scanning every file of the project for them.
            let mut candidates = std::iter::once(base.clone())
                .chain(
                    ["ts", "tsx", "js", "py", "rs", "go"]
                        .iter()
                        .map(|ext| format!("{base}.{ext}")),
                )
                .collect::<Vec<_>>();
            candidates.sort();
            for path in candidates {
                self.depend(dependency::file(&path));
                let Some(candidate) = self.lookup(&path) else {
                    continue;
                };
                let path = &path;
                self.note_file_visit();
                if candidate.language != parse.language {
                    continue;
                }
                if parse.language == "go" && candidate.dispatch.go_package.is_none() {
                    continue;
                }
                if let Some(t) = candidate
                    .dispatch
                    .types
                    .iter()
                    .find(|t| t.name == imported_name)
                {
                    return Some((path.clone(), t));
                }
            }
            return None;
        }
        None
    }
    fn methods(&self, file: &str, owner: &str, name: &str, associated: bool) -> Vec<Target> {
        let language = &self.known_file(file).language;
        self.methods_named(name)
            .iter()
            .filter(|(method_file, p, m)| {
                &p.language == language
                    && (language != "rust"
                        || associated
                        || rust_receiver_shape(&m.shape) != Some(false))
                    && self
                        .project_type(method_file, &m.owner)
                        .is_some_and(|(f, t)| f == file && t.name == owner)
            })
            .map(|(method_file, _, m)| Target {
                file: method_file.clone(),
                symbol: m.symbol.clone(),
                provenance: "exact",
            })
            .collect()
    }
    fn nearest(
        &self,
        file: &str,
        ty: &TypeHint,
        name: &str,
        seen: &mut BTreeSet<(String, String)>,
        associated: bool,
    ) -> Vec<Target> {
        if !seen.insert((file.to_string(), ty.name.clone())) {
            return Vec::new();
        }
        let direct = self.methods(file, &ty.name, name, associated);
        if !direct.is_empty() {
            return direct;
        }
        let mut frontier = ty
            .bases
            .iter()
            .filter_map(|base| self.project_type(file, base))
            .collect::<Vec<_>>();
        while !frontier.is_empty() {
            let mut hits = Vec::new();
            let mut next = Vec::new();
            for (file, ty) in frontier {
                if !seen.insert((file.clone(), ty.name.clone())) {
                    continue;
                }
                hits.extend(self.methods(&file, &ty.name, name, associated));
                next.extend(
                    ty.bases
                        .iter()
                        .filter_map(|base| self.project_type(&file, base)),
                );
            }
            if !hits.is_empty() {
                return hits;
            }
            frontier = next;
        }
        Vec::new()
    }
    fn subtype(
        &self,
        file: &str,
        ty: &TypeHint,
        base_file: &str,
        base: &str,
        seen: &mut BTreeSet<(String, String)>,
    ) -> bool {
        if !seen.insert((file.to_string(), ty.name.clone())) {
            return false;
        }
        ty.bases
            .iter()
            .filter_map(|b| self.project_type(file, b))
            .any(|(f, t)| {
                (f == base_file && t.name == base) || self.subtype(&f, t, base_file, base, seen)
            })
    }
    fn python_base_type(&self, file: &str, name: &str) -> Option<(String, &TypeHint)> {
        let mut file = file.to_string();
        let mut name = name.to_string();
        let mut visited = BTreeSet::new();
        loop {
            if !visited.insert((file.clone(), name.clone())) {
                return None;
            }
            let parse = self.file(&file)?;
            if parse.language != "python" {
                return None;
            }
            if let Some(ty) = parse.dispatch.types.iter().find(|ty| ty.name == name) {
                return Some((file, ty));
            }
            // Materialized facts keep only the checked module imports. Direct
            // parse-blob users derive that same subset from re-export records.
            let imports = if parse.refs.is_empty() {
                parse
                    .imports
                    .iter()
                    .map(|import| (import.module_path.clone(), import.names.clone()))
                    .collect()
            } else {
                python_reexports(parse)
            };
            let mut targets = BTreeSet::new();
            for (module, names) in imports {
                for specifier in names {
                    if crate::imports::specifier_local_name(&specifier) != name {
                        continue;
                    }
                    for candidate in crate::callgraph_store::python_module_candidates(
                        std::path::Path::new(""),
                        std::path::Path::new(&file),
                        &module,
                    ) {
                        let path = candidate.to_string_lossy().replace('\\', "/");
                        // Probe in Python's package-before-module order, keeping
                        // missing higher-priority files as refresh dependencies.
                        if self.file(&path).is_some() {
                            targets.insert((
                                path,
                                crate::imports::specifier_imported_name(&specifier).to_string(),
                            ));
                            break;
                        }
                    }
                }
            }
            if targets.len() != 1 {
                return None;
            }
            (file, name) = targets.into_iter().next()?;
        }
    }

    fn python_mro(
        &self,
        file: &str,
        ty: &TypeHint,
        active: &mut BTreeSet<(String, String)>,
        memo: &mut BTreeMap<(String, String), Vec<(String, String)>>,
    ) -> Option<Vec<(String, String)>> {
        let key = (file.to_string(), ty.name.clone());
        if let Some(mro) = memo.get(&key) {
            return Some(mro.clone());
        }
        if !active.insert(key.clone()) {
            return None;
        }
        let mut sequences = Vec::new();
        let mut bases = Vec::new();
        for base in &ty.bases {
            if let Some((base_file, base_ty)) = self.python_base_type(file, base) {
                bases.push((base_file.clone(), base_ty.name.clone()));
                sequences.push(self.python_mro(&base_file, base_ty, active, memo)?);
            } else {
                // Unknown bases are opaque barriers, not permission to search
                // later bases by name. The built-in object is a known leaf.
                let key = if base == "object" {
                    (String::new(), "object".to_string())
                } else {
                    (file.to_string(), format!("@unknown:{base}"))
                };
                bases.push(key.clone());
                sequences.push(vec![key]);
            }
        }
        if bases.is_empty() {
            bases.push((String::new(), "object".to_string()));
            sequences.push(bases.clone());
        }
        sequences.push(bases);
        let mut mro = vec![key.clone()];
        // C3 chooses the first head absent from every tail, preserving written
        // base order and postponing shared ancestors in a diamond hierarchy.
        while sequences.iter().any(|sequence| !sequence.is_empty()) {
            let head = sequences
                .iter()
                .filter_map(|sequence| sequence.first())
                .find(|head| {
                    !sequences
                        .iter()
                        .any(|sequence| sequence.iter().skip(1).any(|item| item == *head))
                })?
                .clone();
            for sequence in &mut sequences {
                if sequence.first() == Some(&head) {
                    sequence.remove(0);
                }
            }
            mro.push(head);
        }
        active.remove(&key);
        memo.insert(key, mro.clone());
        Some(mro)
    }

    fn python_super(&self, file: &str, owner: &str, member: &str) -> Resolution {
        let Some((owner_file, ty)) = self.project_type(file, owner) else {
            return self.unknown("python", member);
        };
        let Some(mro) =
            self.python_mro(&owner_file, ty, &mut BTreeSet::new(), &mut BTreeMap::new())
        else {
            return self.unknown("python", member);
        };
        for (base_file, base) in mro.into_iter().skip(1) {
            if base.starts_with("@unknown:") {
                // An unindexed base is not evidence of a project receiver.
                // Keep the call external instead of marking every project initializer live.
                return Resolution {
                    external: 1,
                    ..Resolution::default()
                };
            }
            if base_file.is_empty() {
                continue;
            }
            let targets = self.methods(&base_file, &base, member, false);
            if !targets.is_empty() {
                return Resolution {
                    targets: targets.into_iter().collect(),
                    ..Resolution::default()
                };
            }
        }
        Resolution {
            external: 1,
            ..Resolution::default()
        }
    }

    fn unknown(&self, language: &str, member: &str) -> Resolution {
        // Through `methods_named`, which reads the source when there is one:
        // a resolver over a source holds no files of its own.
        let protected: BTreeSet<_> = self
            .methods_named(member)
            .iter()
            .filter(|(_, parse, _)| parse.language == language)
            .map(|(file, _, method)| (file.clone(), method.symbol.clone()))
            .collect();
        if ["go", "python"].contains(&language) {
            return Resolution {
                unresolved: usize::from(!protected.is_empty()),
                external: usize::from(protected.is_empty()),
                protected,
                ..Resolution::default()
            };
        }
        let targets = protected
            .iter()
            .filter(|(file, symbol)| {
                let parse = self.known_file(file);
                parse
                    .dispatch
                    .methods
                    .iter()
                    .find(|m| &m.symbol == symbol)
                    .is_some_and(|m| {
                        m.trait_name.is_some()
                            || self.project_type(file, &m.owner).is_some_and(|(_, t)| {
                                t.interface
                                    || t.bases.iter().any(|base| {
                                        self.project_type(file, base)
                                            .is_some_and(|(_, b)| b.interface)
                                    })
                            })
                            || language == "go"
                                && {
                                    self.depend(dependency::GO_INTERFACES.to_string());
                                    true
                                }
                                && self.go_interface_requires(&m.name, &m.shape)
                    })
            })
            .map(|(file, symbol)| Target {
                file: file.clone(),
                symbol: symbol.clone(),
                provenance: "name_match",
            })
            .collect();
        Resolution {
            targets,
            unresolved: usize::from(!protected.is_empty()),
            external: usize::from(protected.is_empty()),
            protected,
            ..Resolution::default()
        }
    }
    /// Resolves one method-call site of `file`. The caller's own facts are
    /// read without recording a dependency: an unknown receiver's answer does
    /// not depend on them beyond the language, and [`MemoizedResolver`] shares
    /// that answer across files. Owners treat every file as depending on itself.
    pub fn resolve(&self, file: &str, site: &SiteHint) -> Resolution {
        let parse = self.lookup(file).expect("resolved file has facts");
        if site.dynamic {
            return Resolution {
                dynamic: 1,
                ..Resolution::default()
            };
        }
        let Some(member) = &site.member else {
            return Resolution::default();
        };
        if parse.language == "go" && parse.dispatch.go_package.is_none() {
            return self.unknown("go", member);
        }
        let Some(receiver) = &site.receiver else {
            return self.unknown(&parse.language, member);
        };
        if parse.language == "rust" {
            if let Some(symbol) = receiver.strip_prefix(TEMPLATE_SELF_RECEIVER) {
                return Resolution {
                    targets: BTreeSet::from([Target {
                        file: file.to_string(),
                        symbol: symbol.to_string(),
                        provenance: "name_match",
                    }]),
                    ..Resolution::default()
                };
            }
        }
        if parse.language == "python" {
            if let Some(owner) = receiver.strip_prefix("@super:") {
                return self.python_super(file, owner, member);
            }
        }
        let Some((type_file, ty)) = self.project_type(file, receiver) else {
            return Resolution {
                external: 1,
                ..Resolution::default()
            };
        };
        let associated = parse.language == "rust" && self.associated_site(file, site.ordinal);
        let mut exact = self.nearest(&type_file, ty, member, &mut BTreeSet::new(), associated);
        if parse.language == "rust" {
            // An older method shape may not distinguish an instance method from
            // an associated function. Keep it live without an exact edge.
            if !associated
                && exact.iter().any(|target| {
                    self.known_file(&target.file)
                        .dispatch
                        .methods
                        .iter()
                        .find(|method| method.symbol == target.symbol)
                        .is_some_and(|method| rust_receiver_shape(&method.shape).is_none())
                })
            {
                return Resolution {
                    unresolved: 1,
                    protected: self
                        .methods_named(member)
                        .iter()
                        .filter(|(_, parse, _)| parse.language == "rust")
                        .map(|(file, _, method)| (file.clone(), method.symbol.clone()))
                        .collect(),
                    ..Resolution::default()
                };
            }
            let methods = self
                .known_file(&type_file)
                .dispatch
                .methods
                .iter()
                .filter(|m| {
                    m.owner == ty.name
                        && m.name == *member
                        && (associated || rust_receiver_shape(&m.shape) != Some(false))
                })
                .collect::<Vec<_>>();
            let inherent = methods
                .iter()
                .filter(|m| m.trait_name.is_none())
                .collect::<Vec<_>>();
            if !inherent.is_empty() {
                exact = inherent
                    .iter()
                    .map(|m| Target {
                        file: type_file.clone(),
                        symbol: m.symbol.clone(),
                        provenance: "exact",
                    })
                    .collect();
            } else if !ty.interface {
                exact.retain(|target| {
                    let Some(method) = self
                        .known_file(&target.file)
                        .dispatch
                        .methods
                        .iter()
                        .find(|m| m.symbol == target.symbol)
                    else {
                        return false;
                    };
                    let trait_name = method.trait_name.as_deref().unwrap_or(&method.owner);
                    if site
                        .caller
                        .as_deref()
                        .and_then(|name| name.rsplit_once("::"))
                        .map(|(scope, _)| scope)
                        == method.symbol.rsplit_once("::").map(|(scope, _)| scope)
                    {
                        return true;
                    }
                    let Some((trait_file, declaration)) =
                        self.project_type(&target.file, trait_name)
                    else {
                        return false;
                    };
                    let caller_name = trait_name.rsplit("::").next().unwrap_or(trait_name);
                    self.project_type(file, caller_name).is_some_and(
                        |(caller_file, caller_trait)| {
                            caller_file == trait_file && caller_trait.name == declaration.name
                        },
                    )
                });
                if exact.len() > 1 {
                    return self.unknown(&parse.language, member);
                }
            }
        }
        if parse.language == "go" && exact.len() > 1 {
            return self.unknown(&parse.language, member);
        }
        let mut targets = exact
            .into_iter()
            .filter(|target| {
                parse.language != "go"
                    || file.ends_with("_test.go")
                    || !target.file.ends_with("_test.go")
            })
            .collect::<BTreeSet<_>>();
        if ty.interface || (!ty.closed && !["rust", "go"].contains(&parse.language.as_str())) {
            for (candidate_file, candidate, method) in self.methods_named(member).iter() {
                let (candidate_file, candidate, method) = (candidate_file, *candidate, *method);
                if candidate.language != parse.language {
                    continue;
                }
                if parse.language == "go" && candidate.dispatch.go_package.is_none() {
                    continue;
                }
                if parse.language == "rust"
                    && !associated
                    && rust_receiver_shape(&method.shape) != Some(true)
                {
                    continue;
                }
                {
                    let implements = method.trait_name.as_deref().is_some_and(|name| {
                        self.project_type(candidate_file, name)
                            .is_some_and(|(f, t)| f == type_file && t.name == ty.name)
                    }) || candidate
                        .dispatch
                        .types
                        .iter()
                        .find(|t| t.name == method.owner)
                        .is_some_and(|sub| {
                            self.subtype(
                                candidate_file,
                                sub,
                                &type_file,
                                &ty.name,
                                &mut BTreeSet::new(),
                            )
                        })
                        || (parse.language == "go" && ty.interface && {
                            let required = self
                                .known_file(&type_file)
                                .dispatch
                                .methods
                                .iter()
                                .filter(|m| m.owner == ty.name)
                                .map(|m| (&m.name, &m.shape))
                                .collect::<BTreeSet<_>>();
                            !required.is_empty()
                                && required.iter().all(|(name, shape)| {
                                    candidate.dispatch.methods.iter().any(|m| {
                                        m.owner == method.owner
                                            && &m.name == *name
                                            && &m.shape == *shape
                                    })
                                })
                        });
                    if implements && !(candidate_file == &type_file && method.owner == ty.name) {
                        targets.insert(Target {
                            file: candidate_file.clone(),
                            symbol: method.symbol.clone(),
                            provenance: "dispatch",
                        });
                    }
                }
            }
        }
        let external = usize::from(targets.is_empty());
        Resolution {
            targets,
            external,
            ..Resolution::default()
        }
    }
}

/// Resolution inputs are borrowed immutably for one emission pass. Locations
/// are emitted by the caller. Rust typed lookup also uses the enclosing impl
/// scope and whether the written call is a scoped path.
pub struct MemoizedResolver<'r, 'a> {
    resolver: &'r Resolver<'a>,
    resolved: std::cell::RefCell<BTreeMap<ResolutionKey, MemoizedResolution>>,
}

/// A memoized answer with the dependency keys computing it recorded, which a
/// later hit records again.
type MemoizedResolution = (std::rc::Rc<Resolution>, std::rc::Rc<BTreeSet<String>>);

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct ResolutionKey {
    scope: String,
    receiver: Option<String>,
    member: Option<String>,
    dynamic: bool,
    associated: bool,
    caller_scope: Option<String>,
}

impl<'r, 'a> MemoizedResolver<'r, 'a> {
    pub fn new(resolver: &'r Resolver<'a>) -> Self {
        Self {
            resolver,
            resolved: Default::default(),
        }
    }

    pub fn resolve(&self, file: &str, site: &SiteHint) -> std::rc::Rc<Resolution> {
        let key = ResolutionKey {
            // An unknown receiver consults only language membership, so callers
            // in different files can reuse the same conservative answer. Typed
            // receiver lookup also depends on the caller's imports and directory.
            scope: if site.receiver.is_none() {
                self.resolver.caller_language(file)
            } else {
                file.to_string()
            },
            receiver: site.receiver.clone(),
            member: site.member.clone(),
            dynamic: site.dynamic,
            associated: self.resolver.caller_language(file) == "rust"
                && self.resolver.associated_site(file, site.ordinal),
            caller_scope: (self.resolver.caller_language(file) == "rust"
                && site.receiver.is_some())
            .then(|| {
                site.caller
                    .as_deref()
                    .and_then(|name| name.rsplit_once("::"))
                    .map(|(scope, _)| scope.to_string())
            })
            .flatten(),
        };
        if let Some((resolved, dependencies)) = self.resolved.borrow().get(&key) {
            self.resolver
                .dependencies
                .borrow_mut()
                .extend(dependencies.iter().cloned());
            return resolved.clone();
        }
        let outer = self.resolver.take_dependencies();
        let resolved = std::rc::Rc::new(self.resolver.resolve(file, site));
        let dependencies = self.resolver.take_dependencies();
        {
            let mut recorded = self.resolver.dependencies.borrow_mut();
            *recorded = outer;
            recorded.extend(dependencies.iter().cloned());
        }
        // Large projects can have almost one distinct receiver expression per
        // site. Retain the hot set, not every answer for the whole manifest.
        if self.resolved.borrow().len() < 4096 {
            self.resolved
                .borrow_mut()
                .insert(key, (resolved.clone(), std::rc::Rc::new(dependencies)));
        }
        resolved
    }
}

#[cfg(test)]
#[path = "dispatch/tests.rs"]
mod tests;
