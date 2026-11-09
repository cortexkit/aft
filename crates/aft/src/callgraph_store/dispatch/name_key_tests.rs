use super::*;
use crate::callgraph_store::method_call_facts::FileFacts;

fn blob(source: &str, language: &str) -> ParseBlob {
    match super::super::CallgraphBlob::extract(source, language, "name-key-tests").unwrap() {
        super::super::CallgraphBlob::Parse(parse) => parse,
        _ => unreachable!(),
    }
}

fn diff(path: &str, old: &ParseBlob, new: &ParseBlob) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    FileFacts::changed_keys_by_method(
        path,
        Some(&FileFacts::new(old)),
        Some(&FileFacts::new(new)),
        &mut keys,
    );
    keys
}

fn covered(path: &str, old: &ParseBlob, new: &ParseBlob, reads: &BTreeSet<String>, key: String) {
    let emitted = diff(path, old, new);
    assert!(reads.contains(&key), "missing read {key}: {reads:?}");
    assert!(
        emitted.contains(&key),
        "missing emission {key}: {emitted:?}"
    );
    assert!(
        !emitted.contains(&dependency::file(path)),
        "precise changes must not use the coarse file key"
    );
    assert!(!emitted.contains(&dependency::directory(path)));
}

#[test]
fn name_keys_types_and_negative_probes_are_per_name_across_languages() {
    for (language, source) in [
        ("rust", "struct A; struct B;"),
        ("typescript", "class A {} class B {}"),
        ("python", "class A: pass\nclass B: pass\n"),
    ] {
        let old = blob(source, language);
        let resolver = Resolver::new(BTreeMap::from([("provider".into(), &old)]));
        assert!(resolver.project_type("provider", "A").is_some());
        let a_reads = resolver.take_dependencies();
        assert!(resolver.project_type("provider", "B").is_some());
        let b_reads = resolver.take_dependencies();
        assert!(resolver.project_type("provider", "Fresh").is_none());
        let negative_reads = resolver.take_dependencies();
        let mut new = old.clone();
        new.dispatch
            .types
            .iter_mut()
            .find(|t| t.name == "A")
            .unwrap()
            .bases
            .push("B".into());
        covered(
            "provider",
            &old,
            &new,
            &a_reads,
            dependency::type_name("provider", "A"),
        );
        assert!(diff("provider", &old, &new).is_disjoint(&b_reads));
        new = old.clone();
        new.dispatch.types.push(TypeHint {
            name: "Fresh".into(),
            bases: Vec::new(),
            interface: false,
            closed: true,
        });
        covered(
            "provider",
            &old,
            &new,
            &negative_reads,
            dependency::type_name("provider", "Fresh"),
        );
        assert!(diff("provider", &old, &new).is_disjoint(&a_reads));
        assert!(diff("provider", &old, &new).is_disjoint(&b_reads));
    }
}

