//! Outline extraction for the data languages: JSON/JSONC documents and YAML
//! documents (generic mappings plus the Kubernetes/Argo-shaped clusters).

use tree_sitter::Node;

use super::{node_range, node_range_with_decorators, node_text, LangId};
use crate::error::AftError;
use crate::symbols::{Symbol, SymbolKind};

/// Return the first non-comment value in a JSON document.
///
/// JSONC permits comments before the document value. Tree-sitter exposes those
/// comments as named children, so callers must not assume `named_child(0)` is the
/// object or array that contains the document's data.
pub fn json_document_value<'a>(root: &Node<'a>) -> Option<Node<'a>> {
    let mut cursor = root.walk();
    let value = root
        .named_children(&mut cursor)
        .find(|child| child.kind() != "comment");
    value
}

pub(super) fn extract_json_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let Some(value) = json_document_value(root) else {
        return Ok(Vec::new());
    };

    if value.kind() != "object" {
        return Ok(Vec::new());
    }

    let mut symbols = Vec::new();
    let mut cursor = value.walk();
    for child in value.named_children(&mut cursor) {
        if child.kind() != "pair" {
            continue;
        }
        let Some(key_node) = child.child_by_field_name("key") else {
            continue;
        };
        let name = node_text(source, &key_node).trim_matches('"').to_string();
        if name.is_empty() {
            continue;
        }
        symbols.push(Symbol {
            name,
            kind: SymbolKind::Variable,
            range: node_range_with_decorators(&child, source, LangId::Json),
            signature: None,
            scope_chain: vec![],
            exported: false,
            parent: None,
        });
    }

    Ok(symbols)
}

/// Read a YAML scalar/key node as trimmed text, stripping surrounding quotes.
fn yaml_scalar_text(source: &str, node: &Node) -> String {
    node_text(source, node)
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}

/// Find the mapping that is the direct payload of `node`, descending only
/// through wrapper nodes (`document`, `block_node`, `flow_node`) rather than
/// arbitrary nested mappings. This keeps detection anchored to the document's
/// top-level mapping instead of latching onto a mapping buried inside a
/// sequence or nested value.
fn yaml_find_mapping<'a>(node: &Node<'a>) -> Option<Node<'a>> {
    let mut current = *node;
    loop {
        match current.kind() {
            "block_mapping" | "flow_mapping" => return Some(current),
            "document" | "block_node" | "flow_node" => {
                let mut cursor = current.walk();
                let next = current
                    .named_children(&mut cursor)
                    .find(|child| !matches!(child.kind(), "tag" | "anchor"));
                match next {
                    Some(inner) => current = inner,
                    None => return None,
                }
            }
            _ => return None,
        }
    }
}

/// Look up a top-level key in a YAML mapping and return its value node.
fn yaml_get<'a>(mapping: &Node<'a>, source: &str, key: &str) -> Option<Node<'a>> {
    let mut cursor = mapping.walk();
    for pair in mapping.named_children(&mut cursor) {
        if pair.kind() != "block_mapping_pair" && pair.kind() != "flow_pair" {
            continue;
        }
        let Some(key_node) = pair.child_by_field_name("key") else {
            continue;
        };
        if yaml_scalar_text(source, &key_node) == key {
            return pair.child_by_field_name("value");
        }
    }
    None
}

/// Flatten a YAML value node to clean text for embed_text. Scalars return their
/// trimmed text; block/flow sequences are joined as comma-separated scalar items
/// (so `verbs: [get, list, watch]` becomes `get,list,watch` instead of raw
/// multi-line `- get\n- list` text). Nested mappings are ignored here.
fn yaml_unwrap<'a>(node: &Node<'a>) -> Node<'a> {
    let mut current = *node;
    while current.kind() == "block_node" || current.kind() == "flow_node" {
        let mut cursor = current.walk();
        let next = current
            .named_children(&mut cursor)
            .find(|child| !matches!(child.kind(), "tag" | "anchor"));
        match next {
            Some(inner) => current = inner,
            None => break,
        }
    }
    current
}

fn yaml_flatten_value(source: &str, node: &Node) -> String {
    let node = &yaml_unwrap(node);
    match node.kind() {
        "block_sequence" | "flow_sequence" => {
            let mut items = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                // block_sequence_item wraps a value; flow_sequence holds nodes directly.
                let value = if child.kind() == "block_sequence_item" {
                    child.named_child(0)
                } else {
                    Some(child)
                };
                if let Some(v) = value {
                    let v = yaml_unwrap(&v);
                    // Only flatten scalar leaves; skip mappings (handled elsewhere).
                    if v.kind() != "block_mapping" && v.kind() != "flow_mapping" {
                        let text = yaml_scalar_text(source, &v);
                        if !text.is_empty() {
                            items.push(text);
                        }
                    }
                }
            }
            items.join(",")
        }
        // Mapping values (e.g. container `resources:` block) are not flattened
        // here — their high-signal leaves (cpu/memory) are collected separately.
        // Returning empty avoids dumping raw multi-line YAML into embed_text.
        "block_mapping" | "flow_mapping" => String::new(),
        _ => yaml_scalar_text(source, node),
    }
}

