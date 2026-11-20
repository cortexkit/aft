use super::*;

fn check(language: &str, source: &str, case: &str) {
    let parse = round_trip(source, language);
    let sites = parse
        .dispatch
        .sites
        .iter()
        .filter(|site| site.member.as_deref() == Some("probe"))
        .collect::<Vec<_>>();
    assert_eq!(
        sites.len(),
        1,
        "{case}: fixture must have a method site: {:?}",
        parse.dispatch.sites
    );
    let site = sites[0];
    let evidence = parse
        .dispatch
        .static_facts
        .as_ref()
        .and_then(|f| f.site_evidence(site.ordinal));
    assert!(
        evidence.is_none(),
        "{case}: a value became a static receiver: {}",
        serde_json::to_string(&evidence).unwrap()
    );
    // Compare targets and the methods kept live even without an edge.
    let resolver = Resolver::new(BTreeMap::from([("input".into(), &parse)]));
    assert_eq!(
        resolver.resolve("input", site),
        resolver.resolve_nonstatic("input", site),
        "{case}"
    );
}

macro_rules! csharp {
    ($($case:ident: $body:literal,)*) => {$(
        #[test]
        fn $case() {
            let source = format!("namespace App; class Receiver {{ public class Inner {{ public static void probe() {{}} }} }} class Caller {{ void Run() {{ {} }} }}", $body);
            check("csharp", &source, stringify!($case));
        }
    )*};
}

csharp! {
    csharp_implicit_lambda: "Use(Receiver => Receiver.Inner.probe());",
    csharp_typed_lambda: "Use((dynamic Receiver) => Receiver.Inner.probe());",
    csharp_single_underscore_lambda: "Use(_ => _.probe());",
    csharp_lambda_discard_list: "Use((_, _, Receiver) => Receiver.Inner.probe());",
    csharp_local_function_name: "void Receiver() {} Receiver.Inner.probe();",
    csharp_local_function_parameter: "void Local(dynamic Receiver) { Receiver.Inner.probe(); }",
    csharp_out_var: "Get(out var Receiver); Receiver.Inner.probe();",
    csharp_is_pattern: "if (obj is Holder Receiver) Receiver.Inner.probe();",
    csharp_switch_statement_pattern: "switch (obj) { case Holder Receiver: Receiver.Inner.probe(); break; }",
    csharp_switch_expression_pattern: "var z = obj switch { Holder Receiver => Receiver.Inner.probe(), _ => null };",
    csharp_property_pattern: "if (obj is Holder { Item: { } Receiver }) Receiver.Inner.probe();",
    csharp_foreach: "foreach (var Receiver in items) Receiver.Inner.probe();",
    csharp_foreach_deconstruction: "foreach (var (Receiver, _) in items) Receiver.Inner.probe();",
    csharp_catch: "try { Work(); } catch (Problem Receiver) { Receiver.Inner.probe(); }",
    csharp_using_declaration: "using var Receiver = Get(); Receiver.Inner.probe();",
    csharp_using_statement: "using (var Receiver = Get()) Receiver.Inner.probe();",
    csharp_fixed: "fixed (int* Receiver = data) Receiver.Inner.probe();",
    csharp_linq_from: "var z = from Receiver in items select Receiver.Inner.probe();",
    csharp_linq_let: "var z = from item in items let Receiver = item select Receiver.Inner.probe();",
    csharp_linq_join: "var z = from item in items join Receiver in more on item.Key equals Receiver.Key select Receiver.Inner.probe();",
    csharp_linq_join_into: "var z = from item in items join x in more on item.Key equals x.Key into Receiver select Receiver.Inner.probe();",
    csharp_linq_select_into: "var z = from item in items select item into Receiver select Receiver.Inner.probe();",
    csharp_tuple_deconstruction: "var (Receiver, other) = Pair(); Receiver.Inner.probe();",
    csharp_typed_tuple_deconstruction: "(Holder Receiver, int _) = Pair(); Receiver.Inner.probe();",
}

macro_rules! java {
    ($($case:ident: $body:literal,)*) => {$(
        #[test]
        fn $case() {
            let source = format!("package own; class Receiver {{ static class Inner {{ static void probe() {{}} }} }} class Caller {{ void run() {{ {} }} }}", $body);
            check("java", &source, stringify!($case));
        }
    )*};
}

java! {
    java_implicit_lambda: "use(Receiver -> Receiver.Inner.probe());",
    java_inferred_lambda_list: "use((Receiver, other) -> Receiver.Inner.probe());",
    java_typed_lambda: "use((Holder Receiver) -> Receiver.Inner.probe());",
    java_enhanced_for: "for (Holder Receiver : items) Receiver.Inner.probe();",
    java_catch: "try { work(); } catch (Problem Receiver) { Receiver.Inner.probe(); }",
    java_resource: "try (var Receiver = open()) { Receiver.Inner.probe(); }",
    java_instanceof_pattern: "if (obj instanceof Holder Receiver) Receiver.Inner.probe();",
    java_record_pattern: "if (obj instanceof Box(Holder Receiver)) Receiver.Inner.probe();",
    java_switch_pattern: "switch (obj) { case Holder Receiver -> Receiver.Inner.probe(); default -> work(); }",
    java_switch_record_pattern: "switch (obj) { case Box(Holder Receiver) -> Receiver.Inner.probe(); default -> work(); }",
}

macro_rules! js {
    ($($case:ident: $body:literal,)*) => {$(
        #[test]
        fn $case() {
            let source = format!("import {{ Receiver }} from 'outside'; {}", $body);
            for language in ["javascript", "typescript"] {
                check(language, &source, stringify!($case));
            }
        }
    )*};
}

js! {
    js_arrow_bare_parameter: "use(Receiver => Receiver.probe());",
    js_arrow_parenthesized_parameters: "use((Receiver, other) => Receiver.probe());",
    js_arrow_destructured_parameter: "use(({ member: Receiver }) => Receiver.probe());",
    js_arrow_default_parameter: "use((Receiver = get()) => Receiver.probe());",
    js_function_parameter: "function run(Receiver) { Receiver.probe(); }",
    js_function_destructured_parameter: "function run([Receiver], { other }) { Receiver.probe(); }",
    js_function_default_parameter: "function run({ Receiver = get() } = get()) { Receiver.probe(); }",
    js_for_of: "function run(items) { for (const Receiver of items) Receiver.probe(); }",
    js_for_in: "function run(items) { for (let Receiver in items) Receiver.probe(); }",
    js_for_of_destructuring: "function run(items) { for (const [Receiver] of items) Receiver.probe(); }",
    js_catch: "function run() { try { work(); } catch (Receiver) { Receiver.probe(); } }",
    js_catch_destructuring: "function run() { try { work(); } catch ({ Receiver }) { Receiver.probe(); } }",
    js_destructuring_declaration: "function run() { const { field: [Receiver] } = get(); Receiver.probe(); }",
    js_class_field: "class Caller { Receiver = get(); run() { Receiver.probe(); } }",
}
