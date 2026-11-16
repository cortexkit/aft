//! Identity-proven field autoderef hints. These are resolution inputs, not types
//! to display in a tool response.

use std::collections::{HashMap, HashSet};

use super::{text, type_name, Nav, Node};

pub(crate) fn parts(name: &str) -> Option<(&str, &str, &str)> {
    let (wrapper, name) = name.strip_prefix("@smart:")?.split_once(':')?;
    let (outer, inner) = name.split_once('|')?;
    Some((wrapper, outer, inner))
}

pub(super) struct Fields {
    bindings: HashMap<usize, HashMap<String, Option<String>>>,
    globs: HashSet<usize>,
    box_prelude: bool,
}

fn scope<'tree>(mut node: Node<'tree>, nav: impl Nav<'tree>) -> Node<'tree> {
    loop {
        if matches!(nav.kind(node), "source_file" | "declaration_list" | "block") {
            return node;
        }
        let Some(parent) = nav.parent(node) else {
            return node;
        };
        node = parent;
    }
}

fn type_parameter<'tree>(
    mut node: Node<'tree>,
    name: &str,
    source: &str,
    nav: impl Nav<'tree>,
) -> bool {
    loop {
        if let Some(parameters) = node.child_by_field_name("type_parameters") {
            if parameters
                .named_children(&mut parameters.walk())
                .any(|parameter| {
                    parameter
                        .child_by_field_name("name")
                        .is_some_and(|n| text(n, source) == name)
                })
            {
                return true;
            }
        }
        let Some(parent) = nav.parent(node) else {
            return false;
        };
        if nav.kind(node) == "mod_item" {
            return false;
        }
        node = parent;
    }
}

