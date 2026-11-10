use super::*;

#[test]
fn rust_receiver_index_slices_preserve_local_preorder() {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&crate::parser::grammar_for(LangId::Rust))
        .unwrap();
    let mut checked = 0;
    for source in [
        "fn outer() { fn inner() { f(); } inner(); } fn sibling() {}",
        "struct T; impl T { fn new() -> Self { T } fn call(&self) { self.m(); } }",
        "fn before() {} fn unfinished(",
        "fn before() {} fn broken() { let x = ; x.m(); }",
        "fn outer() { fn nested() {} fn unfinished(",
    ] {
        let tree = parser.parse(source, None).unwrap();
        let nodes = descendants(tree.root_node());
        let bindings = RustBindings::new(source, tree.root_node());
        let index = RustReceiverIndex::new(&nodes, source, &bindings, Direct);
        for &function in nodes.iter().filter(|node| FUNCTIONS.contains(&node.kind())) {
            assert_eq!(
                index
                    .function_nodes(function)
                    .unwrap()
                    .iter()
                    .map(|node| node.id())
                    .collect::<Vec<_>>(),
                descendants(function)
                    .iter()
                    .map(|node| node.id())
                    .collect::<Vec<_>>(),
                "subtree order for {source:?}"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 8,
        "complete and incomplete function scopes must be exercised"
    );
}
use crate::callgraph_store::join::CallgraphBlob;

fn parse(source: &str, language: &str) -> ParseBlob {
    match CallgraphBlob::extract(source, language, "ruled-dispatch-1").unwrap() {
        CallgraphBlob::Parse(parse) => parse,
        _ => unreachable!(),
    }
}

#[test]
fn perf_audit2_dispatch_repeated_sites_do_not_rescan_the_project() {
    let parses = (0..128)
        .map(|index| {
            parse(
                &format!("export class C{index} {{ run() {{}} }}"),
                "typescript",
            )
        })
        .collect::<Vec<_>>();
    let resolver = Resolver::new(
        parses
            .iter()
            .enumerate()
            .map(|(index, parse)| (format!("p{index:03}.ts"), parse))
            .collect(),
    );
    let memo = MemoizedResolver::new(&resolver);
    let mut site = SiteHint {
        ordinal: 0,
        caller: Some("caller".into()),
        line: 1,
        member: Some("run".into()),
        receiver: None,
        dynamic: false,
    };
    let unknown = resolver.resolve("p000.ts", &site);
    assert_eq!(unknown.protected.len(), 128);
    site.receiver = Some("C0".into());
    let typed = resolver.resolve("p000.ts", &site);
    assert!(!typed.targets.is_empty());
    resolver.file_visits.set(0);
    for index in 0..96 {
        site.ordinal = index;
        site.line = index + 1;
        site.caller = Some(format!("caller{index}"));
        site.receiver = None;
        assert_eq!(*memo.resolve("p000.ts", &site), unknown);
        site.receiver = Some("C0".into());
        assert_eq!(*memo.resolve("p000.ts", &site), typed);
    }
    let visits = resolver.file_visits.get();
    eprintln!("dispatch file scans: {visits} for 192 sites in 128 files");
    assert!(
        visits <= 384,
        "repeated receiver/member queries must be resolved once: {visits}"
    );
}
fn resolutions(parse: &ParseBlob) -> Vec<Resolution> {
    let resolver = Resolver::new(BTreeMap::from([("fixture".into(), parse)]));
    parse
        .dispatch
        .sites
        .iter()
        .map(|site| resolver.resolve("fixture", site))
        .collect()
}

fn ts_object_target(parse: &ParseBlob, line: u32) -> BTreeSet<String> {
    let resolver = Resolver::new(BTreeMap::from([("fixture".into(), parse)]));
    let site = parse
        .dispatch
        .sites
        .iter()
        .find(|s| s.line == line)
        .unwrap();
    resolver
        .resolve("fixture", site)
        .targets
        .into_iter()
        .map(|t| t.symbol)
        .collect()
}

#[test]
fn typescript_object_wrappers_bind_the_literal_not_a_homonym() {
    let parse = parse("interface Resolver { handler(): void }\nconst actual = ({ handler: () => {} } satisfies Resolver);\nconst other = { handler: () => {} } as Resolver;\nactual.handler();\nother.handler();", "typescript");
    let first = ts_object_target(&parse, 4);
    let second = ts_object_target(&parse, 5);
    assert_eq!(first.len(), 1, "{parse:#?}");
    assert_eq!(second.len(), 1);
    assert!(first.is_disjoint(&second));
    assert!(first
        .iter()
        .chain(&second)
        .all(|s| s.starts_with("@object:")));
}

#[test]
fn typescript_object_definitions_are_graph_only() {
    let source = "function factory() { return { run() { helper(); } }; }\nfunction helper() {}\nregister({ work() {} });\nconst wrapped = { wrappedWork() {} } satisfies Contract;";
    let tree = crate::parser::FileParser::parse_source(
        std::path::Path::new("fixture.ts"),
        source,
        LangId::TypeScript,
    )
    .unwrap();
    let before =
        crate::parser::extract_symbols_from_tree(source, &tree, LangId::TypeScript).unwrap();
    let data = crate::callgraph::build_file_data_from_tree(
        std::path::Path::new("fixture.ts"),
        source,
        LangId::TypeScript,
        &tree,
        &crate::callgraph_store::LineIndex::new(source),
    )
    .unwrap();
    let after =
        crate::parser::extract_symbols_from_tree(source, &tree, LangId::TypeScript).unwrap();
    assert_eq!(before, after);
    let mut names = before
        .iter()
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, ["factory", "helper", "wrapped"]);
    for name in ["run", "work", "wrappedWork"] {
        assert!(
            data.symbol_metadata
                .keys()
                .any(|symbol| symbol.ends_with(&format!("::{name}"))),
            "{name}"
        );
    }
    let run = data
        .symbol_metadata
        .iter()
        .find(|(name, _)| name.ends_with("::run"))
        .unwrap()
        .1;
    assert_eq!(run.signature.as_deref(), Some("run()"));
    assert!(data
        .calls_by_symbol
        .iter()
        .any(|(symbol, calls)| symbol.ends_with("::run")
            && calls.iter().any(|call| call.callee_name == "helper")));
}

