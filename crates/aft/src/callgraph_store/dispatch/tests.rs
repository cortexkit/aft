use super::*;
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
                            shape: "->".into(),
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
                                shape: "->".into(),
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
                        shape: "different-arity".into(),
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
