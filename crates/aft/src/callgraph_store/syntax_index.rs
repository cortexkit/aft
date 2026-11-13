//! One walk over a file's syntax tree, shared by every extraction pass.
//!
//! Tree-sitter nodes carry no parent pointer: `Node::parent`, and the
//! sibling steps built on it, search down from the root on every call, and
//! `Node::kind` measures and checks its name each time. Extraction asks both
//! questions for most nodes in many passes, and walked the whole tree again
//! in most of them. [`SyntaxIndex`] walks the tree once and records each
//! node's kind, parent, siblings and subtree; the passes iterate it and ask
//! it instead.
//!
//! The passes take their lookups through [`Nav`], so one copy of each rule
//! runs either on tree-sitter's own lookups ([`Direct`], the extractor the
//! index replaced) or on the index. Every index answer equals tree-sitter's.
//! The one place the two could disagree is a zero-width node (an empty or
//! MISSING token), where tree-sitter's root-down search has its own tie
//! rules; for those the index asks tree-sitter.

use rustc_hash::FxHashMap;
use tree_sitter::Node;

/// No node: the root's parent, or a missing sibling.
const NONE: u32 = u32::MAX;

/// How an extraction pass walks a tree and looks up a node's kind and
/// neighbours.
pub(crate) trait Nav<'tree>: Copy {
    /// `node.kind()`.
    fn kind(self, node: Node<'tree>) -> &'static str;
    /// `node.parent()`.
    fn parent(self, node: Node<'tree>) -> Option<Node<'tree>>;
    /// `node.prev_sibling()`.
    fn prev_sibling(self, node: Node<'tree>) -> Option<Node<'tree>>;
    /// `node.prev_named_sibling()`.
    fn prev_named_sibling(self, node: Node<'tree>) -> Option<Node<'tree>>;
    /// `node.next_named_sibling()`.
    fn next_named_sibling(self, node: Node<'tree>) -> Option<Node<'tree>>;
    /// Visits `root` and every node under it in preorder, as a tree cursor
    /// does, skipping the subtree of each node `visit` returns `false` for.
    fn walk(self, root: Node<'tree>, visit: impl FnMut(Node<'tree>, &'static str) -> bool);
    /// Visits `root` and every node under it in preorder, as a tree cursor
    /// does, with the kind of the node the cursor came down from (`None`
    /// for `root`).
    fn walk_with_parent_kind(
        self,
        root: Node<'tree>,
        visit: impl FnMut(Node<'tree>, &'static str, Option<&'static str>),
    );
    /// Visits nodes as a stack walk does that starts with `root`, pops a
    /// node, and pushes its named children in order when `visit` returns
    /// `true` for it.
    fn walk_named(self, root: Node<'tree>, visit: impl FnMut(Node<'tree>, &'static str) -> bool);
    /// Calls `visit` with each child of `node`, named or not, in order.
    fn for_each_child(self, node: Node<'tree>, visit: impl FnMut(Node<'tree>));
    /// Calls `visit` with each named child of `node`, in order.
    fn for_each_named_child(self, node: Node<'tree>, visit: impl FnMut(Node<'tree>));
    /// `root`, then its named children's subtrees from the last child to the
    /// first: the order [`Nav::walk_named`] visits when it prunes nothing.
    fn reverse_named_preorder(self, root: Node<'tree>) -> Vec<Node<'tree>>;

    /// The first of `node` and its ancestors whose kind is one of `kinds`.
    fn enclosing(self, mut node: Node<'tree>, kinds: &[&str]) -> Option<Node<'tree>> {
        loop {
            if kinds.contains(&self.kind(node)) {
                return Some(node);
            }
            node = self.parent(node)?;
        }
    }
}

/// Tree-sitter's own lookups and cursor walks.
#[derive(Clone, Copy)]
pub(crate) struct Direct;

impl<'tree> Nav<'tree> for Direct {
    fn kind(self, node: Node<'tree>) -> &'static str {
        node.kind()
    }

    fn parent(self, node: Node<'tree>) -> Option<Node<'tree>> {
        node.parent()
    }

    fn prev_sibling(self, node: Node<'tree>) -> Option<Node<'tree>> {
        node.prev_sibling()
    }

    fn prev_named_sibling(self, node: Node<'tree>) -> Option<Node<'tree>> {
        node.prev_named_sibling()
    }

    fn next_named_sibling(self, node: Node<'tree>) -> Option<Node<'tree>> {
        node.next_named_sibling()
    }

    fn walk(self, root: Node<'tree>, mut visit: impl FnMut(Node<'tree>, &'static str) -> bool) {
        let mut cursor = root.walk();
        loop {
            let node = cursor.node();
            if visit(node, node.kind()) && cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return;
                }
            }
        }
    }

    fn walk_with_parent_kind(
        self,
        root: Node<'tree>,
        mut visit: impl FnMut(Node<'tree>, &'static str, Option<&'static str>),
    ) {
        let mut cursor = root.walk();
        let mut parents: Vec<&'static str> = Vec::new();
        'walk: loop {
            let node = cursor.node();
            let kind = node.kind();
            visit(node, kind, parents.last().copied());
            if cursor.goto_first_child() {
                parents.push(kind);
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    break 'walk;
                }
                parents.pop();
            }
        }
    }

    fn walk_named(
        self,
        root: Node<'tree>,
        mut visit: impl FnMut(Node<'tree>, &'static str) -> bool,
    ) {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if visit(node, node.kind()) {
                stack.extend(node.named_children(&mut node.walk()));
            }
        }
    }

    fn for_each_child(self, node: Node<'tree>, mut visit: impl FnMut(Node<'tree>)) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            visit(child);
        }
    }

    fn for_each_named_child(self, node: Node<'tree>, mut visit: impl FnMut(Node<'tree>)) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            visit(child);
        }
    }

    fn reverse_named_preorder(self, root: Node<'tree>) -> Vec<Node<'tree>> {
        let mut result = Vec::new();
        let mut cursor = root.walk();
        let mut depth = 0;
        loop {
            result.push(cursor.node());
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
}

/// A syntax tree's nodes in preorder, as a tree cursor visits them (every
/// named and anonymous node, hidden nodes flattened away), with each one's
/// kind, parent, previous and next siblings and subtree.
pub(crate) struct SyntaxIndex<'tree> {
    nodes: Vec<Node<'tree>>,
    kinds: Vec<&'static str>,
    named: Vec<bool>,
    parents: Vec<u32>,
    previous_siblings: Vec<u32>,
    next_siblings: Vec<u32>,
    /// One past the position of each node's last descendant.
    subtree_ends: Vec<u32>,
    /// Each node's position by node id, built on the first lookup of a node
    /// that came from somewhere other than the index (a field, a child).
    positions: std::cell::OnceCell<FxHashMap<usize, u32>>,
    reverse_named: Vec<Node<'tree>>,
    /// Each grammar symbol's name by symbol id: `node.kind()` is the name
    /// of `node.kind_id()`.
    kind_names: Vec<Option<&'static str>>,
}

impl<'tree> SyntaxIndex<'tree> {
    /// Walks the tree under `root` once.
    pub(crate) fn new(root: Node<'tree>) -> Self {
        let language = root.language();
        let kind_names = (0..language.node_kind_count())
            .map(|id| {
                u16::try_from(id)
                    .ok()
                    .and_then(|id| language.node_kind_for_id(id))
            })
            .collect();
        let mut index = Self {
            nodes: Vec::new(),
            kinds: Vec::new(),
            named: Vec::new(),
            parents: Vec::new(),
            previous_siblings: Vec::new(),
            next_siblings: Vec::new(),
            subtree_ends: Vec::new(),
            positions: std::cell::OnceCell::new(),
            reverse_named: Vec::new(),
            kind_names,
        };
        let mut cursor = root.walk();
        // Each open ancestor of the node being visited, with the last child
        // seen under it so far.
        let mut ancestors: Vec<(u32, u32)> = Vec::new();
        'walk: loop {
            let node = cursor.node();
            let position = index.nodes.len() as u32;
            let (parent, previous) = match ancestors.last_mut() {
                Some((parent, last_child)) => {
                    let previous = std::mem::replace(last_child, position);
                    (*parent, previous)
                }
                None => (NONE, NONE),
            };
            if previous != NONE {
                index.next_siblings[previous as usize] = position;
            }
            index.nodes.push(node);
            index.kinds.push(index.kind_name(node));
            index.named.push(node.is_named());
            index.parents.push(parent);
            index.previous_siblings.push(previous);
            index.next_siblings.push(NONE);
            index.subtree_ends.push(position + 1);
            if cursor.goto_first_child() {
                ancestors.push((position, NONE));
                continue;
            }
            loop {
                if cursor.goto_next_sibling() {
                    break;
                }
                if !cursor.goto_parent() {
                    break 'walk;
                }
                let (closed, _) = ancestors.pop().expect("every parent was entered");
                index.subtree_ends[closed as usize] = index.nodes.len() as u32;
            }
        }
        index.reverse_named = index.reverse_named_from(0);
        index
    }

    /// Every node in preorder with its kind.
    pub(crate) fn preorder_kinds(&self) -> impl Iterator<Item = (Node<'tree>, &'static str)> + '_ {
        self.nodes.iter().copied().zip(self.kinds.iter().copied())
    }

    fn kind_name(&self, node: Node<'tree>) -> &'static str {
        match self.kind_names.get(usize::from(node.kind_id())) {
            Some(Some(name)) => name,
            // ERROR nodes have a reserved id outside the symbol table.
            _ => node.kind(),
        }
    }

    fn position(&self, node: Node<'tree>) -> Option<u32> {
        if self.nodes.first() == Some(&node) {
            return Some(0);
        }
        self.positions
            .get_or_init(|| {
                self.nodes
                    .iter()
                    .enumerate()
                    .map(|(position, node)| (node.id(), position as u32))
                    .collect()
            })
            .get(&node.id())
            .copied()
    }

    fn reverse_named_from(&self, root: u32) -> Vec<Node<'tree>> {
        let mut order = Vec::new();
        let mut stack = vec![root];
        while let Some(position) = stack.pop() {
            order.push(self.nodes[position as usize]);
            // Pushing the named children in order pops the last one first.
            let end = self.subtree_ends[position as usize];
            let mut child = position + 1;
            while child < end {
                if self.named[child as usize] {
                    stack.push(child);
                }
                child = self.subtree_ends[child as usize];
            }
        }
        order
    }

    /// The node at `position`, unless it is `NONE` or zero-width. A
    /// zero-width neighbour is where tree-sitter's root-down search has its
    /// own tie rules, so the caller asks tree-sitter instead.
    fn neighbour(&self, position: u32) -> Result<Option<Node<'tree>>, ()> {
        if position == NONE {
            return Ok(None);
        }
        let node = self.nodes[position as usize];
        if node.start_byte() == node.end_byte() {
            return Err(());
        }
        Ok(Some(node))
    }

    /// The step `links` records for a non-empty `node`, or `None` when
    /// tree-sitter has to answer.
    fn step(&self, node: Node<'tree>, links: &[u32]) -> Option<Option<Node<'tree>>> {
        if node.start_byte() == node.end_byte() {
            return None;
        }
        let position = self.position(node)?;
        self.neighbour(links[position as usize]).ok()
    }

    /// The nearest named sibling of `node` along `links`, or `None` when
    /// tree-sitter has to answer.
    fn named_step(&self, node: Node<'tree>, links: &[u32]) -> Option<Option<Node<'tree>>> {
        if node.start_byte() == node.end_byte() {
            return None;
        }
        let mut position = links[self.position(node)? as usize];
        loop {
            let sibling = self.neighbour(position).ok()?;
            match sibling {
                None => return Some(None),
                Some(sibling) if self.named[position as usize] => return Some(Some(sibling)),
                Some(_) => position = links[position as usize],
            }
        }
    }
}

impl<'tree> Nav<'tree> for &SyntaxIndex<'tree> {
    fn kind(self, node: Node<'tree>) -> &'static str {
        self.kind_name(node)
    }

    fn parent(self, node: Node<'tree>) -> Option<Node<'tree>> {
        if node.start_byte() == node.end_byte() {
            return node.parent();
        }
        match self.position(node) {
            Some(position) => {
                let parent = self.parents[position as usize];
                (parent != NONE).then(|| self.nodes[parent as usize])
            }
            None => node.parent(),
        }
    }

    fn prev_sibling(self, node: Node<'tree>) -> Option<Node<'tree>> {
        self.step(node, &self.previous_siblings)
            .unwrap_or_else(|| node.prev_sibling())
    }

    fn prev_named_sibling(self, node: Node<'tree>) -> Option<Node<'tree>> {
        self.named_step(node, &self.previous_siblings)
            .unwrap_or_else(|| node.prev_named_sibling())
    }

    fn next_named_sibling(self, node: Node<'tree>) -> Option<Node<'tree>> {
        self.named_step(node, &self.next_siblings)
            .unwrap_or_else(|| node.next_named_sibling())
    }

    fn walk(self, root: Node<'tree>, mut visit: impl FnMut(Node<'tree>, &'static str) -> bool) {
        let Some(start) = self.position(root) else {
            return Direct.walk(root, visit);
        };
        let end = self.subtree_ends[start as usize];
        let mut position = start;
        while position < end {
            let index = position as usize;
            position = if visit(self.nodes[index], self.kinds[index]) {
                position + 1
            } else {
                self.subtree_ends[index]
            };
        }
    }

    fn walk_with_parent_kind(
        self,
        root: Node<'tree>,
        mut visit: impl FnMut(Node<'tree>, &'static str, Option<&'static str>),
    ) {
        let Some(start) = self.position(root) else {
            return Direct.walk_with_parent_kind(root, visit);
        };
        visit(root, self.kinds[start as usize], None);
        for position in start + 1..self.subtree_ends[start as usize] {
            let index = position as usize;
            let parent = self.parents[index] as usize;
            visit(
                self.nodes[index],
                self.kinds[index],
                Some(self.kinds[parent]),
            );
        }
    }

    fn walk_named(
        self,
        root: Node<'tree>,
        mut visit: impl FnMut(Node<'tree>, &'static str) -> bool,
    ) {
        let Some(start) = self.position(root) else {
            return Direct.walk_named(root, visit);
        };
        let mut stack = vec![start];
        while let Some(position) = stack.pop() {
            let index = position as usize;
            if visit(self.nodes[index], self.kinds[index]) {
                let end = self.subtree_ends[index];
                let mut child = position + 1;
                while child < end {
                    if self.named[child as usize] {
                        stack.push(child);
                    }
                    child = self.subtree_ends[child as usize];
                }
            }
        }
    }

    fn for_each_child(self, node: Node<'tree>, mut visit: impl FnMut(Node<'tree>)) {
        let Some(position) = self.position(node) else {
            return Direct.for_each_child(node, visit);
        };
        let end = self.subtree_ends[position as usize];
        let mut child = position + 1;
        while child < end {
            visit(self.nodes[child as usize]);
            child = self.subtree_ends[child as usize];
        }
    }

    fn for_each_named_child(self, node: Node<'tree>, mut visit: impl FnMut(Node<'tree>)) {
        let Some(position) = self.position(node) else {
            return Direct.for_each_named_child(node, visit);
        };
        let end = self.subtree_ends[position as usize];
        let mut child = position + 1;
        while child < end {
            if self.named[child as usize] {
                visit(self.nodes[child as usize]);
            }
            child = self.subtree_ends[child as usize];
        }
    }

    fn reverse_named_preorder(self, root: Node<'tree>) -> Vec<Node<'tree>> {
        match self.position(root) {
            Some(0) => self.reverse_named.clone(),
            Some(position) => self.reverse_named_from(position),
            None => Direct.reverse_named_preorder(root),
        }
    }
}

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
