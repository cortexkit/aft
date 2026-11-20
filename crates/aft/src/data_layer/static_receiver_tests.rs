use std::collections::{BTreeMap, BTreeSet};

use aft_store::content::ContentRecord;

use super::callgraph::FileResolution;
use super::incremental_tests::{checkout as project_checkout, update_checked_bytes};
use super::site_records_tests::records_match_files;
use crate::callgraph_store::join::dispatch::{CSharpProjects, Resolution, Resolver};
use crate::callgraph_store::join::{BlobRefKind, CallgraphBlob, ParseBlob};

#[path = "static_receiver_tests/bindings.rs"]
mod bindings;
#[path = "static_receiver_tests/projects.rs"]
mod projects;
#[path = "static_receiver_tests/visibility.rs"]
mod visibility;

fn checkout(files: &[(&str, &str)], links: &[(&str, &str)]) -> super::callgraph::CheckoutInputs {
    let mut files = files.to_vec();
    if files.iter().any(|(path, _)| path.ends_with(".cs")) {
        files.push(("Fixture.csproj", "<Project />"));
    }
    project_checkout(&files, links)
}

fn round_trip(source: &str, language: &str) -> ParseBlob {
    let extracted = super::content::extract(source.as_bytes(), language).unwrap();
    let (surface, body) = extracted.encode();
    let decoded = ContentRecord::decode(&surface, &body).unwrap();
    assert_eq!(extracted, decoded);
    let CallgraphBlob::Parse(parse) = super::content::blob_from_record(&decoded).unwrap() else {
        panic!("expected {language} parse");
    };
    parse
}

fn retained_static_call<'a>(parse: &'a ParseBlob, receiver: &str, member: &str) -> &'a str {
    let sites: Vec<_> = parse
        .dispatch
        .sites
        .iter()
        .filter(|site| site.member.as_deref() == Some(member))
        .collect();
    assert_eq!(sites.len(), 1, "{:#?}", parse.dispatch.sites);
    let site = sites[0];
    let refs: Vec<_> = parse
        .refs
        .iter()
        .filter(|reference| {
            reference.kind == BlobRefKind::Call && reference.ordinal == site.ordinal
        })
        .collect();
    assert!(
        refs.len() > 1,
        "witness must have multiple call references for one ordinal: {refs:#?}"
    );
    let first = refs[0];
    assert!(
        refs.iter()
            .all(|reference| (reference.byte_start, reference.byte_end)
                == (first.byte_start, first.byte_end)),
        "{refs:#?}"
    );
    let evidence = parse
        .dispatch
        .static_facts
        .as_ref()
        .unwrap()
        .site_evidence(site.ordinal)
        .unwrap();
    assert_eq!(evidence.receiver, receiver);
    site.caller.as_deref().unwrap()
}

#[test]
fn duplicate_static_receiver_ordinals_round_trip_csharp_yield_body() {
    let source = r#"namespace System.Collections.Tests {
    public static class Tuple {
        public static int Create(int x) { return x; }
    }
    public static class TestData {
        public static System.Collections.Generic.IEnumerable<int> GetData() {
            yield return Tuple.Create(1);
        }
    }
}"#;
    let path = "InputData.cs";
    let parse = round_trip(source, "csharp");
    assert!(retained_static_call(&parse, "Tuple", "Create").contains("GetData"));
    let markers = CSharpProjects::project_markers(b"<Project />");
    let projects = CSharpProjects::new([(b"Fixture.csproj".as_slice(), Some(markers.as_slice()))]);
    let resolver =
        Resolver::new(BTreeMap::from([(path.to_string(), &parse)])).with_csharp_projects(&projects);
    let site = parse
        .dispatch
        .sites
        .iter()
        .find(|site| site.member.as_deref() == Some("Create"))
        .unwrap();
    let answer = resolver.resolve(path, site);
    let target = parse
        .symbols
        .iter()
        .find(|symbol| symbol.name == "Create")
        .unwrap();
    exact(&answer, path, &[&target.scoped_name]);
}

