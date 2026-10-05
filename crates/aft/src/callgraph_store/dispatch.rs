//! Source-independent method linking for manifest views. Extraction records only
//! syntactic receiver evidence; joining never reparses or opens checkout files.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tree_sitter::Node;

use super::{BlobRef, BlobRefKind, BlobSymbol, ManifestJoinError, ParseBlob};
use crate::parser::LangId;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DispatchFacts {
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
fn method_shape(node: Node<'_>, source: &str) -> String {
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
    format!("{parameters}->{result}")
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
fn enclosing<'a>(mut node: Node<'a>, kinds: &[&str]) -> Option<Node<'a>> {
    loop {
        if kinds.contains(&node.kind()) {
            return Some(node);
        }
        node = node.parent()?;
    }
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
    "lambda_expression",
];
fn named(node: Node<'_>, source: &str) -> Option<String> {
    field(node, &["name"])
        .or_else(|| {
            node.named_children(&mut node.walk()).find(|n| {
                ["identifier", "simple_identifier", "type_identifier"].contains(&n.kind())
            })
        })
        .map(|name| text(name, source).to_string())
}
fn owner(node: Node<'_>, source: &str, language: &str) -> Option<(String, Option<String>)> {
    if language == "rust" {
        if let Some(implementation) = enclosing(node, &["impl_item"]) {
            return Some((
                type_name(text(field(implementation, &["type"])?, source)),
                field(implementation, &["trait"]).map(|n| type_name(text(n, source))),
            ));
        }
    }
    if language == "go" {
        if let Some(method) = enclosing(node, &["method_declaration"]) {
            let receiver = field(method, &["receiver"])?;
            let parameter = descendants(receiver)
                .into_iter()
                .find(|n| n.kind() == "parameter_declaration")?;
            return Some((type_name(text(field(parameter, &["type"])?, source)), None));
        }
    }
    enclosing(node, TYPES)
        .and_then(|n| named(n, source))
        .map(|n| (n, None))
}