/// Collect `key=value` pairs for the given keys (e.g. image, cpu, memory,
/// verbs) anywhere below `node`, in document order, to enrich embed_text for
/// semantic search. Sequence values are flattened to comma-joined scalars.
/// Capped to avoid runaway output. The walk keeps its pending nodes on the
/// heap rather than recursing, so a deeply nested document cannot overflow
/// the stack.
fn yaml_collect_values(
    source: &str,
    node: &Node,
    keys: &[&str],
    out: &mut Vec<String>,
    cap: usize,
) {
    // Children are pushed in reverse so they are popped in document order.
    let mut pending: Vec<Node> = node.named_children(&mut node.walk()).collect();
    pending.reverse();
    while let Some(child) = pending.pop() {
        if out.len() >= cap {
            return;
        }
        if child.kind() == "block_mapping_pair" || child.kind() == "flow_pair" {
            if let Some(key_node) = child.child_by_field_name("key") {
                let key_text = yaml_scalar_text(source, &key_node);
                if keys.contains(&key_text.as_str()) {
                    if let Some(value_node) = child.child_by_field_name("value") {
                        let value_text = yaml_flatten_value(source, &value_node);
                        if !value_text.is_empty() && out.len() < cap {
                            out.push(format!("{}={}", key_text, value_text));
                        }
                    }
                }
            }
        }
        let start = pending.len();
        pending.extend(child.named_children(&mut child.walk()));
        pending[start..].reverse();
    }
}

/// Collect the `name:` field from every item of a `<parent_key>:` sequence of
/// mappings. The bare `name` key is too generic to match globally (it collides
/// with metadata/container names), so these are gathered by parent key and
/// emitted as `<label>=A,B,...`. Handles k8s `env: [{name,value}]` and Argo
/// Workflow `templates: [{name, ...}]`. Capped. The walk keeps its pending
/// nodes on the heap rather than recursing, so a deeply nested document
/// cannot overflow the stack.
fn yaml_collect_named_items(
    source: &str,
    node: &Node,
    parent_key: &str,
    label: &str,
    out: &mut Vec<String>,
    cap: usize,
) {
    // Children are pushed in reverse so they are popped in document order.
    let mut pending: Vec<Node> = node.named_children(&mut node.walk()).collect();
    pending.reverse();
    while let Some(child) = pending.pop() {
        if (child.kind() == "block_mapping_pair" || child.kind() == "flow_pair")
            && child
                .child_by_field_name("key")
                .map(|k| yaml_scalar_text(source, &k))
                .as_deref()
                == Some(parent_key)
        {
            if let Some(seq) = child.child_by_field_name("value") {
                let seq = yaml_unwrap(&seq);
                let mut names = Vec::new();
                let mut seq_cursor = seq.walk();
                for item in seq.named_children(&mut seq_cursor) {
                    let value = if item.kind() == "block_sequence_item" {
                        item.named_child(0)
                    } else {
                        Some(item)
                    };
                    if let Some(v) = value {
                        if let Some(map) = yaml_find_mapping(&v) {
                            if let Some(name_node) = yaml_get(&map, source, "name") {
                                let n = yaml_scalar_text(source, &name_node);
                                if !n.is_empty() && names.len() < 16 {
                                    names.push(n);
                                }
                            }
                        }
                    }
                }
                if !names.is_empty() && out.len() < cap {
                    out.push(format!("{}={}", label, names.join(",")));
                }
            }
        }
        let start = pending.len();
        pending.extend(child.named_children(&mut child.walk()));
        pending[start..].reverse();
    }
}