impl Fields {
    pub(super) fn new<'tree>(nodes: &[Node<'tree>], source: &str, nav: impl Nav<'tree>) -> Self {
        let mut result = Self {
            bindings: HashMap::new(),
            globs: HashSet::new(),
            box_prelude: !nodes.iter().any(|node| {
                matches!(nav.kind(*node), "inner_attribute_item" | "attribute_item")
                    && (text(*node, source).contains("no_std")
                        || text(*node, source).contains("no_implicit_prelude"))
            }),
        };
        for &node in nodes {
            let Some(parent) = nav.parent(node) else {
                continue;
            };
            let id = scope(parent, nav).id();
            if nav.kind(node) == "use_declaration" {
                let raw = text(node, source);
                if raw.contains('*') {
                    result.globs.insert(id);
                }
                for (local, path) in crate::callgraph::rust_import_paths(raw) {
                    result.insert(id, local, Some(path));
                }
            } else if matches!(
                nav.kind(node),
                "struct_item"
                    | "enum_item"
                    | "union_item"
                    | "trait_item"
                    | "type_item"
                    | "mod_item"
                    | "extern_crate_declaration"
            ) {
                if let Some(name) = node
                    .child_by_field_name("alias")
                    .or_else(|| node.child_by_field_name("name"))
                {
                    let standard_crate = (nav.kind(node) == "extern_crate_declaration")
                        .then(|| node.child_by_field_name("name"))
                        .flatten()
                        .map(|name| text(name, source))
                        .filter(|name| matches!(*name, "std" | "alloc"))
                        .map(str::to_string);
                    result.insert(id, text(name, source).to_string(), standard_crate);
                }
            }
        }
        result
    }

    fn insert(&mut self, scope: usize, name: String, path: Option<String>) {
        self.bindings
            .entry(scope)
            .or_default()
            .entry(name)
            .and_modify(|old| *old = None)
            .or_insert(path);
    }

    fn binding<'a, 'tree>(
        &'a self,
        node: Node<'tree>,
        name: &str,
        source: &str,
        nav: impl Nav<'tree>,
    ) -> Option<Option<&'a str>> {
        let mut current = Some(node);
        while let Some(node) = current {
            if let Some(parameters) = node.child_by_field_name("type_parameters") {
                for parameter in parameters.named_children(&mut parameters.walk()) {
                    if parameter
                        .child_by_field_name("name")
                        .is_some_and(|n| text(n, source) == name)
                    {
                        return Some(None);
                    }
                }
            }
            if let Some(binding) = self
                .bindings
                .get(&node.id())
                .and_then(|bindings| bindings.get(name))
            {
                return Some(binding.as_deref());
            }
            if self.globs.contains(&node.id()) {
                return Some(None);
            }
            // Inline modules have their own imports; a parent's imports are not
            // inherited. Blocks, in contrast, inherit their enclosing scope.
            if nav.kind(node) == "mod_item" {
                break;
            }
            current = nav.parent(node);
        }
        None
    }

    fn identity<'tree>(
        &self,
        node: Node<'tree>,
        path: &str,
        source: &str,
        nav: impl Nav<'tree>,
    ) -> Option<&'static str> {
        let path = path.trim_start_matches("::");
        let (head, tail) = path.split_once("::").unwrap_or((path, ""));
        let resolved = match self.binding(node, head, source, nav) {
            Some(Some(imported)) => {
                if tail.is_empty() {
                    imported.to_string()
                } else {
                    format!("{imported}::{tail}")
                }
            }
            Some(None) => return None,
            None if path == "Box" && self.box_prelude => return Some("Box"),
            None => path.to_string(),
        };
        let root = resolved.split("::").next().unwrap();
        match self.binding(node, root, source, nav) {
            None => {}
            Some(Some(imported)) if imported == root => {}
            _ => return None,
        }
        match resolved.as_str() {
            "std::sync::Arc" | "alloc::sync::Arc" => Some("Arc"),
            "std::rc::Rc" | "alloc::rc::Rc" => Some("Rc"),
            "std::boxed::Box" | "alloc::boxed::Box" => Some("Box"),
            _ => None,
        }
    }

    pub(super) fn field_type<'tree>(
        &self,
        node: Node<'tree>,
        source: &str,
        nav: impl Nav<'tree>,
    ) -> String {
        self.field_type_at_depth(node, node, source, nav, 0)
    }

    fn field_type_at_depth<'tree>(
        &self,
        node: Node<'tree>,
        context: Node<'tree>,
        source: &str,
        nav: impl Nav<'tree>,
        depth: usize,
    ) -> String {
        let ordinary = type_name(text(node, source));
        if depth > 16 || nav.kind(node) != "generic_type" {
            return ordinary;
        }
        let Some(outer) = node.child_by_field_name("type") else {
            return ordinary;
        };
        let Some(wrapper) = self.identity(context, text(outer, source), source, nav) else {
            return ordinary;
        };
        let Some(arguments) = node.child_by_field_name("type_arguments") else {
            return ordinary;
        };
        let mut cursor = arguments.walk();
        let mut arguments = arguments.named_children(&mut cursor);
        let Some(inner) = arguments.next() else {
            return ordinary;
        };
        // Allocator arguments and trait objects retain their existing dispatch.
        // In particular, this must not turn Box<dyn Trait> into concrete dispatch.
        if arguments.next().is_some()
            || !matches!(
                nav.kind(inner),
                "type_identifier" | "scoped_type_identifier" | "generic_type"
            )
        {
            return ordinary;
        }
        let inner_name = type_name(text(inner, source));
        if type_parameter(
            context,
            inner_name.split("::").next().unwrap_or_default(),
            source,
            nav,
        ) {
            return ordinary;
        }
        let inner = self.field_type_at_depth(inner, context, source, nav, depth + 1);
        format!("@smart:{wrapper}:{ordinary}|{inner}")
    }
}

pub(super) fn owns_method(wrapper: &str, member: &str) -> bool {
    // Autoderef considers receiver methods on the wrapper before T. These
    // unconditional std impls, including blanket-trait methods, must not be
    // mistaken for an inherent T method.
    // Associated functions (new, leak, downgrade, ...) have no self parameter
    // and cannot win a dot call. Conditional delegating impls (for example,
    // Box<T>: Iterator when T: Iterator) forward to T, so do not block them.
    matches!(
        member,
        "as_ref" | "borrow" | "borrow_mut" | "deref" | "drop" | "into" | "try_into" | "type_id"
    ) || match wrapper {
        "Arc" | "Rc" => matches!(
            member,
            "clone" | "clone_from" | "to_owned" | "clone_into" | "clone_to_uninit"
        ),
        "Box" => matches!(member, "as_mut" | "deref_mut"),
        _ => false,
    }
}