/// Complete member declarations omitted by the legacy callable-only extractor.
/// Interface declarations and trait default bodies need stable node identities
/// even when the old graph never considered them callable.
pub fn complete_members(
    source: &str,
    lang: LangId,
    root: Node<'_>,
    parse: &mut ParseBlob,
) -> Result<(), ManifestJoinError> {
    let all = descendants(root);
    let spans = exact_ordinals(parse);
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
    for &node in &all {
        if !FUNCTIONS.contains(&node.kind()) {
            continue;
        }
        let Some((owner, _)) = owner(node, source, &parse.language) else {
            continue;
        };
        let Some(name) = named(node, source) else {
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
        if let Some(symbol) = identities.get(&identity) {
            callers.entry(position).or_insert_with(|| symbol.clone());
            continue;
        }
        let mut scoped_name = format!("{owner}::{name}");
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
        .filter(|r| r.kind == BlobRefKind::Call)
        .map(|r| r.ordinal)
        .collect::<HashSet<_>>();
    for call in all
        .into_iter()
        .filter(|n| crate::calls::call_node_kinds(lang).contains(&n.kind()))
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
        if call_ordinals.contains(&ordinal) {
            continue;
        }
        let caller = enclosing(call, FUNCTIONS).and_then(|function| {
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
        call_ordinals.insert(ordinal);
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
    parse: &ParseBlob,
) -> Result<DispatchFacts, ManifestJoinError> {
    let all = descendants(root);
    let spans = exact_ordinals(parse);
    let callers = symbol_callers(parse);
    let mut symbols_by_name_line = HashMap::new();
    for symbol in &parse.symbols {
        symbols_by_name_line
            .entry((symbol.name.as_str(), symbol.start_line))
            .or_insert(symbol);
    }
    let mut functions = HashMap::<(u32, u32), Node<'_>>::new();
    for &node in all.iter().filter(|n| FUNCTIONS.contains(&n.kind())) {
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
    let language = parse.language.as_str();
    let mut facts = DispatchFacts::default();
    if language == "rust" {
        for node in &all {
            if node.kind() == "field_declaration" {
                if let (Some(container), Some(name), Some(ty)) = (
                    enclosing(*node, &["struct_item"]),
                    field(*node, &["name"]),
                    field(*node, &["type"]),
                ) {
                    if let Some(owner) = named(container, source) {
                        facts.fields.push(FieldHint {
                            owner,
                            name: text(name, source).to_string(),
                            ty: type_name(text(ty, source)),
                        });
                    }
                }
            }
        }
        for symbol in &parse.symbols {
            if let Some(&node) = functions.get(&(symbol.end_line, symbol.end_col)) {
                if let Some(ty) = field(node, &["return_type"]) {
                    let mut ty = type_name(text(ty, source));
                    if ty == "Self" {
                        ty = owner(node, source, language).map(|(o, _)| o).unwrap_or(ty);
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
        if !TYPES.contains(&node.kind()) {
            continue;
        }
        let Some(name) = named(*node, source) else {
            continue;
        };
        let body = field(*node, &["body", "type"]);
        let interface = node.kind().contains("interface")
            || node.kind() == "trait_item"
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
            if [
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
        bases.sort();
        bases.dedup();
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
        let Some(&node) = functions.get(&(symbol.end_line, symbol.end_col)) else {
            continue;
        };
        if !["method", "function"].contains(&symbol.kind.as_str()) {
            continue;
        }
        let Some((owner, trait_name)) = owner(node, source, language) else {
            continue;
        };
        facts.methods.push(MethodHint {
            owner,
            trait_name,
            name: symbol.name.clone(),
            symbol: symbol.scoped_name.clone(),
            has_body: field(node, &["body"]).is_some(),
            shape: method_shape(node, source),
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
        .contains(&node.kind())
        {
            continue;
        }
        let Some((owner, trait_name)) = owner(*node, source, language) else {
            continue;
        };
        let Some(name) = named(*node, source) else {
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
                shape: method_shape(*node, source),
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
        for implementation in all.iter().filter(|n| n.kind() == "impl_item") {
            if let (Some(ty), Some(trait_node)) = (
                field(*implementation, &["type"]),
                field(*implementation, &["trait"]),
            ) {
                let ty = type_name(text(ty, source));
                let trait_name = type_name(text(trait_node, source));
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
        .filter(|n| call_kinds.contains(&n.kind()))
        .collect::<Vec<_>>();
    let call_ranges = calls
        .iter()
        .map(|n| (n.start_byte(), n.end_byte()))
        .collect::<Vec<_>>();
    let references = parse
        .refs
        .iter()
        .filter(|r| r.kind == BlobRefKind::Call)
        .collect::<Vec<_>>();
    let queries = references
        .iter()
        .map(|r| (r.byte_start, r.byte_end))
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
    for (reference, target) in references.into_iter().zip(targets) {
        let Some(call) = target.map(|i| calls[i]) else {
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
        let dynamic = syntactic_dynamic(callee, source, language);
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
        if receiver_node.is_some_and(|n| namespace_imports.contains(text(n, source))) {
            // A namespace-qualified function is a static import binding, not an
            // unknown object receiver. Preserve the ordinary manifest resolver.
            continue;
        }
        let receiver = receiver_node
            .and_then(|receiver| receiver_type(receiver, call, source, language, &facts));
        facts.sites.push(SiteHint {
            ordinal: reference.ordinal,
            caller: enclosing(call, FUNCTIONS)
                .and_then(|function| {
                    callers
                        .get(&(
                            function.start_position().row as u32,
                            function.start_position().column as u32,
                        ))
                        .cloned()
                })
                .or_else(|| reference.caller_symbol.clone()),
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
    facts.sites.sort_by_key(|s| s.ordinal);
    facts.sites.dedup_by_key(|s| s.ordinal);
    facts.methods.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    facts.methods.dedup();
    Ok(facts)
}

fn exact_ordinals(parse: &ParseBlob) -> HashMap<(usize, usize), u32> {
    let mut spans = HashMap::new();
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
fn syntactic_dynamic(callee: Node<'_>, source: &str, language: &str) -> bool {
    match language {
        "javascript" | "typescript" | "tsx" => callee.kind() == "subscript_expression",
        "python" => {
            callee.kind() == "call"
                && field(callee, &["function"]).is_some_and(|n| text(n, source) == "getattr")
        }
        _ => false,
    }
}

fn receiver_type(
    receiver: Node<'_>,
    call: Node<'_>,
    source: &str,
    language: &str,
    facts: &DispatchFacts,
) -> Option<String> {
    let value = text(receiver, source);
    let function = enclosing(call, FUNCTIONS)?;
    let current_owner = owner(function, source, language).map(|(o, _)| o);
    if language == "rust" {
        if receiver.kind() == "call_expression" {
            return field(receiver, &["function"]).map(|n| format!("@return:{}", text(n, source)));
        }
        if receiver.kind() == "field_expression" {
            let base = receiver_type(field(receiver, &["value"])?, call, source, language, facts)?;
            let member = text(field(receiver, &["field"])?, source);
            return Some(format!("@field:{base}|{member}"));
        }
    }
    match language {
        "typescript" | "tsx" | "javascript" | "java" | "csharp" | "kotlin" if value == "this" => {
            return current_owner
        }
        "python" if value == "self" => return current_owner,
        "python" if value == "cls" => {
            let decorated = function
                .parent()
                .filter(|p| p.kind() == "decorated_definition")?;
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
            return facts
                .types
                .iter()
                .find(|t| Some(&t.name) == current_owner.as_ref())?
                .bases
                .first()
                .cloned()
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
    // Only declarations in the enclosing function are evidence. Assignments
    // invalidate constructor inference; arbitrary return-value inference is absent.
    let mut nodes = descendants(function);
    if ["java", "csharp", "kotlin"].contains(&language) {
        if let Some(class) = enclosing(function, TYPES) {
            nodes.extend(descendants(class).into_iter().filter(|n| {
                enclosing(*n, FUNCTIONS).is_none()
                    && [
                        "field_declaration",
                        "variable_declarator",
                        "property_declaration",
                    ]
                    .contains(&n.kind())
            }));
        }
    }
    for declaration in &nodes {
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
        .contains(&declaration.kind())
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
        if let Some(annotation) = field(*declaration, &["type"])
            .or_else(|| {
                declaration
                    .parent()
                    .filter(|p| {
                        [
                            "variable_declaration",
                            "local_variable_declaration",
                            "field_declaration",
                        ]
                        .contains(&p.kind())
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
                    ["type_parameter", "type_parameter_declaration"].contains(&n.kind())
                        && named(**n, source).as_deref() == Some(&annotation)
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
                        && nodes.iter().any(|n| n.kind() == "type_parameter")
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
                    n.kind() == "type_parameter"
                        && named(*n, source).as_deref() == Some(&annotation)
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
                .contains(&n.kind())
                    && field(**n, &["left", "argument"])
                        .is_some_and(|lhs| text(lhs, source).trim() == value)
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
                    let valid = descendants(enclosing(function, &["source_file"])?)
                        .into_iter()
                        .any(|n| {
                            n.kind() == "function_item"
                                && named(n, source).as_deref() == Some("new")
                                && owner(n, source, language).is_some_and(|(o, _)| o == ty)
                                && field(n, &["return_type"])
                                    .is_some_and(|t| ["Self", ty].contains(&text(t, source)))
                        });
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

/// The ruled resolver operates on a complete manifest's immutable hints. Unknown
/// receivers protect members by written name and expose interface candidates as
/// name-only edges, never as proof of a particular implementation.
pub struct Resolver<'a> {
    pub files: BTreeMap<String, &'a ParseBlob>,
    pub import_targets: BTreeMap<(String, String), String>,
    pub type_targets: BTreeMap<(String, String), (String, String)>,
    #[cfg(test)]
    file_visits: std::cell::Cell<usize>,
}
impl<'a> Resolver<'a> {
    pub fn new(files: BTreeMap<String, &'a ParseBlob>) -> Self {
        Self {
            files,
            import_targets: BTreeMap::new(),
            type_targets: BTreeMap::new(),
            #[cfg(test)]
            file_visits: std::cell::Cell::new(0),
        }
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
            let field = self.files[&base_file]
                .dispatch
                .fields
                .iter()
                .find(|f| f.owner == ty.name && f.name == member)?;
            return self.project_type_at_depth(&base_file, &field.ty, depth + 1);
        }
        if let Some(expression) = name.strip_prefix("@return:") {
            let (target_file, symbol) = self
                .type_targets
                .get(&(file.to_string(), expression.to_string()))?;
            let returned = self.files[target_file]
                .dispatch
                .returns
                .iter()
                .find(|r| &r.symbol == symbol)?;
            return self.project_type_at_depth(target_file, &returned.ty, depth + 1);
        }
        let parse = self.files.get(file)?;
        if let Some(t) = parse.dispatch.types.iter().find(|t| t.name == name) {
            return Some((file.to_string(), t));
        }
        if let Some((target_file, symbol)) =
            self.type_targets.get(&(file.to_string(), name.to_string()))
        {
            if let Some(ty) = self
                .files
                .get(target_file)?
                .dispatch
                .types
                .iter()
                .find(|t| &t.name == symbol)
            {
                return Some((target_file.clone(), ty));
            }
        }
        if ["go", "java", "csharp"].contains(&parse.language.as_str()) {
            let directory = std::path::Path::new(file).parent();
            let mut candidates = self
                .files
                .iter()
                .filter(|(f, p)| {
                    self.note_file_visit();
                    p.language == parse.language && std::path::Path::new(f).parent() == directory
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
            if let Some(target) = self
                .import_targets
                .get(&(file.to_string(), import.module_path.clone()))
            {
                if let Some(candidate) = self.files.get(target) {
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
            for (path, candidate) in &self.files {
                self.note_file_visit();
                if candidate.language != parse.language {
                    continue;
                }
                if path == &base
                    || ["ts", "tsx", "js", "py", "rs", "go"]
                        .iter()
                        .any(|ext| path == &format!("{base}.{ext}"))
                {
                    if let Some(t) = candidate
                        .dispatch
                        .types
                        .iter()
                        .find(|t| t.name == imported_name)
                    {
                        return Some((path.clone(), t));
                    }
                }
            }
            return None;
        }
        None
    }
    fn methods(&self, file: &str, owner: &str, name: &str) -> Vec<Target> {
        self.files
            .iter()
            .filter(|(_, p)| p.language == self.files[file].language)
            .inspect(|_| self.note_file_visit())
            .flat_map(|(method_file, p)| {
                p.dispatch
                    .methods
                    .iter()
                    .filter(move |m| {
                        m.name == name
                            && self
                                .project_type(method_file, &m.owner)
                                .is_some_and(|(f, t)| f == file && t.name == owner)
                    })
                    .map(move |m| Target {
                        file: method_file.clone(),
                        symbol: m.symbol.clone(),
                        provenance: "exact",
                    })
            })
            .collect()
    }
    fn nearest(
        &self,
        file: &str,
        ty: &TypeHint,
        name: &str,
        seen: &mut BTreeSet<(String, String)>,
    ) -> Vec<Target> {
        if !seen.insert((file.to_string(), ty.name.clone())) {
            return Vec::new();
        }
        let direct = self.methods(file, &ty.name, name);
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
                hits.extend(self.methods(&file, &ty.name, name));
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
    fn unknown(&self, language: &str, member: &str) -> Resolution {
        let protected: BTreeSet<_> = self
            .files
            .iter()
            .filter(|(_, p)| p.language == language)
            .inspect(|_| self.note_file_visit())
            .flat_map(|(file, p)| {
                p.dispatch
                    .methods
                    .iter()
                    .filter(move |m| m.name == member)
                    .map(move |m| (file.clone(), m.symbol.clone()))
            })
            .collect();
        let targets = protected
            .iter()
            .filter(|(file, symbol)| {
                let parse = self.files[file];
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
                                && self.files.values().any(|p| {
                                    p.language == "go"
                                        && p.dispatch.types.iter().any(|t| {
                                            t.interface
                                                && p.dispatch.methods.iter().any(|required| {
                                                    required.owner == t.name
                                                        && required.name == m.name
                                                        && required.shape == m.shape
                                                })
                                        })
                                })
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
    pub fn resolve(&self, file: &str, site: &SiteHint) -> Resolution {
        let parse = self.files[file];
        if site.dynamic {
            return Resolution {
                dynamic: 1,
                ..Resolution::default()
            };
        }
        let Some(member) = &site.member else {
            return Resolution::default();
        };
        let Some(receiver) = &site.receiver else {
            return self.unknown(&parse.language, member);
        };
        let Some((type_file, ty)) = self.project_type(file, receiver) else {
            return Resolution {
                external: 1,
                ..Resolution::default()
            };
        };
        let mut exact = self.nearest(&type_file, ty, member, &mut BTreeSet::new());
        if parse.language == "rust" {
            let methods = self.files[&type_file]
                .dispatch
                .methods
                .iter()
                .filter(|m| m.owner == ty.name && m.name == *member)
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
            } else if methods.len() > 1 || exact.len() > 1 {
                return self.unknown(&parse.language, member);
            }
        }
        if parse.language == "go" && exact.len() > 1 {
            return self.unknown(&parse.language, member);
        }
        let mut targets = exact.into_iter().collect::<BTreeSet<_>>();
        if ty.interface || (!ty.closed && !["rust", "go"].contains(&parse.language.as_str())) {
            for (candidate_file, candidate) in &self.files {
                self.note_file_visit();
                if candidate.language != parse.language {
                    continue;
                }
                for method in &candidate.dispatch.methods {
                    if method.name != *member {
                        continue;
                    }
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
                            let required = self.files[&type_file]
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
/// and caller identities are emitted by the caller, not used to choose targets.
pub struct MemoizedResolver<'r, 'a> {
    resolver: &'r Resolver<'a>,
    resolved: std::cell::RefCell<BTreeMap<ResolutionKey, std::rc::Rc<Resolution>>>,
}

#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct ResolutionKey {
    scope: String,
    receiver: Option<String>,
    member: Option<String>,
    dynamic: bool,
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
                self.resolver.files[file].language.clone()
            } else {
                file.to_string()
            },
            receiver: site.receiver.clone(),
            member: site.member.clone(),
            dynamic: site.dynamic,
        };
        if let Some(resolved) = self.resolved.borrow().get(&key) {
            return resolved.clone();
        }
        let resolved = std::rc::Rc::new(self.resolver.resolve(file, site));
        // Large projects can have almost one distinct receiver expression per
        // site. Retain the hot set, not every answer for the whole manifest.
        if self.resolved.borrow().len() < 4096 {
            self.resolved.borrow_mut().insert(key, resolved.clone());
        }
        resolved
    }
}

#[cfg(test)]
#[path = "dispatch/tests.rs"]
mod tests;