#[test]
fn typescript_object_argument_definitions_have_distinct_identities() {
    let parse = parse("register({ run: () => helper() });\nregister({ run() { helper(); } });\n({ run: function() {} }).run();\nfunction helper() {}", "typescript");
    let definitions = parse
        .symbols
        .iter()
        .filter(|s| s.name == "run")
        .collect::<Vec<_>>();
    assert_eq!(definitions.len(), 3, "{parse:#?}");
    assert_eq!(
        definitions
            .iter()
            .map(|s| &s.scoped_name)
            .collect::<HashSet<_>>()
            .len(),
        3
    );
    assert_eq!(ts_object_target(&parse, 3).len(), 1);
    assert_eq!(
        parse
            .refs
            .iter()
            .filter(|r| r.short_name.as_deref() == Some("helper")
                && r.caller_symbol.as_ref().is_some_and(|s| s.contains("run")))
            .count(),
        2
    );
}

#[test]
fn typescript_object_factory_returns_are_lexically_bound() {
    let parse = parse("function make() { return { run: () => {} }; }\nfunction other() { return { run() {} }; }\nfunction caller() {\n const adapter = make();\n adapter.run();\n const nested = () => ({ child: { run: () => {} } });\n nested().child.run();\n}\nfunction shadow(make: unknown) { const adapter = make(); adapter.run(); }", "typescript");
    let target = ts_object_target(&parse, 5);
    assert_eq!(target.len(), 1, "{parse:#?}");
    assert!(target.iter().all(|s| s.starts_with("@object:0:")));
    assert_eq!(ts_object_target(&parse, 7).len(), 1, "{parse:#?}");
    assert!(ts_object_target(&parse, 9).is_empty());
}

#[test]
fn typescript_object_same_named_local_bindings_do_not_share_a_method() {
    let parse = parse("function first() { const value = { run() {} }; value.run(); }\nfunction second() { const value = { run() {} }; value.run(); }", "typescript");
    let first = ts_object_target(&parse, 1);
    let second = ts_object_target(&parse, 2);
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert!(first.is_disjoint(&second));
}

#[test]
fn typescript_object_unique_existing_selectors_are_preserved() {
    let parse = parse(
        "function owner() { const local = { run() {} }; local.run(); }",
        "typescript",
    );
    assert_eq!(
        parse
            .symbols
            .iter()
            .find(|symbol| symbol.name == "run")
            .unwrap()
            .scoped_name,
        "local::run"
    );
    assert_eq!(
        ts_object_target(&parse, 1),
        BTreeSet::from(["local::run".to_string()])
    );
}

#[test]
fn typescript_object_factory_import_uses_the_export_binding() {
    let producer = parse(
        "export function make() { return { run: () => {} }; }",
        "typescript",
    );
    let consumer = parse("import { make as build } from './producer';\nfunction caller() { const adapter = build(); adapter.run(); }", "typescript");
    let mut resolver = Resolver::new(BTreeMap::from([
        ("producer.ts".into(), &producer),
        ("consumer.ts".into(), &consumer),
    ]));
    resolver.type_targets.insert(
        ("consumer.ts".into(), "build".into()),
        ("producer.ts".into(), "make".into()),
    );
    let result = resolver.resolve("consumer.ts", &consumer.dispatch.sites[0]);
    assert_eq!(result.targets.len(), 1, "{consumer:#?}");
    assert!(result
        .targets
        .iter()
        .all(|t| t.file == "producer.ts" && t.provenance == "exact"));
}

#[test]
fn typescript_object_ambiguous_or_mutated_values_do_not_gain_edges() {
    for source in [
        "const value = { run() {}, ...unknown }; value.run();",
        "const value = { run() {}, run: () => {} }; value.run();",
        "const value = { run() {}, [key]: () => {} }; value.run();",
        "let value = { run() {} }; value = unknown; value.run();",
        "const value = { run() {} }; value.run = unknown; value.run();",
        "function make(flag) { if(flag) return {run(){}}; return {run(){}}; } const value = make(); value.run();",
        "function make() { function inner() { return { run() {} }; } } const value = make(); value.run();",
        "async function make() { return { run() {} }; } const value = make(); value.run();",
        "const value = { run() {} }; function caller(value) { value.run(); }",
        "register({ run() {} }); function callback(value) { value.run(); }",
        "const value = { run() {} }; const alias = value; alias.run = unknown; value.run();",
        "import {make} from './producer'; function caller() { const {make} = unknown; make().run(); }",
        "import {make} from './producer'; const callback = make => make().run();",
        "import {make} from './producer'; function caller() { try {} catch(make) { make().run(); } }",
        "const make = function*() { return { run() {} }; }; make().run();",
        "const value = { run: function internal() {} }; value.internal();",
        "const value = { run: () => this.other(), other() {} };",
        "class Target { run() {} } function make(): Target { make.run(); return new Target(); }",
        "const value = { run() {} }; function caller(values) { for(const value of values) { value.run(); } }",
        "import {make} from './producer'; function caller(values) { for(const make of values) { make().run(); } }",
        "import {make} from './producer'; function caller(values) { for(var make of values) {} make().run(); }",
        "import {make} from './producer'; function caller() { make = unknown; make().run(); }",
    ] {
        let parse = parse(source, "typescript");
        let producer = self::parse("export function make() { return { run() {} }; }", "typescript");
        let mut resolver = Resolver::new(BTreeMap::from([("fixture".into(), &parse), ("producer.ts".into(), &producer)]));
        resolver.type_targets.insert(("fixture".into(), "make".into()), ("producer.ts".into(), "make".into()));
        assert!(parse.dispatch.sites.iter().all(|site| resolver.resolve("fixture", site).targets.is_empty()), "{source}\n{parse:#?}");
    }
}