#[test]
fn duplicate_static_receiver_ordinals_round_trip_java_field_initializer() {
    let source = r#"package sample;
import org.apache.commons.logging.Log;
import org.apache.commons.logging.LogFactory;
public class SampleReflection {
    private final Log logger = LogFactory.getLog(SampleReflection.class);
    public void performReflection() { logger.info("reflection"); }
}"#;
    let path = "SampleReflection.java";
    let parse = round_trip(source, "java");
    let _ = retained_static_call(&parse, "LogFactory", "getLog");
    let resolver = Resolver::new(BTreeMap::from([(path.to_string(), &parse)]));
    let site = parse
        .dispatch
        .sites
        .iter()
        .find(|site| site.member.as_deref() == Some("getLog"))
        .unwrap();
    external(&resolver.resolve(path, site));
}

#[test]
fn duplicate_static_receiver_ordinals_round_trip_typescript_nested_callback() {
    let source = r#"import path from 'node:path';
function createCheckoutReaders() {
    const root = '/checkout';
    return {
        lf: relativePath => readFileSync(path.join(root, relativePath)),
    };
}"#;
    let file = "detect.ts";
    let parse = round_trip(source, "typescript");
    let _ = retained_static_call(&parse, "path", "join");
    let resolver = Resolver::new(BTreeMap::from([(file.to_string(), &parse)]));
    let site = parse
        .dispatch
        .sites
        .iter()
        .find(|site| site.member.as_deref() == Some("join"))
        .unwrap();
    external(&resolver.resolve(file, site));
}

fn answer<'a>(file: &'a FileResolution, member: &str) -> &'a Resolution {
    let matches = file
        .sites
        .iter()
        .filter(|s| s.site.member.as_deref() == Some(member))
        .collect::<Vec<_>>();
    assert_eq!(matches.len(), 1, "{member}: {matches:?}");
    &matches[0].resolution
}

fn exact(resolution: &Resolution, file: &str, symbols: &[&str]) {
    let expected = symbols
        .iter()
        .map(|symbol| (file.to_string(), symbol.to_string()))
        .collect::<BTreeSet<_>>();
    assert_eq!(resolution.external, 0, "{resolution:?}");
    assert_eq!(resolution.unresolved, 0, "{resolution:?}");
    assert_eq!(resolution.protected, expected);
    assert!(resolution
        .targets
        .iter()
        .all(|target| target.provenance == "exact"));
    assert_eq!(
        resolution
            .targets
            .iter()
            .map(|target| (target.file.clone(), target.symbol.clone()))
            .collect::<BTreeSet<_>>(),
        expected
    );
}

fn external(resolution: &Resolution) {
    assert_eq!(
        resolution,
        &Resolution {
            external: 1,
            ..Resolution::default()
        }
    );
}