/// Tier 1: if a document is a Kubernetes resource (has both apiVersion + kind),
/// emit one rich symbol named `<ns>/<Kind>/<name>` with enriched signature.
/// Generalizes to arbitrary CRDs since apiVersion+kind is the CRD contract.
fn yaml_k8s_resource_symbol(source: &str, doc: &Node, mapping: &Node) -> Option<Symbol> {
    let api_version =
        yaml_get(mapping, source, "apiVersion").map(|n| yaml_scalar_text(source, &n))?;
    let kind = yaml_get(mapping, source, "kind").map(|n| yaml_scalar_text(source, &n))?;
    if api_version.is_empty() || kind.is_empty() {
        return None;
    }

    let (name, namespace) = match yaml_get(mapping, source, "metadata") {
        Some(meta) => match yaml_find_mapping(&meta) {
            Some(meta_map) => {
                // Prefer `name`; fall back to `generateName` (common in Argo
                // Workflows submitted without a fixed name) so the symbol still
                // carries an identifier instead of collapsing to bare <Kind>.
                let name = yaml_get(&meta_map, source, "name")
                    .map(|n| yaml_scalar_text(source, &n))
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        yaml_get(&meta_map, source, "generateName")
                            .map(|n| yaml_scalar_text(source, &n))
                            .filter(|s| !s.is_empty())
                    });
                (
                    name,
                    yaml_get(&meta_map, source, "namespace").map(|n| yaml_scalar_text(source, &n)),
                )
            }
            None => (None, None),
        },
        None => (None, None),
    };

    let res_name = name.clone().filter(|s| !s.is_empty());
    let sym_name = match (&namespace, &res_name) {
        (Some(ns), Some(n)) if !ns.is_empty() => format!("{}/{}/{}", ns, kind, n),
        (_, Some(n)) => format!("{}/{}", kind, n),
        _ => kind.clone(),
    };

    let mut sig = format!("apiVersion={} kind={}", api_version, kind);
    if let Some(ns) = namespace.as_ref().filter(|s| !s.is_empty()) {
        sig.push_str(&format!(" namespace={}", ns));
    }
    if let Some(n) = res_name.as_ref() {
        sig.push_str(&format!(" name={}", n));
    }
    // Enrich with high-signal spec fields so intent queries match. Covers
    // containers (image/ports), resource limits/requests (cpu/memory), RBAC
    // rules (verbs/resources/apiGroups), and storage (volumeMounts mountPath).
    let mut extras = Vec::new();
    yaml_collect_values(
        source,
        mapping,
        &[
            "image",
            "containerPort",
            "port",
            "targetPort",
            "cpu",
            "memory",
            "storage",
            "verbs",
            "resources",
            "apiGroups",
            "mountPath",
            "replicas",
            // Argo Workflow high-signal scalars.
            "entrypoint",
            "command",
            "args",
            "schedule",
        ],
        &mut extras,
        24,
    );
    // Sequence-of-mappings whose `name` key is ambiguous: collect by parent key.
    // k8s env vars and Argo Workflow templates both use the `{name: ...}` shape.
    yaml_collect_named_items(source, mapping, "env", "env", &mut extras, 24);
    yaml_collect_named_items(source, mapping, "templates", "templates", &mut extras, 24);
    if !extras.is_empty() {
        sig.push(' ');
        sig.push_str(&extras.join(" "));
    }

    Some(Symbol {
        name: sym_name,
        kind: SymbolKind::Class,
        range: node_range(doc),
        signature: Some(sig),
        scope_chain: vec![],
        exported: true,
        parent: None,
    })
}

/// Tier 2: generic YAML — emit top-level mapping keys as Variable symbols
/// (docker-compose services, CI jobs, Helm values.yaml, etc.).
fn yaml_generic_keys(source: &str, mapping: &Node, symbols: &mut Vec<Symbol>) {
    let mut cursor = mapping.walk();
    for pair in mapping.named_children(&mut cursor) {
        if pair.kind() != "block_mapping_pair" && pair.kind() != "flow_pair" {
            continue;
        }
        let Some(key_node) = pair.child_by_field_name("key") else {
            continue;
        };
        let name = yaml_scalar_text(source, &key_node);
        if name.is_empty() {
            continue;
        }
        symbols.push(Symbol {
            name,
            kind: SymbolKind::Variable,
            range: node_range(&pair),
            signature: None,
            scope_chain: vec![],
            exported: false,
            parent: None,
        });
    }
}

/// Extract symbols from a YAML stream. Handles multi-document (`---`) streams:
/// each document becomes a Kubernetes resource symbol (Tier 1) when it carries
/// apiVersion+kind, otherwise its top-level keys are emitted (Tier 2). Helm/Go
/// templated YAML that yields no parseable mapping degrades gracefully (the
/// document is skipped rather than failing the whole file).
pub(super) fn extract_yaml_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let mut symbols = Vec::new();
    let mut cursor = root.walk();
    for doc in root.named_children(&mut cursor) {
        if doc.kind() != "document" {
            continue;
        }
        let Some(mapping) = yaml_find_mapping(&doc) else {
            continue;
        };
        if let Some(symbol) = yaml_k8s_resource_symbol(source, &doc, &mapping) {
            symbols.push(symbol);
        } else {
            yaml_generic_keys(source, &mapping, &mut symbols);
        }
    }
    Ok(symbols)
}