#[test]
fn interface_two_implementations_are_possible_targets() {
    let parse = parse("interface I { m(): void; }\nclass A implements I { m() {} }\nclass B implements I { m() {} }\nfunction caller(x: I) { x.m(); }", "typescript");
    let result = resolutions(&parse);
    assert_eq!(result.len(), 1, "{:#?}", parse.dispatch);
    assert_eq!(result[0].targets.len(), 3, "{:#?}\n{:#?}", parse, result);
    assert_eq!(
        result[0]
            .targets
            .iter()
            .filter(|t| t.provenance == "dispatch")
            .count(),
        2
    );
    assert_eq!(
        result[0]
            .targets
            .iter()
            .filter(|t| t.provenance == "exact")
            .count(),
        1
    );
    assert_eq!(
        result[0].targets,
        BTreeSet::from([
            Target { file: "fixture".into(), symbol: "I::m".into(), provenance: "exact" },
            Target { file: "fixture".into(), symbol: "A::m".into(), provenance: "dispatch" },
            Target { file: "fixture".into(), symbol: "B::m".into(), provenance: "dispatch" },
        ]),
        "a known interface binds its declaration precisely and only its implementations as possible targets"
    );
}

#[test]
fn unknown_arity_and_private_methods_remain_live_without_edges() {
    let parse = parse("class A { m() {} }\nclass B { private m(a: number, b: number) {} n() {} }\nfunction caller(x) { x.m(); }", "typescript");
    let result = resolutions(&parse);
    assert_eq!(result.len(), 1, "{:#?}", parse.dispatch);
    assert!(result[0].targets.is_empty());
    assert_eq!(result[0].unresolved, 1);
    assert_eq!(result[0].protected.len(), 2, "{:#?}", parse.dispatch);
    assert!(result[0].protected.iter().all(|(_, s)| s.ends_with("m")));
}

#[test]
fn unknown_and_dynamic_public_fixture_python_js_ts() {
    for (language, source) in [
        ("typescript", "class A { m() {} }\nclass B { m() {} n() {} }\nfunction caller(x, name) { x.m(); x[name](); }"),
        ("javascript", "class A { m() {} }\nclass B { m() {} n() {} }\nfunction caller(x, name) { x.m(); x[name](); }"),
        ("python", "class A:\n def m(self): pass\nclass B:\n def m(self): pass\n def n(self): pass\ndef caller(x, name):\n x.m()\n getattr(x, name)()\n"),
    ] {
        let parse = parse(source, language);
        let result = resolutions(&parse);
        assert_eq!(result.iter().map(|r| r.unresolved).sum::<usize>(), 1, "{language}: {:#?}", parse.dispatch);
        assert_eq!(result.iter().map(|r| r.dynamic).sum::<usize>(), 1, "{language}: {:#?}", parse.dispatch);
        assert_eq!(result.iter().map(|r| r.external).sum::<usize>(), 0, "{language}: {:#?}", parse.dispatch);
        assert!(result.iter().all(|r| r.targets.is_empty()));
        assert_eq!(result.iter().flat_map(|r| &r.protected).collect::<BTreeSet<_>>().len(), 2);
    }
}

#[test]
fn builtin_type_does_not_link_project_names() {
    let parse = parse(
        "class A { m() {} }\nfunction caller(x: String) { x.m(); }",
        "typescript",
    );
    let result = resolutions(&parse);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].external, 1);
    assert!(result[0].targets.is_empty());
    assert!(result[0].protected.is_empty());
}

#[test]
fn python_unknown_abstract_receivers_stay_live_without_caller_guesses() {
    let parsed = parse("from abc import abstractmethod\nclass Contract:\n @abstractmethod\n def get_items(self): pass\nclass Other:\n def get_items(self): pass\ndef caller(x):\n x.get_items()\n self.underlying.get_items()\n", "python");
    let result = resolutions(&parsed);
    assert_eq!(result.len(), 2);
    for resolution in result {
        assert!(resolution.targets.is_empty(), "{resolution:#?}");
        assert_eq!(resolution.unresolved, 1);
        assert_eq!(resolution.protected.len(), 2);
    }
}

#[test]
fn python_receiver_evidence_is_local_to_the_calling_scope() {
    let parsed = parse("class C:\n def m(self): pass\n def own(self):\n  self.m()\ndef caller(x: C):\n x.m()\n y: C\n y.m()\n z = C()\n def nested():\n  z = external\n z.m()\ndef unknown():\n def nested():\n  hidden = C()\n hidden.m()\n", "python");
    let resolver = Resolver::new(BTreeMap::from([("fixture".into(), &parsed)]));
    assert_eq!(parsed.dispatch.sites.len(), 5);
    for site in &parsed.dispatch.sites {
        let resolution = resolver.resolve("fixture", site);
        if site.caller.as_deref() == Some("unknown") {
            assert!(resolution.targets.is_empty());
            assert_eq!(resolution.unresolved, 1);
        } else {
            assert!(resolution.targets.iter().any(|t| t.symbol == "C::m"));
            assert!(resolution.targets.iter().all(|t| t.provenance == "exact"));
        }
    }
}

