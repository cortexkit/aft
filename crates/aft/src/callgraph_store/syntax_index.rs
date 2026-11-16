pub use aft_parse::callgraph_store::join::syntax_index::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{grammar_for, LangId};

    fn parse(lang: LangId, source: &str) -> tree_sitter::Tree {
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&grammar_for(lang).unwrap()).unwrap();
        parser.parse(source, None).unwrap()
    }

    /// Every node's lookups, as tree-sitter answers them and as the index
    /// does.
    fn lookups<'tree>(
        nav: impl Nav<'tree>,
        node: Node<'tree>,
    ) -> (
        &'static str,
        Option<Node<'tree>>,
        Option<Node<'tree>>,
        Option<Node<'tree>>,
        Option<Node<'tree>>,
        Vec<Node<'tree>>,
        Vec<Node<'tree>>,
    ) {
        let mut children = Vec::new();
        nav.for_each_child(node, |child| children.push(child));
        let mut named = Vec::new();
        nav.for_each_named_child(node, |child| named.push(child));
        (
            nav.kind(node),
            nav.parent(node),
            nav.prev_sibling(node),
            nav.prev_named_sibling(node),
            nav.next_named_sibling(node),
            children,
            named,
        )
    }

    fn walks<'tree>(nav: impl Nav<'tree>, root: Node<'tree>) -> Vec<Vec<(Node<'tree>, String)>> {
        let mut preorder = Vec::new();
        nav.walk(root, |node, kind| {
            preorder.push((node, kind.to_string()));
            true
        });
        // Prune the subtree of every third node, as a pass that stops early.
        let mut pruned = Vec::new();
        nav.walk(root, |node, kind| {
            pruned.push((node, kind.to_string()));
            pruned.len() % 3 != 0
        });
        let mut with_parents = Vec::new();
        nav.walk_with_parent_kind(root, |node, kind, parent| {
            with_parents.push((node, format!("{kind} in {parent:?}")));
        });
        let mut named = Vec::new();
        nav.walk_named(root, |node, kind| {
            named.push((node, kind.to_string()));
            named.len() % 4 != 0
        });
        let reverse = nav
            .reverse_named_preorder(root)
            .into_iter()
            .map(|node| (node, String::new()))
            .collect();
        vec![preorder, pruned, with_parents, named, reverse]
    }

    #[test]
    fn index_answers_equal_tree_sitter_answers() {
        let sources = [
            (
                LangId::Rust,
                "mod a { #[test] /// doc\n fn f() { g(1, h!(x)); } }\nimpl T { fn m(&self) {} }\n",
            ),
            // Error recovery leaves ERROR and zero-width MISSING nodes.
            (LangId::Rust, "fn broken( { let x = ; struct S { a: } }\nfn ok() {}"),
            (
                LangId::TypeScript,
                "export default class A { m() { return this.n(() => 1); } }\nconst o = { f() {}, g: () => 2 };\n",
            ),
            (LangId::TypeScript, "function f(a, { b } = {}) { if (a"),
            (LangId::Python, "class C(B):\n    @property\n    def m(self):\n        return f(self)\n"),
            (LangId::Go, "package main\nfunc (t T) M() { t.n(func() {}) }\n"),
            (LangId::Json, "{\"a\": [1, {\"b\": null}], \"c\": }"),
        ];
        for (lang, source) in sources {
            let tree = parse(lang, source);
            let root = tree.root_node();
            let index = SyntaxIndex::new(root);
            let mut cursor_nodes = Vec::new();
            Direct.walk(root, |node, _| {
                cursor_nodes.push(node);
                true
            });
            for node in cursor_nodes {
                assert_eq!(
                    lookups(&index, node),
                    lookups(Direct, node),
                    "{lang:?}: {node:?}"
                );
                assert_eq!(
                    walks(&index, node),
                    walks(Direct, node),
                    "{lang:?}: {node:?}"
                );
            }
        }
    }
}
