pub use aft_parse::callgraph_store::go_dispatch::*;

// Shared import scope for the unit tests below.
#[cfg(test)]
#[allow(unused_imports)]
use super::*;
#[cfg(test)]
#[allow(unused_imports)]
use tree_sitter::Node;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_concrete_index_work_is_linear_and_lookups_do_not_walk_source() {
        let mut visits = Vec::new();
        for count in [40, 160] {
            let source = format!("package p\ntype T struct{{}}\nfunc (x *T) run() {{}}\nfunc factory() *T {{ return &T{{}} }}\nfunc caller() {{\n x := factory()\n{}\n}}\n", "x.run()\n".repeat(count));
            let tree =
                parse_source_with_cached_parser(Path::new("indexed.go"), &source, LangId::Go)
                    .unwrap();
            WORK.with(|work| work.set((0, 0, 0)));
            let index = SourceIndex::new(tree.root_node(), &source, false);
            let work = WORK.with(|work| work.get());
            assert_eq!(work.1, 3, "analyze each function once, not once per call");
            assert_eq!(work.2, 2, "receiver and local bindings are analyzed once");
            assert_eq!(index.calls.len(), count);
            for (&(start, end), ty) in &index.calls {
                assert_eq!(ty.as_deref(), Some("T"));
                assert_eq!(index.receiver(start, end), Some("T"));
            }
            assert!(index.calls_by_line.is_empty());
            assert_eq!(
                WORK.with(|work| work.get()),
                work,
                "span lookups must not walk the source or function"
            );
            WORK.with(|work| work.set((0, 0, 0)));
            let lines = SourceIndex::new(tree.root_node(), &source, true);
            assert!(lines.calls.is_empty());
            assert_eq!(WORK.with(|work| work.get()), work);
            for calls in lines.calls_by_line.values() {
                assert_eq!(
                    calls.get(&("x".into(), "run".into())).unwrap().as_deref(),
                    Some("T")
                );
            }
            assert_eq!(
                WORK.with(|work| work.get()),
                work,
                "lookups must not walk the source or function"
            );
            visits.push(work.0);
        }
        assert!(
            visits[1] < visits[0] * 5,
            "four times as many calls must have linear indexing work: {visits:?}"
        );
    }

    #[test]
    fn go_concrete_repeated_parameter_scope_index_has_bounded_lookup_work() {
        let mut visits = Vec::new();
        for count in [40, 160] {
            let source = format!("package p\ntype T struct{{}}\nfunc (r *T) run() {{}}\nfunc caller() {{ var callbacks=[]func(x *T){{\n{}\n}}; _=callbacks }}", "func(x *T) { x.run() },\n".repeat(count));
            let tree =
                parse_source_with_cached_parser(Path::new("callbacks.go"), &source, LangId::Go)
                    .unwrap();
            WORK.with(|work| work.set((0, 0, 0)));
            let index = SourceIndex::new(tree.root_node(), &source, false);
            let work = WORK.with(|work| work.get());
            assert_eq!(work.1, 2);
            assert_eq!(
                work.2, 3,
                "function-type parameter names are not value bindings"
            );
            assert_eq!(index.calls.len(), count);
            assert!(index.calls.values().all(|ty| ty.as_deref() == Some("T")));
            for &(start, end) in index.calls.keys() {
                assert_eq!(index.receiver(start, end), Some("T"));
            }
            assert_eq!(WORK.with(|work| work.get()), work);
            visits.push(work.0);
        }
        assert!(
            visits[1] < visits[0] * 5,
            "scope indexing must not rescan a function per callback: {visits:?}"
        );
    }
}