#[test]
fn name_keys_fields_and_returns_do_not_invalidate_other_members() {
    let mut old = blob("struct A; struct B; struct C;", "rust");
    old.dispatch.fields.push(FieldHint {
        owner: "A".into(),
        name: "child".into(),
        ty: "B".into(),
    });
    old.dispatch.returns.push(ReturnHint {
        symbol: "factory".into(),
        ty: "B".into(),
    });
    let resolver = Resolver::new(BTreeMap::from([("provider".into(), &old)]));
    assert_eq!(
        resolver
            .project_type("provider", "@field:A|child")
            .unwrap()
            .1
            .name,
        "B"
    );
    let field_reads = resolver.take_dependencies();
    assert_eq!(
        resolver
            .project_type("provider", "@return:factory")
            .unwrap()
            .1
            .name,
        "B"
    );
    let return_reads = resolver.take_dependencies();
    assert!(resolver
        .project_type("provider", "@field:A|missing")
        .is_none());
    let missing_field_reads = resolver.take_dependencies();
    assert!(resolver
        .project_type("provider", "@return:missing")
        .is_none());
    let missing_return_reads = resolver.take_dependencies();
    let mut new = old.clone();
    new.dispatch.fields[0].ty = "C".into();
    covered(
        "provider",
        &old,
        &new,
        &field_reads,
        dependency::field("provider", "A", "child"),
    );
    assert!(diff("provider", &old, &new).is_disjoint(&return_reads));
    new = old.clone();
    new.dispatch.returns[0].ty = "C".into();
    covered(
        "provider",
        &old,
        &new,
        &return_reads,
        dependency::return_hint("provider", "factory"),
    );
    assert!(diff("provider", &old, &new).is_disjoint(&field_reads));
    new = old.clone();
    new.dispatch.fields.push(FieldHint {
        owner: "A".into(),
        name: "missing".into(),
        ty: "B".into(),
    });
    covered(
        "provider",
        &old,
        &new,
        &missing_field_reads,
        dependency::field("provider", "A", "missing"),
    );
    new = old.clone();
    new.dispatch.returns.push(ReturnHint {
        symbol: "missing".into(),
        ty: "B".into(),
    });
    covered(
        "provider",
        &old,
        &new,
        &missing_return_reads,
        dependency::return_hint("provider", "missing"),
    );
}

#[test]
fn name_keys_typescript_default_and_import_alias_reads() {
    let caller = blob("import Alias from './provider';", "typescript");
    let old = blob("export default class A {}\nexport class B {}", "typescript");
    let mut resolver = Resolver::new(BTreeMap::from([
        ("main.ts".into(), &caller),
        ("provider.ts".into(), &old),
    ]));
    resolver.import_targets.insert(
        ("main.ts".into(), "./provider".into()),
        "provider.ts".into(),
    );
    assert_eq!(
        resolver.project_type("main.ts", "Alias").unwrap().1.name,
        "A"
    );
    let reads = resolver.take_dependencies();
    assert!(reads.contains(&dependency::type_name("main.ts", "Alias")));
    let mut new = old.clone();
    new.default_export_symbol = Some("B".into());
    covered(
        "provider.ts",
        &old,
        &new,
        &reads,
        dependency::type_name("provider.ts", "@default"),
    );
    let new_caller = blob("import { B as Alias } from './provider';", "typescript");
    covered(
        "main.ts",
        &caller,
        &new_caller,
        &reads,
        dependency::type_name("main.ts", "Alias"),
    );
    let mut old_facts = FileFacts::new(&caller);
    old_facts
        .import_targets
        .insert("./provider".into(), "provider.ts".into());
    let mut new_facts = old_facts.clone();
    new_facts
        .import_targets
        .insert("./provider".into(), "other.ts".into());
    let mut keys = BTreeSet::new();
    FileFacts::changed_keys_by_method("main.ts", Some(&old_facts), Some(&new_facts), &mut keys);
    assert_eq!(
        keys,
        BTreeSet::from([dependency::type_name("main.ts", "Alias")])
    );
}

#[test]
fn name_keys_rust_type_targets_record_absence_and_emit_target_changes() {
    let caller = blob("fn caller() {}", "rust");
    let resolver = Resolver::new(BTreeMap::from([("main.rs".into(), &caller)]));
    assert!(resolver.project_type("main.rs", "Alias").is_none());
    let reads = resolver.take_dependencies();
    assert!(reads.contains(&dependency::type_name("main.rs", "Alias")));
    let old = FileFacts::new(&caller);
    let mut new = old.clone();
    new.type_targets
        .insert("Alias".into(), ("provider.rs".into(), "A".into()));
    let mut keys = BTreeSet::new();
    FileFacts::changed_keys_by_method("main.rs", Some(&old), Some(&new), &mut keys);
    assert_eq!(
        keys,
        BTreeSet::from([dependency::type_name("main.rs", "Alias")])
    );
    assert!(!keys.is_disjoint(&reads));
}