#[test]
fn python_super_uses_c3_base_order_without_subtype_fanout() {
    // Zulu precedes Alpha even though lexical sorting would reverse them.
    // Shared owns an initializer, but C3 puts Alpha before that shared ancestor.
    let parsed = parse("class Shared:\n def __init__(self): pass\nclass Zulu(Shared): pass\nclass Alpha(Shared):\n def __init__(self): pass\nclass Child(Zulu, Alpha):\n def __init__(self):\n  super().__init__()\nclass Sibling(Shared):\n def __init__(self): pass\nclass Direct(Alpha, Zulu):\n def __init__(self):\n  super().__init__()\n", "python");
    let result = resolutions(&parsed);
    assert_eq!(result.len(), 2);
    for resolution in result {
        assert_eq!(resolution.targets.len(), 1, "{resolution:#?}");
        let target = resolution.targets.first().unwrap();
        assert_eq!(target.symbol, "Alpha::__init__");
        assert_eq!(target.provenance, "exact");
    }
}

#[test]
fn python_super_unknown_cyclic_and_inconsistent_bases_never_guess() {
    for (source, external) in [
        ("class Known:\n def __init__(self): pass\nclass Child(External, Known):\n def __init__(self):\n  super().__init__()\n", true),
        ("class A(B): pass\nclass B(A):\n def __init__(self):\n  super().__init__()\n", false),
        ("class A: pass\nclass B: pass\nclass X(A, B): pass\nclass Y(B, A): pass\nclass Child(X, Y):\n def __init__(self):\n  super().__init__()\n", false),
    ] {
        let parsed = parse(source, "python");
        let result = resolutions(&parsed);
        assert_eq!(result.len(), 1);
        assert!(result[0].targets.is_empty(), "{:#?}", result[0]);
        assert_eq!(result[0].unresolved, usize::from(!external));
        assert_eq!(result[0].external, usize::from(external));
        if external {
            assert!(result[0].protected.is_empty());
        }
    }
}

#[test]
fn concrete_inherited_and_constructor_receiver_forms() {
    let parse = parse("class A { m() {} }\nclass B extends A { f() { this.m(); super.m(); } }\nclass D extends B { m() {} }\nfunction caller() { const x = new B(); x.m(); }", "typescript");
    let result = resolutions(&parse);
    assert_eq!(result.len(), 3, "{:#?}", parse.dispatch);
    for r in result {
        assert_eq!(r.targets.len(), 2, "{r:#?}\n{:#?}", parse.dispatch);
        assert_eq!(
            r.targets.iter().filter(|t| t.provenance == "exact").count(),
            1
        );
        assert_eq!(
            r.targets
                .iter()
                .filter(|t| t.provenance == "dispatch")
                .count(),
            1
        );
    }
}

#[test]
fn interface_zero_one_two_implementation_cells() {
    for count in 0..=2 {
        let mut source =
            "interface I { m(): void; }\nfunction caller(x: I) { x.m(); }\n".to_string();
        for i in 0..count {
            source.push_str(&format!("class C{i} implements I {{ m() {{}} }}\n"));
        }
        let parsed = parse(&source, "typescript");
        let result = resolutions(&parsed);
        assert_eq!(result[0].targets.len(), count + 1);
        assert_eq!(
            result[0]
                .targets
                .iter()
                .filter(|t| t.provenance == "dispatch")
                .count(),
            count
        );
    }
}

#[test]
fn rust_concrete_trait_default_and_ambiguous_cells() {
    for (source, expected, unknown) in [
        ("struct T; impl T { fn m(&self) {} } fn caller(x: &T) { x.m(); }", 1, false),
        ("trait I { fn m(&self); } struct T; impl I for T { fn m(&self) {} } fn caller(x: &T) { x.m(); }", 1, false),
        ("trait I { fn m(&self) {} } struct T; impl I for T {} fn caller(x: &T) { x.m(); }", 1, false),
        ("trait I { fn m(&self) {} } trait J { fn m(&self) {} } struct T; impl I for T {} impl J for T {} fn caller(x: &T) { x.m(); }", 2, true),
    ] {
        let parsed = parse(source, "rust");
        let result = resolutions(&parsed);
        assert_eq!(result.len(), 1, "{:#?}", parsed.dispatch);
        assert_eq!(result[0].targets.len(), expected, "{:#?}\n{:#?}", parsed.dispatch, parsed.symbols);
        assert_eq!(result[0].unresolved, usize::from(unknown), "{:#?}", parsed.dispatch);
        assert_eq!(result[0].dynamic, 0);
        if unknown {
            // Both trait defaults are visible candidates. Retaining them as
            // name-only evidence is more honest than dropping both targets;
            // neither default can be asserted as the exact callee.
            assert!(result[0].targets.iter().all(|t| t.provenance == "name_match"));
        } else {
            assert!(result[0].targets.iter().all(|t| t.provenance == "exact"));
        }
    }
}

#[test]
fn go_direct_promoted_shadowed_and_ambiguous_cells() {
    for (source, expected, unknown) in [
        ("package p\ntype T struct{}\nfunc (t T) m() {}\nfunc caller(x T) { x.m() }", 1, false),
        ("package p\ntype A struct{}\nfunc (a A) m() {}\ntype T struct{ A }\nfunc caller(x T) { x.m() }", 1, false),
        ("package p\ntype A struct{}\nfunc (a A) m() {}\ntype T struct{ A }\nfunc (t T) m() {}\nfunc caller(x T) { x.m() }", 1, false),
        ("package p\ntype A struct{}\nfunc (a A) m() {}\ntype B struct{}\nfunc (b B) m() {}\ntype T struct{ A; B }\nfunc caller(x T) { x.m() }", 0, true),
    ] {
        let parsed = parse(source, "go");
        let result = resolutions(&parsed);
        assert_eq!(result.len(), 1, "{:#?}", parsed.dispatch);
        assert_eq!(result[0].targets.len(), expected, "{:#?}\n{:#?}", parsed.dispatch, parsed.symbols);
        assert_eq!(result[0].unresolved, usize::from(unknown), "{:#?}", parsed.dispatch);
        assert_eq!(result[0].dynamic, 0);
    }
}

