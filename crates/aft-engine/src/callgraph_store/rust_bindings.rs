pub use aft_parse::callgraph_store::rust_bindings::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::join::syntax_index::Nav;
#[cfg(test)]
#[allow(unused_imports)]
use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};
#[cfg(test)]
#[allow(unused_imports)]
use std::ops::Range;
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::Node;

#[cfg(test)]
mod tests {
    use super::*;

    fn index(source: &str) -> RustBindings {
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&crate::parser::grammar_for(crate::parser::LangId::Rust).unwrap())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        RustBindings::new(source, tree.root_node())
    }

    #[test]
    fn local_bindings_are_indexed_by_callable_and_lexical_scope() {
        let source = "fn run(value: &Item) { let value = Item::new(); value.m(); { let value = opaque(); value.m(); } value.m(); let closure = |value| value.m(); }";
        let index = index(source);
        let calls = source
            .match_indices("value.m()")
            .map(|(byte, _)| byte)
            .collect::<Vec<_>>();
        let callee = |byte| match index.binding("value", byte).unwrap().initializer.as_ref() {
            Some(Initializer::Call { callee, .. }) => Some(callee.as_str()),
            _ => None,
        };
        assert_eq!(callee(calls[0]), Some("Item::new"));
        assert_eq!(callee(calls[1]), Some("opaque"));
        assert_eq!(callee(calls[2]), Some("Item::new"));
        assert_eq!(callee(calls[3]), None);
    }

    #[test]
    fn return_declarations_use_owner_identity_not_name_sized_ranges() {
        let source = "struct Item; impl Item { /// Factory.\n fn new() -> Self { Self } } trait Analyzer { fn get(&self) -> &dyn Analyzer { todo!() } }";
        let index = index(source);
        assert_eq!(
            index
                .declaration("Item::new")
                .unwrap()
                .return_type
                .as_deref(),
            Some("Self")
        );
        assert_eq!(
            index
                .declaration("Analyzer::get")
                .unwrap()
                .return_type
                .as_deref(),
            Some("&dyn Analyzer")
        );
        assert!(index.declaration("new").is_none());
    }

    #[test]
    fn duplicate_owner_declarations_are_not_guessed() {
        let source =
            "struct Item; impl Item { fn new() -> Self { Self } fn new() -> Self { Self } }";
        let index = index(source);
        assert!(index.declaration("Item::new").is_none());
        let positions = source
            .match_indices("fn new")
            .map(|(byte, _)| byte)
            .collect::<Vec<_>>();
        assert_eq!(positions.len(), 2);
        for position in positions {
            let declaration = index.declaration_at(position).unwrap();
            assert_eq!(declaration.symbol, "Item::new");
            assert_eq!(declaration.owner.as_deref(), Some("Item"));
            assert_eq!(declaration.bytes.start, position);
            assert!(index.symbol_at(position).is_none());
        }
    }

    #[test]
    fn distinct_trait_implementations_keep_distinct_declarations() {
        let index = index("trait I { fn m(&self); } struct A; struct B; impl I for A { fn m(&self) {} } impl I for B { fn m(&self) {} }");
        assert!(index.declaration("I::m").is_some());
        assert!(index.declaration("I for A::m").is_some());
        assert!(index.declaration("I for B::m").is_some());
    }

    #[test]
    fn bare_values_require_a_unique_unit_struct_declaration() {
        let index = index(
            "struct Unit; struct Named { field: u8 } struct Tuple(u8); mod inner { struct Unit; }",
        );
        assert!(index.unit_struct("Unit"));
        assert!(index.unit_struct("inner::Unit"));
        assert!(!index.unit_struct("Named"));
        assert!(!index.unit_struct("Tuple"));
    }

    #[test]
    fn local_inline_owners_are_qualified_without_moving_imported_types() {
        let index = index("struct Root; mod local { struct Item; impl Item { fn new() -> Self { Self } } } mod implementation { use super::Root; impl Root { fn new() -> Self { Root } } }");
        assert_eq!(
            index
                .declaration("local::Item::new")
                .unwrap()
                .owner
                .as_deref(),
            Some("local::Item")
        );
        assert_eq!(
            index.declaration("Root::new").unwrap().owner.as_deref(),
            None
        );
    }
}