#[test]
fn csharp_static_receiver_resolves_qualified_overloads_and_not_open_hierarchies() {
    let inputs = checkout(&[
        ("lib/Tuple.cs", "namespace System {\n  public class Tuple {\n    public static void Create() {}\n    public static void Create(int i) {}\n  }\n}"),
        ("lib/Generic.cs", "namespace System {\n  public class Tuple<T> {\n    public static void Create() {}\n    public static void GenericCreate() {}\n  }\n}"),
        ("other/Tuple.cs", "namespace Other { public class Tuple { public static void Create() {} } }"),
        ("src/User.cs", "using System; using Json = External.JsonSerializer; namespace App; class User { void Use<T>() { Tuple.Create(); Json.Serialize(); System.Tuple<T>.GenericCreate(); } }"),
        ("src/Base.cs", "namespace App; class Base : Outside.FrameworkBase {}"),
        ("src/Collections.cs", "namespace App; interface IList { void Add(); } class Items : IList { public static void Add() {} }"),
        ("src/Open.cs", "namespace App; class Open : Base { void Use() { Items.Add(); } }"),
    ], &[]);
    let resolved = records_match_files(&inputs);
    let create = answer(&resolved["src/User.cs"], "Create");
    assert_eq!(create.targets.len(), 2, "{create:?}");
    let (_, parse) = inputs.parses.get("lib/Tuple.cs").unwrap();
    let symbols = parse
        .symbols
        .iter()
        .filter(|symbol| symbol.name == "Create" && matches!(symbol.start_line, 2 | 3))
        .map(|symbol| symbol.scoped_name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(symbols.len(), 2, "{symbols:?}");
    assert!(
        symbols
            .iter()
            .all(|symbol| symbol.contains("Tuple::Create")),
        "{symbols:?}"
    );
    exact(create, "lib/Tuple.cs", &symbols);
    let generic = answer(&resolved["src/User.cs"], "GenericCreate");
    exact(generic, "lib/Generic.cs", &["System::Tuple::GenericCreate"]);
    external(answer(&resolved["src/User.cs"], "Serialize"));
    let open = answer(&resolved["src/Open.cs"], "Add");
    assert_eq!(open.unresolved, 1, "{open:?}");
    assert!(
        open.protected
            .contains(&("src/Collections.cs".into(), "IList::Add".into())),
        "{open:?}"
    );
}

#[test]
fn java_static_receiver_requires_package_import_or_java_lang_evidence() {
    let inputs = checkout(&[
        ("a/src/test/java/own/User.java", "package own; import java.util.Objects; import external.Api; class User { void use() { Objects.equals(null, null); Api.load(); Math.abs(-1); Widget.create(); } }"),
        ("a/src/main/java/own/Widget.java", "package own; public class Widget extends Parent { private void ignore() {} }"),
        ("a/src/main/java/own/Parent.java", "package own; public class Parent { public static void create() {} }"),
        ("a/src/test/java/own/Other.java", "package unrelated; class Widget { public static void create() {} }"),
        ("other/Names.java", "package other; interface Names { boolean equals(Object o); void load(); int abs(); }"),
    ], &[]);
    let resolved = records_match_files(&inputs);
    let user = &resolved["a/src/test/java/own/User.java"];
    for member in ["equals", "load", "abs"] {
        external(answer(user, member));
    }
    let create = answer(user, "create");
    assert_eq!(create.targets.len(), 1, "{create:?}");
    let target = create.targets.first().unwrap();
    assert!(target.symbol.ends_with("Parent::create"), "{target:?}");
    exact(create, "a/src/main/java/own/Parent.java", &[&target.symbol]);
}

#[test]
fn typescript_static_receiver_preserves_namespace_imports_and_local_shadowing() {
    let inputs = checkout(&[
        ("src/lib.ts", "export class Real { static create() {} static shadow() {} } export function ordinary() {}"),
        ("src/registry.ts", "class Registry { register() {} } class hidden { static conceal() {} } export const registry = new Registry();"),
        ("src/run.ts", "import { Real as Imported } from './lib'; import * as ns from './lib'; import { External } from 'package'; import { registry, hidden } from './registry'; import { Missing } from './missing'; class Local { static run() {} } namespace Local { export class Nested { static make() {} } } interface Names { shadow(): void; check(): void; objectCall(): void; recover(): void; conceal(): void; } function run(Imported) { Imported.shadow(); } Imported.create(); Local.run(); Local.Nested.make(); Imported.helper.objectCall(); ns.ordinary(); External.check(); registry.register(); Missing.recover(); hidden.conceal();"),
    ], &[]);
    let resolved = records_match_files(&inputs);
    let run = &resolved["src/run.ts"];
    exact(answer(run, "create"), "src/lib.ts", &["Real::create"]);
    exact(answer(run, "run"), "src/run.ts", &["Local::run"]);
    external(answer(run, "check"));
    let registry = answer(run, "register");
    assert_eq!(registry.unresolved, 1, "{registry:?}");
    assert!(
        registry
            .protected
            .contains(&("src/registry.ts".into(), "Registry::register".into())),
        "{registry:?}"
    );
    assert_eq!(answer(run, "recover").unresolved, 1);
    assert_eq!(answer(run, "conceal").unresolved, 1);
    let shadowed = answer(run, "shadow");
    assert_eq!(shadowed.unresolved, 1, "{shadowed:?}");
    assert!(
        shadowed
            .protected
            .contains(&("src/lib.ts".into(), "Real::shadow".into())),
        "{shadowed:?}"
    );
    let dotted = answer(run, "make");
    assert_eq!(dotted.targets.len(), 1, "{dotted:?}");
    let target = dotted.targets.first().unwrap();
    assert!(target.symbol.ends_with("Nested::make"), "{target:?}");
    exact(dotted, "src/run.ts", &[&target.symbol]);
    assert_eq!(answer(run, "objectCall").unresolved, 1);
    assert!(run
        .sites
        .iter()
        .all(|s| s.site.member.as_deref() != Some("ordinary")));
    assert!(run
        .rows
        .iter()
        .any(|row| row.target_symbol.as_deref() == Some("ordinary")));
}

#[test]
fn csharp_static_receiver_keeps_unknown_for_values_spelled_like_types() {
    let inputs = checkout(&[
        ("lib/Items.cs", "namespace App; class Items { public static void Add() {} public class Nested { public static void Add() {} } }"),
        ("src/Property.cs", "namespace App; class Parent { protected object Items { get; } } class Property : Parent { void Run() { Items.Add(); } }"),
        ("src/Field.cs", "namespace App; class Field { object Items; void Run() { Items.Nested.Add(); } }"),
        ("src/Parameter.cs", "namespace App; class Parameter { void Run(object Items) { Items.Nested.Add(); } }"),
        ("src/Local.cs", "namespace App; class Local { void Run() { var Items = Read(); Items.Add(); } }"),
        ("src/OutVar.cs", "namespace App; class OutVar { void Run() { Read(out var Items); Items.Add(); } }"),
    ], &[]);
    let resolved = records_match_files(&inputs);
    for path in [
        "src/Property.cs",
        "src/Field.cs",
        "src/Parameter.cs",
        "src/Local.cs",
        "src/OutVar.cs",
    ] {
        let value = answer(&resolved[path], "Add");
        assert_eq!(value.unresolved, 1, "{path}: {value:?}");
        assert!(!value.protected.is_empty(), "{path}: {value:?}");
    }
}

#[test]
fn java_static_receiver_keeps_unknown_for_shadowed_imports() {
    let inputs = checkout(&[
        ("src/Shadow.java", "package pkg; class Field { Object Api; void use() { Api.Inner.run(); } } class Parameter { void use(Object Api) { Api.Inner.call(); } }"),
        ("src/Api.java", "package pkg; public class Api { public static class Inner { public static void run() {} public static void call() {} } }"),
        ("external/Local.java", "package consumer; import external.Api; class Local { void use() { var Api = load(); Api.make(); } }"),
        ("src/Names.java", "package pkg; interface Names { void run(); void make(); void call(); }"),
    ], &[]);
    let resolved = records_match_files(&inputs);
    for (path, member) in [
        ("src/Shadow.java", "run"),
        ("src/Shadow.java", "call"),
        ("external/Local.java", "make"),
    ] {
        let value = answer(&resolved[path], member);
        assert_eq!(value.unresolved, 1, "{path}: {value:?}");
    }
}

#[test]
fn csharp_static_receiver_refresh_matches_cold_on_type_and_hierarchy_changes() {
    let caller = (
        "src/Caller.cs",
        "namespace App; class Caller : Base { void Use() { X.M(); } }",
    );
    let base = ("src/Base.cs", "namespace App; class Base {}");
    let open = (
        "src/Base.cs",
        "namespace App; class Base : External.FrameworkBase {}",
    );
    let x = (
        "library/Types.cs",
        "namespace App; public class X { public static void M() {} }",
    );
    let renamed = (
        "library/Types.cs",
        "namespace App; public class Y { public static void M() {} }",
    );
    let unrelated = (
        "other/Names.cs",
        "namespace Other; interface Names { void M(); }",
    );
    let closed = checkout(&[caller, base, x, unrelated], &[]);
    let (stats, resolved) = update_checked_bytes(
        checkout(&[caller, base, unrelated], &[]),
        closed.clone(),
        &[x.0],
    );
    assert!(stats.sites_resolved > 0, "{stats:?}");
    exact(answer(&resolved[caller.0], "M"), x.0, &["X::M"]);
    for (files, changed) in [
        (vec![caller, base, unrelated], x.0),
        (vec![caller, base, renamed, unrelated], x.0),
        (vec![caller, open, x, unrelated], base.0),
    ] {
        let (_, resolved) = update_checked_bytes(closed.clone(), checkout(&files, &[]), &[changed]);
        let fallback = answer(&resolved[caller.0], "M");
        assert_eq!(fallback.unresolved, 1, "{fallback:?}");
        assert!(
            fallback
                .protected
                .contains(&(unrelated.0.into(), "Names::M".into())),
            "{fallback:?}"
        );
    }
}