#[test]
fn known_parameter_forms_across_supported_languages() {
    for (language, source) in [
        (
            "python",
            "class C:\n def m(self): pass\ndef caller(x: C):\n x.m()\n",
        ),
        (
            "rust",
            "struct C; impl C { fn m(&self) {} } fn caller(x: &mut C) { x.m(); }",
        ),
        (
            "go",
            "package p\ntype C struct{}\nfunc (c C) m() {}\nfunc caller(x C) { x.m() }",
        ),
        (
            "java",
            "class C { void m() {} void caller(C x) { x.m(); } }",
        ),
        (
            "csharp",
            "class C { void m() {} void caller(C x) { x.m(); } }",
        ),
        (
            "kotlin",
            "class C {\n fun m() {}\n fun caller(x: C) { x.m() }\n}",
        ),
    ] {
        let parsed = parse(source, language);
        let result = resolutions(&parsed);
        assert_eq!(result.len(), 1, "{language}: {:#?}", parsed.dispatch);
        assert_eq!(
            result[0].targets.len(),
            1,
            "{language}: {:#?}",
            parsed.dispatch
        );
        assert_eq!(result[0].targets.iter().next().unwrap().provenance, "exact");
        assert_eq!(result[0].dynamic, 0);
    }
}

#[test]
fn receiver_form_extraction_table() {
    for (language, source, expected_sites) in [
        ("typescript", "class C { m() {} f(x: C) { this.m(); x.m(); let y: C; y.m(); const z = new C(); z.m(); let w = new C(); w.m(); } }", 5),
        ("javascript", "class C { m() {} f() { this.m(); const x = new C(); x.m(); let y = new C(); y.m(); } }", 3),
        ("python", "class C:\n def m(self): pass\n def f(self, x: C):\n  self.m()\n  x.m()\n  y: C\n  y.m()\n  z = C()\n  z.m()\n @classmethod\n def g(cls):\n  cls.m()\n", 5),
        ("rust", "struct C; impl C { fn m(&self) {} fn new() -> Self { C } fn f(&self, x: C, y: &C, z: &mut C) { self.m(); x.m(); y.m(); z.m(); let a: C = C; a.m(); let b = C::new(); b.m(); let c = C {}; c.m(); } }", 7),
        ("go", "package p\ntype C struct{}\nfunc (c C) m() {}\nfunc (c C) f(x C) { c.m(); x.m(); var y C; y.m(); z := C{}; z.m(); w := &C{}; w.m() }", 5),
        ("java", "class C { C field; void m() {} void f(C x) { this.m(); x.m(); C y = new C(); y.m(); field.m(); var z = new C(); z.m(); } }", 5),
        ("csharp", "class C { C field; void m() {} void f(C x) { this.m(); x.m(); C y = new C(); y.m(); field.m(); var z = new C(); z.m(); } }", 5),
        ("kotlin", "class C {\n fun m() {}\n fun f(x: C) {\n this.m()\n x.m()\n val y: C = x\n y.m()\n }\n}", 3),
    ] {
        let parsed = parse(source, language);
        let sites = parsed.dispatch.sites.iter().filter(|s| s.member.as_deref() == Some("m")).collect::<Vec<_>>();
        assert_eq!(sites.len(), expected_sites, "{language}: {:#?}", parsed.dispatch);
        for site in sites {
            assert_eq!(site.receiver.as_deref(), Some("C"), "{language}: {site:#?}");
            let resolver = Resolver::new(BTreeMap::from([("fixture".into(), &parsed)]));
            let resolved = resolver.resolve("fixture", site);
            assert_eq!(resolved.targets.len(), 1, "{language}: {resolved:#?}");
            assert_eq!(resolved.targets.iter().next().unwrap().provenance, "exact");
        }
    }
}

#[test]
fn unknown_zero_one_two_overload_private_and_language_exclusions() {
    for language in [
        "typescript",
        "javascript",
        "python",
        "rust",
        "go",
        "java",
        "csharp",
        "kotlin",
        "cpp",
    ] {
        // Unknown receivers keep same-language project methods live by written
        // name, regardless of signature. Receiver syntax is tested separately.
        let mut parsed = parse("class A { m() {} }\nclass B { private m(a: number): void; private m(a: number) {} n() {} }\nfunction caller(x) { x.m(); }", "typescript");
        parsed.language = language.into();
        let mut other_language = parsed.clone();
        other_language.language = "not-the-callsite-language".into();
        let resolver = Resolver::new(BTreeMap::from([
            ("fixture".into(), &parsed),
            ("other-language".into(), &other_language),
        ]));
        let site = parsed
            .dispatch
            .sites
            .iter()
            .find(|s| s.member.as_deref() == Some("m"))
            .unwrap();
        let result = resolver.resolve("fixture", site);
        assert_eq!(
            result.protected.len(),
            3,
            "{language}: {:#?}",
            parsed.dispatch
        );
        assert_eq!(result.unresolved, 1);
        assert!(result.targets.is_empty());
        assert!(result.protected.iter().all(|(f, _)| f == "fixture"));
        let mut absent = site.clone();
        absent.member = Some("absent".into());
        let result = resolver.resolve("fixture", &absent);
        assert_eq!(result.external, 1);
        assert!(result.protected.is_empty());
        assert_eq!(result.dynamic, 0);
    }
}