#[test]
fn name_keys_python_base_reexports_are_named_reads() {
    let old = blob(
        "from .base import A as Alias\nclass Child(Alias): pass\n",
        "python",
    );
    let base = blob("class A: pass\nclass B: pass\n", "python");
    let resolver = Resolver::new(BTreeMap::from([
        ("pkg/child.py".into(), &old),
        ("pkg/base.py".into(), &base),
    ]));
    assert_eq!(
        resolver
            .python_base_type("pkg/child.py", "Alias")
            .unwrap()
            .1
            .name,
        "A"
    );
    let reads = resolver.take_dependencies();
    let new = blob(
        "from .base import B as Alias\nclass Child(Alias): pass\n",
        "python",
    );
    covered(
        "pkg/child.py",
        &old,
        &new,
        &reads,
        dependency::type_name("pkg/child.py", "Alias"),
    );
    let unrelated = blob(
        "from .base import A as Alias\nfrom .base import B as Other\nclass Child(Alias): pass\n",
        "python",
    );
    assert!(diff("pkg/child.py", &old, &unrelated).is_disjoint(&reads));
}

#[test]
fn name_keys_java_directory_negative_probe_is_scoped() {
    let caller = blob("class Caller {}", "java");
    let old = blob("class Existing {}", "java");
    let resolver = Resolver::new(BTreeMap::from([
        ("pkg/Caller.java".into(), &caller),
        ("pkg/Types.java".into(), &old),
    ]));
    assert!(resolver.project_type("pkg/Caller.java", "Fresh").is_none());
    let reads = resolver.take_dependencies();
    let new = blob("class Existing {} class Fresh {}", "java");
    covered(
        "pkg/Types.java",
        &old,
        &new,
        &reads,
        dependency::directory_type("pkg/Types.java", "Fresh"),
    );
    let unrelated = blob("class Existing {} class Other {}", "java");
    assert!(diff("pkg/Types.java", &old, &unrelated).is_disjoint(&reads));
    let cold = Resolver::new(BTreeMap::from([
        ("pkg/Caller.java".into(), &caller),
        ("pkg/Types.java".into(), &new),
    ]));
    assert_eq!(
        cold.project_type("pkg/Caller.java", "Fresh").unwrap().0,
        "pkg/Types.java"
    );
}

#[test]
fn name_keys_method_groups_keep_owner_type_dependencies_without_file_fanout() {
    let old = blob("class A { run() {} }", "typescript");
    let resolver = Resolver::new(BTreeMap::from([("provider.ts".into(), &old)]));
    let before = resolver.unknown("typescript", "run");
    let reads = resolver.take_dependencies();
    assert!(before.targets.is_empty());
    let mut new = old.clone();
    new.dispatch
        .types
        .iter_mut()
        .find(|t| t.name == "A")
        .unwrap()
        .interface = true;
    covered(
        "provider.ts",
        &old,
        &new,
        &reads,
        dependency::type_name("provider.ts", "A"),
    );
    assert!(!diff("provider.ts", &old, &new).contains(&dependency::method("run")));
    let cold = Resolver::new(BTreeMap::from([("provider.ts".into(), &new)]));
    assert_eq!(cold.unknown("typescript", "run").targets.len(), 1);
    let added = blob("class A { run() {} stop() {} }", "typescript");
    assert!(diff("provider.ts", &old, &added).is_disjoint(&reads));
    let mut changed = old.clone();
    changed.dispatch.methods[0].shape.push_str(" changed");
    covered(
        "provider.ts",
        &old,
        &changed,
        &reads,
        dependency::method("run"),
    );
    resolver.methods_named("absent");
    let missing = resolver.take_dependencies();
    let added = blob("class A { run() {} absent() {} }", "typescript");
    covered(
        "provider.ts",
        &old,
        &added,
        &missing,
        dependency::method("absent"),
    );
}

