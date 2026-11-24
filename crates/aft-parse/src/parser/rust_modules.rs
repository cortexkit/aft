use tree_sitter::Node;

use super::{node_range_with_decorators, node_text, LangId, RustModule};

enum Step<'tree> {
    Visit(Node<'tree>),
    Finish(RustModule),
}

fn push_children<'tree>(steps: &mut Vec<Step<'tree>>, node: Node<'tree>) {
    let start = steps.len();
    steps.extend(node.named_children(&mut node.walk()).map(Step::Visit));
    steps[start..].reverse();
}

pub fn rust_modules(root: Node<'_>, source: &str) -> Vec<RustModule> {
    let mut steps = Vec::new();
    push_children(&mut steps, root);
    let mut levels = vec![Vec::new()];
    #[cfg(any(test, feature = "test-support"))]
    super::work_counters::RUST_MODULE_SCANS.with(|count| count.set(count.get() + 1));

    // Scan ordinary syntax in source order. Once a module is found, only its
    // body can contribute children; finish that body before visiting siblings.
    while let Some(step) = steps.pop() {
        match step {
            Step::Visit(node) if node.kind() == "mod_item" => {
                let module = RustModule {
                    name: node
                        .child_by_field_name("name")
                        .map(|name| node_text(source, &name))
                        .unwrap_or("module")
                        .to_string(),
                    range: node_range_with_decorators(&node, source, LangId::Rust),
                    exported: node
                        .named_children(&mut node.walk())
                        .any(|child| child.kind() == "visibility_modifier"),
                    children: Vec::new(),
                };
                if let Some(body) = node.child_by_field_name("body") {
                    #[cfg(any(test, feature = "test-support"))]
                    super::work_counters::RUST_MODULE_SCANS
                        .with(|count| count.set(count.get() + 1));
                    levels.push(Vec::new());
                    steps.push(Step::Finish(module));
                    push_children(&mut steps, body);
                } else {
                    levels.last_mut().unwrap().push(module);
                }
            }
            Step::Visit(node) => push_children(&mut steps, node),
            Step::Finish(mut module) => {
                module.children = levels.pop().unwrap();
                levels.last_mut().unwrap().push(module);
            }
        }
    }
    levels.pop().unwrap()
}