#[test]
fn trait_and_go_interface_fanout_and_rust_generic_bound_forms() {
    for source in [
        "trait I { fn m(&self); } struct A; struct B; impl I for A { fn m(&self) {} } impl I for B { fn m(&self) {} } fn caller(x: &dyn I) { x.m(); }",
        "trait I { fn m(&self); } struct A; struct B; impl I for A { fn m(&self) {} } impl I for B { fn m(&self) {} } fn caller(x: impl I) { x.m(); }",
        "trait I { fn m(&self); } struct A; struct B; impl I for A { fn m(&self) {} } impl I for B { fn m(&self) {} } fn caller<T: I>(x: T) { x.m(); }",
    ] {
        let parsed = parse(source, "rust");
        let result = resolutions(&parsed);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].targets.len(), 3, "{:#?}", parsed.dispatch);
        assert_eq!(result[0].targets.iter().filter(|t| t.provenance == "dispatch").count(), 2);
        assert_eq!(result[0].targets.iter().filter(|t| t.provenance == "exact").count(), 1);
    }
    let parsed = parse("package p\ntype I interface { m() }\ntype A struct{}\nfunc (a A) m() {}\ntype B struct{}\nfunc (b B) m() {}\nfunc caller(x I) { x.m() }", "go");
    let result = resolutions(&parsed);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].targets.len(), 3, "{:#?}", parsed.dispatch);
    assert_eq!(
        result[0]
            .targets
            .iter()
            .filter(|t| t.provenance == "dispatch")
            .count(),
        2
    );
}

#[test]
fn unsupported_and_reassigned_receivers_are_unknown() {
    let parsed = parse("class C { m() {} }\nfunction factory() { return new C(); }\nfunction caller() { let x = new C(); x = factory(); x.m(); const y = factory(); y.m(); }", "typescript");
    let result = resolutions(&parsed);
    assert_eq!(result.len(), 2);
    for r in result {
        assert!(r.targets.is_empty());
        assert_eq!(r.unresolved, 1);
        assert_eq!(r.protected.len(), 1);
    }
}

#[test]
fn every_receiver_form_crosses_target_and_unknown_columns() {
    let forms = [
        ("typescript", "class C { m() {} f(x: C) { this.m(); x.m(); let y: C; y.m(); const z = new C(); z.m(); let w = new C(); w.m(); } }"),
        ("javascript", "class C { m() {} f() { this.m(); const x = new C(); x.m(); let y = new C(); y.m(); } }"),
        ("python", "class C:\n def m(self): pass\n def f(self, x: C):\n  self.m()\n  x.m()\n  y: C\n  y.m()\n  z = C()\n  z.m()\n @classmethod\n def g(cls):\n  cls.m()\n"),
        ("rust", "struct C; impl C { fn m(&self) {} fn new() -> Self { C } fn f(&self, x: C, y: &C, z: &mut C) { self.m(); x.m(); y.m(); z.m(); let a: C = C; a.m(); let b = C::new(); b.m(); let c = C {}; c.m(); } }"),
        ("go", "package p\ntype C struct{}\nfunc (c C) m() {}\nfunc (c C) f(x C) { c.m(); x.m(); var y C; y.m(); z := C{}; z.m(); w := &C{}; w.m() }"),
        ("java", "class C { C field; void m() {} void f(C x) { this.m(); x.m(); C y = new C(); y.m(); field.m(); var z = new C(); z.m(); } }"),
        ("csharp", "class C { C field; void m() {} void f(C x) { this.m(); x.m(); C y = new C(); y.m(); field.m(); var z = new C(); z.m(); } }"),
        ("kotlin", "class C {\n fun m() {}\n fun f(x: C) {\n this.m()\n x.m()\n val y: C = x\n y.m()\n }\n}"),
    ];
    for (language, source) in forms {
        let extracted = parse(source, language);
        for site in extracted
            .dispatch
            .sites
            .iter()
            .filter(|s| s.member.as_deref() == Some("m"))
        {
            for inherited in [false, true] {
                for interface in [false, true] {
                    for overrides in 0..=2 {
                        let mut fixture = extracted.clone();
                        let declaration_owner = if inherited { "Base" } else { "C" };
                        fixture.dispatch.types = vec![TypeHint {
                            name: "C".into(),
                            bases: if inherited {
                                vec!["Base".into()]
                            } else {
                                Vec::new()
                            },
                            interface,
                            closed: false,
                        }];
                        if inherited {
                            fixture.dispatch.types.push(TypeHint {
                                name: "Base".into(),
                                bases: Vec::new(),
                                interface,
                                closed: false,
                            });
                        }
                        fixture.dispatch.methods = vec![MethodHint {
                            owner: declaration_owner.into(),
                            trait_name: None,
                            name: "m".into(),
                            symbol: format!("{declaration_owner}::m"),
                            has_body: !interface,
                            shape: if language == "rust" { "self;->" } else { "->" }.into(),
                            private: false,
                        }];
                        for i in 0..overrides {
                            fixture.dispatch.types.push(TypeHint {
                                name: format!("D{i}"),
                                bases: vec!["C".into()],
                                interface: false,
                                closed: false,
                            });
                            fixture.dispatch.methods.push(MethodHint {
                                owner: format!("D{i}"),
                                trait_name: if language == "rust" && interface {
                                    Some("C".into())
                                } else {
                                    None
                                },
                                name: "m".into(),
                                symbol: format!("D{i}::m"),
                                has_body: true,
                                shape: if language == "rust" { "self;->" } else { "->" }.into(),
                                private: false,
                            });
                        }
                        let resolver =
                            Resolver::new(BTreeMap::from([("fixture".into(), &fixture)]));
                        let resolution = resolver.resolve("fixture", site);
                        let fanout = if interface || !["rust", "go"].contains(&language) {
                            overrides
                        } else {
                            0
                        };
                        assert_eq!(resolution.targets.len(), 1 + fanout, "{language} {site:?} inherited={inherited} interface={interface} overrides={overrides}");
                        assert_eq!(
                            resolution
                                .targets
                                .iter()
                                .filter(|t| t.provenance == "exact")
                                .count(),
                            1
                        );
                        assert_eq!(
                            resolution
                                .targets
                                .iter()
                                .filter(|t| t.provenance == "dispatch")
                                .count(),
                            fanout
                        );
                        assert_eq!(
                            resolution
                                .targets
                                .iter()
                                .map(|t| (&t.file, &t.symbol))
                                .collect::<BTreeSet<_>>()
                                .len(),
                            1 + fanout,
                            "every possible target is a distinct live target"
                        );
                        if !interface {
                            fixture.dispatch.types[0].closed = true;
                            let resolver =
                                Resolver::new(BTreeMap::from([("fixture".into(), &fixture)]));
                            assert_eq!(
                                resolver.resolve("fixture", site).targets.len(),
                                1,
                                "final/sealed does not fan out"
                            );
                        }
                    }
                }
            }
            for candidates in 0..=2 {
                let mut fixture = extracted.clone();
                fixture.dispatch.methods.retain(|m| m.name != "m");
                for i in 0..candidates {
                    fixture.dispatch.methods.push(MethodHint {
                        owner: format!("Unrelated{i}"),
                        trait_name: None,
                        name: "m".into(),
                        symbol: format!("Unrelated{i}::m"),
                        has_body: true,
                        shape: if language == "rust" {
                            "self;different-arity"
                        } else {
                            "different-arity"
                        }
                        .into(),
                        private: true,
                    });
                }
                let resolver = Resolver::new(BTreeMap::from([("fixture".into(), &fixture)]));
                let mut unknown = site.clone();
                unknown.receiver = None;
                let resolution = resolver.resolve("fixture", &unknown);
                assert!(resolution.targets.is_empty());
                assert_eq!(resolution.protected.len(), candidates);
                assert_eq!(resolution.unresolved, usize::from(candidates > 0));
                assert_eq!(resolution.external, usize::from(candidates == 0));
                let mut external = site.clone();
                external.receiver = Some("LibraryType".into());
                let resolution = resolver.resolve("fixture", &external);
                assert!(resolution.targets.is_empty());
                assert!(resolution.protected.is_empty());
                assert_eq!(resolution.external, 1);
            }
        }
    }
}