#[test]
fn name_keys_go_and_file_identity_events_remain_coarse() {
    let old = blob("package p\ntype A struct {}\nfunc (a A) Run() {}", "go");
    let new = blob(
        "package p\ntype A struct {}\nfunc (a A) Run() {}\nfunc unrelated() {}",
        "go",
    );
    // A free function with no return hint may add no resolver facts at all.
    let mut new = new;
    new.dispatch.returns.push(ReturnHint {
        symbol: "unrelated".into(),
        ty: "A".into(),
    });
    let resolver = Resolver::new(BTreeMap::from([("pkg/a.go".into(), &old)]));
    resolver.project_type("pkg/a.go", "A").unwrap();
    let reads = resolver.take_dependencies();
    let keys = diff("pkg/a.go", &old, &new);
    assert!(reads.contains(&dependency::file("pkg/a.go")));
    for key in [
        dependency::file("pkg/a.go"),
        dependency::directory("pkg/a.go"),
        dependency::method("Run"),
        dependency::GO_INTERFACES.to_string(),
    ] {
        assert!(keys.contains(&key), "missing coarse key {key}: {keys:?}");
    }
    let rust = blob("struct A;", "rust");
    for (old, new) in [
        (None, Some(FileFacts::new(&rust))),
        (Some(FileFacts::new(&rust)), None),
        (Some(FileFacts::new(&rust)), Some(FileFacts::new(&old))),
    ] {
        let mut keys = BTreeSet::new();
        FileFacts::changed_keys_by_method("file", old.as_ref(), new.as_ref(), &mut keys);
        assert!(keys.contains(&dependency::file("file")));
        assert!(keys.contains(&dependency::directory("file")));
    }
}

#[test]
#[should_panic(expected = "whole-method-set reads require Go's coarse invalidation")]
fn name_keys_method_group_whole_set_read_rejects_non_go() {
    let parse = blob("class A { run() {} }", "typescript");
    view::MethodGroup::new(&parse).implements("A", &BTreeSet::new());
}

#[test]
fn name_keys_method_group_owner_transition_records_only_the_scoped_key() {
    let old = blob("class A { run() {} }", "typescript");
    let resolver = Resolver::new(BTreeMap::from([("provider.ts".into(), &old)]));
    let group = resolver.methods_named("run");
    let (path, view, method) = &group[0];
    assert_eq!(
        view.owner_type(&resolver, path, &method.owner)
            .unwrap()
            .name,
        "A"
    );
    let reads = resolver.take_dependencies();
    assert_eq!(
        reads,
        BTreeSet::from([
            dependency::method("run"),
            dependency::type_name("provider.ts", "A")
        ])
    );
    let mut new = old.clone();
    new.dispatch.types[0].closed = !new.dispatch.types[0].closed;
    covered(
        "provider.ts",
        &old,
        &new,
        &reads,
        dependency::type_name("provider.ts", "A"),
    );
}

#[test]
fn name_keys_go_global_interface_read_and_group_set_check_remain_coarse() {
    let old = blob(
        "package p\ntype I interface { Run() }\ntype A struct {}\nfunc (a A) Run() {}\n",
        "go",
    );
    let resolver = Resolver::new(BTreeMap::from([("a.go".into(), &old)]));
    let method = old
        .dispatch
        .methods
        .iter()
        .find(|m| m.owner == "A")
        .unwrap();
    assert!(resolver.go_interface_requires("Run", &method.shape));
    let reads = resolver.take_dependencies();
    assert_eq!(
        reads,
        BTreeSet::from([dependency::GO_INTERFACES.to_string()])
    );
    let required = BTreeSet::from([("Run", method.shape.as_str())]);
    assert!(view::MethodGroup::new(&old).implements("A", &required));
    let mut new = old.clone();
    new.dispatch.methods.retain(|m| m.owner != "A");
    assert!(!view::MethodGroup::new(&new).implements("A", &required));
    assert!(!diff("a.go", &old, &new).is_disjoint(&reads));
}
