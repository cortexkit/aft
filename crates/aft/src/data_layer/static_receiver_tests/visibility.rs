use super::*;

fn fallback(resolution: &Resolution, label: &str) {
    assert_eq!(resolution.unresolved, 1, "{label}: {resolution:#?}");
    assert_eq!(resolution.external, 0, "{label}: {resolution:#?}");
    assert!(!resolution.protected.is_empty(), "{label}: {resolution:#?}");
}

#[test]
fn csharp_does_not_bind_unrelated_global_x_or_assert() {
    let inputs = checkout(&[
        ("libraries/Tests.cs", "using Xunit; namespace App; class Test { void Run() { Use(x => x.ToString()); Assert.Equal(1, 1); } }"),
        ("regression/data.cs", "public class x { }"),
        ("smoke/Dataflow.cs", "static class Assert { public static void Equal(int a, int b) {} }"),
        ("libraries/Names.cs", "namespace App; interface Names { string ToString(); void Equal(); }"),
    ], &[]);
    let rows = records_match_files(&inputs);
    let tests = &rows["libraries/Tests.cs"];
    fallback(
        answer(tests, "ToString"),
        "the lambda parameter shadows the type name",
    );
    fallback(
        answer(tests, "Equal"),
        "using Xunit does not see another assembly's Assert",
    );
    assert!(answer(tests, "Equal")
        .targets
        .iter()
        .all(|target| target.provenance != "exact"));
}

#[test]
fn csharp_keeps_fallback_for_unqualified_global_x_in_another_file() {
    let inputs = checkout(
        &[
            (
                "tests/Caller.cs",
                "namespace Unrelated; class Caller { void Run() { x.Call(); } }",
            ),
            (
                "regression/data.cs",
                "public class x { public static void Call() {} }",
            ),
        ],
        &[],
    );
    let rows = records_match_files(&inputs);
    fallback(
        answer(&rows["tests/Caller.cs"], "Call"),
        "global x in another file",
    );
}

#[test]
fn csharp_static_receiver_requires_visible_and_unambiguous_type() {
    let inputs = checkout(&[
        ("src/Project.cs", "using static Types.Outer; using A; using B; namespace App { class InNamespace { void Run() { Global.Local(); Nested.Go(); Widget.M(); Lost.Only(); } } } class Global { public static void Local() {} }"),
        ("lib/Nested.cs", "namespace Types; public static class Outer { public static class Nested { public static void Go() {} } }"),
        ("lib/A.cs", "namespace A; class Widget { public static void M() {} }"),
        ("lib/B.cs", "namespace B; class Widget { public static void M() {} }"),
        ("lib/GlobalUsing.cs", "global using C;"),
        ("lib/Lost.cs", "namespace C; class Lost { public static void Only() {} }"),
    ], &[]);
    let rows = records_match_files(&inputs);
    let caller = &rows["src/Project.cs"];
    for (member, file) in [("Local", "src/Project.cs"), ("Go", "lib/Nested.cs")] {
        let found = answer(caller, member);
        assert_eq!(found.targets.len(), 1, "{member}: {found:?}");
        let symbol = &found.targets.first().unwrap().symbol;
        exact(found, file, &[symbol]);
    }
    fallback(answer(caller, "M"), "ambiguous imported names");
    fallback(answer(caller, "Only"), "cross-file global using");
}

#[test]
fn java_static_receiver_requires_a_shared_module_for_implicit_package_lookup() {
    let inputs = checkout(&[
        ("service/src/testFixtures/java/own/Main.java", "package own; class Main { void run() { Local.a(); Shared.b(); OtherModule.c(); OddLayout.d(); Duplicate.e(); } }"),
        ("service/src/testFixtures/java/own/Local.java", "package own; class Local { static void a() {} }"),
        ("service/src/main/java/own/Shared.java", "package own; class Shared { static void b() {} }"),
        ("unrelated/src/main/java/own/OtherModule.java", "package own; class OtherModule { static void c() {} }"),
        ("service/unusual/own/OddLayout.java", "package own; class OddLayout { static void d() {} }"),
        ("service/src/main/java/own/Duplicate.java", "package own; class Duplicate { static void e() {} }"),
        ("service/src/test/java/own/Duplicate.java", "package own; class Duplicate { static void e() {} }"),
    ], &[]);
    let rows = records_match_files(&inputs);
    let caller = &rows["service/src/testFixtures/java/own/Main.java"];
    exact(
        answer(caller, "a"),
        "service/src/testFixtures/java/own/Local.java",
        &["Local::a"],
    );
    exact(
        answer(caller, "b"),
        "service/src/main/java/own/Shared.java",
        &["Shared::b"],
    );
    for member in ["c", "d", "e"] {
        fallback(answer(caller, member), member);
    }
}

#[test]
fn known_project_type_without_an_indexed_member_keeps_fallback() {
    let inputs = checkout(&[
        ("src/App.cs", "namespace App; class Known {} interface Names { void Call(); } class App { void Run() { Known.Call(); } }"),
        ("src/App.java", "package app; class Known {} interface Names { void call(); } class App { void run() { Known.call(); } }"),
        ("src/app.ts", "class Known {} interface Names { call(): void; } function run() { Known.call(); }"),
    ], &[]);
    let rows = records_match_files(&inputs);
    for (path, member) in [
        ("src/App.cs", "Call"),
        ("src/App.java", "call"),
        ("src/app.ts", "call"),
    ] {
        fallback(answer(&rows[path], member), path);
    }
}