#[test]
fn relative_import_and_open_receiver_dispatch_do_not_rescan_every_file() {
    let target = parse("export class Target { run() {} }", "typescript");
    let caller = parse(
        "import { Target } from './target';\nfunction caller(x: Target) { x.run(); }",
        "typescript",
    );
    let fillers = (0..128)
        .map(|index| {
            parse(
                &format!("export class F{index} {{ other() {{}} }}"),
                "typescript",
            )
        })
        .collect::<Vec<_>>();
    let mut files = BTreeMap::new();
    files.insert("src/caller.ts".to_string(), &caller);
    files.insert("src/target.ts".to_string(), &target);
    for (index, filler) in fillers.iter().enumerate() {
        files.insert(format!("lib/f{index:03}.ts"), filler);
    }
    let resolver = Resolver::new(files);
    let sites = caller.dispatch.sites.clone();
    assert!(!sites.is_empty(), "{:#?}", caller.dispatch);
    resolver.file_visits.set(0);
    for _ in 0..16 {
        for site in &sites {
            let resolution = resolver.resolve("src/caller.ts", site);
            assert!(
                resolution
                    .targets
                    .iter()
                    .any(|target| target.file == "src/target.ts"),
                "{resolution:#?}"
            );
        }
    }
    // The relative import is found by probing its few candidate names, and
    // same-named methods come from an index built once: neither scales with
    // sites times files any more.
    let visits = resolver.file_visits.get();
    assert!(
        visits <= 130 + 16 * sites.len() * 7,
        "{visits} file visits for {} sites in 130 files",
        16 * sites.len()
    );
}

#[test]
fn mixed_old_and_new_rust_shapes_do_not_guess_instance_binding() {
    let mut old = parse("pub struct Old; impl Old { pub fn m(&self) {} }", "rust");
    for method in &mut old.dispatch.methods {
        method.shape = method
            .shape
            .strip_prefix("self;")
            .or_else(|| method.shape.strip_prefix("static;"))
            .unwrap_or(&method.shape)
            .to_string();
    }
    let encoded = super::super::CallgraphBlob::Parse(old).to_bytes().unwrap();
    let decoded = super::super::CallgraphBlob::from_bytes(&encoded).unwrap();
    let old = decoded.parse().unwrap();
    let fresh = parse("use crate::old::Old; pub struct Fresh; impl Fresh { pub fn m(&self) {} } fn caller(old: &Old, fresh: &Fresh) { old.m(); fresh.m(); }", "rust");
    let mut resolver = Resolver::new(BTreeMap::from([
        ("old.rs".to_string(), old),
        ("new.rs".to_string(), &fresh),
    ]));
    resolver.type_targets.insert(
        ("new.rs".into(), "Old".into()),
        ("old.rs".into(), "Old".into()),
    );
    let old_site = fresh
        .dispatch
        .sites
        .iter()
        .find(|site| site.receiver.as_deref() == Some("Old"))
        .unwrap();
    let fresh_site = fresh
        .dispatch
        .sites
        .iter()
        .find(|site| site.receiver.as_deref() == Some("Fresh"))
        .unwrap();
    let uncertain = resolver.resolve("new.rs", old_site);
    assert!(uncertain.targets.is_empty(), "{uncertain:#?}");
    assert_eq!(uncertain.unresolved, 1);
    let exact = resolver.resolve("new.rs", fresh_site);
    assert_eq!(exact.targets.len(), 1, "{exact:#?}");
    assert!(exact.targets.iter().all(|target| target.file == "new.rs"
        && target.symbol == "Fresh::m"
        && target.provenance == "exact"));
}

#[test]
fn rust_colliding_inline_members_retain_positional_declaration_facts() {
    let parsed = parse(
        "mod nested { pub struct T; impl T { fn f(&self) {} fn f(&self) {} } }",
        "rust",
    );
    assert_eq!(
        parsed
            .symbols
            .iter()
            .filter(|symbol| symbol.name == "f")
            .count(),
        2
    );
    let methods = parsed
        .dispatch
        .methods
        .iter()
        .filter(|method| method.name == "f")
        .collect::<Vec<_>>();
    assert!(
        !methods.is_empty(),
        "duplicate binding names must not erase declaration facts"
    );
    assert!(methods.iter().all(|method| method.owner == "nested::T"));
}

/// Functions a `macro_rules!` template stamps into an impl call each other
/// through `self`. The template is tokens in the file's tree, so the call
/// has no call node; its site names the template's own function, and the
/// resolver links it by that name.
#[test]
fn rust_template_self_call_links_the_function_its_template_defines() {
    let parsed = parse(
        "macro_rules! runtime_methods {\n    () => {\n        async fn poll_session(&self, id: &str) {\n            if self.handle_refusal(id).await {\n                return;\n            }\n        }\n\n        async fn handle_refusal(&self, id: &str) -> bool {\n            id.is_empty()\n        }\n    };\n}\n\npub struct Runtime;\n\nimpl Runtime {\n    runtime_methods!();\n}\n",
        "rust",
    );
    let sites = parsed
        .dispatch
        .sites
        .iter()
        .filter(|site| site.member.as_deref() == Some("handle_refusal"))
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 1, "{:#?}", parsed.dispatch.sites);
    assert_eq!(sites[0].caller.as_deref(), Some("poll_session"));
    assert_eq!(sites[0].line, 4);
    let resolver = Resolver::new(BTreeMap::from([("fixture".into(), &parsed)]));
    assert_eq!(
        resolver.resolve("fixture", sites[0]).targets,
        BTreeSet::from([Target {
            file: "fixture".into(),
            symbol: "handle_refusal".into(),
            provenance: "name_match",
        }])
    );
    // `id.is_empty()` has a receiver the template does not define: no site.
    assert!(
        parsed
            .dispatch
            .sites
            .iter()
            .all(|site| site.member.as_deref() != Some("is_empty")),
        "{:#?}",
        parsed.dispatch.sites
    );
}

/// A `self` call to a name the template does not define is left to
/// ordinary resolution.
#[test]
fn rust_template_self_call_to_a_name_the_template_lacks_has_no_site() {
    let parsed = parse(
        "macro_rules! m { () => { fn a(&self) { self.missing(); } }; }\nfn missing() {}\n",
        "rust",
    );
    assert!(
        parsed.dispatch.sites.iter().all(|site| !site
            .receiver
            .as_deref()
            .is_some_and(|receiver| receiver.starts_with(TEMPLATE_SELF_RECEIVER))),
        "{:#?}",
        parsed.dispatch.sites
    );
}

/// `obj["name"]()` calls `name` as written and is left to ordinary
/// resolution; only an index that is not a string literal is dynamic.
#[test]
fn computed_member_with_a_string_literal_is_not_a_dynamic_site() {
    for language in ["typescript", "javascript"] {
        let parsed = parse(
            "function method() {}\nfunction caller(obj, name) { obj[\"method\"](); obj['method'](); obj[name](); }\n",
            language,
        );
        let sites = &parsed.dispatch.sites;
        assert_eq!(sites.len(), 1, "{language}: {sites:#?}");
        assert!(sites[0].dynamic, "{language}: {sites:#?}");
    }
}

/// A C++ member call on a local whose declaration names its type links the
/// one member function of that type, by its scoped name, as a type match.
#[test]
fn cpp_member_call_on_a_declared_local_type_matches_its_member_function() {
    let parsed = parse(
        "namespace api {\nclass Widget {\npublic:\n    int value() { return 42; }\n};\n}\ninline int use_widget() { api::Widget widget; return widget.value(); }\ninline int unknown(api::Widget* pointer, int other) { return other.value(); }\n",
        "cpp",
    );
    assert!(
        parsed
            .dispatch
            .methods
            .iter()
            .any(|method| method.symbol == "api::Widget::value" && method.owner == "api::Widget"),
        "{:#?}",
        parsed.dispatch.methods
    );
    let sites = parsed
        .dispatch
        .sites
        .iter()
        .filter(|site| site.member.as_deref() == Some("value"))
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 2, "{:#?}", parsed.dispatch.sites);
    let resolver = Resolver::new(BTreeMap::from([("fixture".into(), &parsed)]));
    let typed = sites
        .iter()
        .find(|site| site.caller.as_deref() == Some("use_widget"))
        .unwrap();
    assert_eq!(typed.receiver.as_deref(), Some("Widget"));
    assert_eq!(
        resolver.resolve("fixture", typed).targets,
        BTreeSet::from([Target {
            file: "fixture".into(),
            symbol: "api::Widget::value".into(),
            provenance: "type_match",
        }])
    );
    // `int other` is a type the project does not define a `value` for.
    let other = sites
        .iter()
        .find(|site| site.caller.as_deref() == Some("unknown"))
        .unwrap();
    assert_eq!(other.receiver.as_deref(), Some("int"));
    let resolution = resolver.resolve("fixture", other);
    assert!(resolution.targets.is_empty());
    assert_eq!(resolution.external, 1);
}
