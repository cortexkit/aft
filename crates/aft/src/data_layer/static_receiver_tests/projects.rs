use super::*;

const CALLER: (&str, &str) = (
    "app/src/Caller.cs",
    "using Xunit; using TestAssert = Xunit.Assert; namespace App; class Caller { void Run() { Assert.Check(); TestAssert.AliasCheck(); } }",
);
const ASSERT: &str = "namespace Xunit; public static class Assert { public static void Check() {} public static void AliasCheck() {} }";
const APP: (&str, &str) = ("app/App.csproj", "<Project />");
const TYPE: (&str, &str) = ("app/helpers/Assert.cs", ASSERT);

fn check_unknown(file: &FileResolution) {
    for member in ["Check", "AliasCheck"] {
        let result = answer(file, member);
        assert_eq!(result.unresolved, 1, "{member}: {result:#?}");
        assert_eq!(result.external, 0, "{member}: {result:#?}");
        assert!(!result.protected.is_empty(), "{member}: {result:#?}");
        assert!(result
            .targets
            .iter()
            .all(|target| target.provenance != "exact"));
    }
}

#[test]
fn csharp_namespace_or_alias_cannot_bind_a_type_in_another_project() {
    let inputs = project_checkout(
        &[
            CALLER,
            APP,
            ("tests/Tests.csproj", "<Project />"),
            ("tests/src/Assert.cs", ASSERT),
        ],
        &[],
    );
    check_unknown(&records_match_files(&inputs)[CALLER.0]);
}

fn check_exact(file: &FileResolution) {
    for member in ["Check", "AliasCheck"] {
        let symbol = format!("Assert::{member}");
        exact(answer(file, member), TYPE.0, &[symbol.as_str()]);
    }
}

#[test]
fn csharp_project_lookup_keeps_same_project_types_in_other_folders() {
    let inputs = project_checkout(
        &[
            CALLER,
            APP,
            TYPE,
            ("unrelated/Test.csproj", "<Project />"),
            ("unrelated/Assert.cs", ASSERT),
        ],
        &[],
    );
    check_exact(&records_match_files(&inputs)[CALLER.0]);
}

#[test]
fn csharp_nearest_folder_with_two_projects_is_not_a_project() {
    let files = [
        CALLER,
        APP,
        TYPE,
        ("app/src/First.csproj", "<Project />"),
        ("app/src/Second.csproj", "<Project />"),
    ];
    let inputs = project_checkout(&files, &[]);
    check_unknown(&records_match_files(&inputs)[CALLER.0]);
}

#[test]
fn csharp_file_without_a_project_keeps_fallback() {
    let inputs = project_checkout(&[CALLER, TYPE], &[]);
    check_unknown(&records_match_files(&inputs)[CALLER.0]);
}

#[test]
fn csharp_project_file_add_remove_move_refresh_matches_cold_bytes() {
    let src_project = ("app/src/Nested.csproj", "<Project />");
    let target_project = ("app/helpers/Nested.csproj", "<Project />");
    let fixture = |projects: &[(&str, &str)]| {
        let mut files = vec![CALLER, TYPE];
        files.extend_from_slice(projects);
        project_checkout(&files, &[])
    };
    let unknown = fixture(&[]);
    let single = fixture(&[APP]);
    let split = fixture(&[APP, target_project]);
    let moved = fixture(&[APP, src_project]);
    for (before, after, changed, is_exact) in [
        (unknown.clone(), single.clone(), vec![APP.0], true),
        (single.clone(), split.clone(), vec![target_project.0], false),
        (
            split,
            moved.clone(),
            vec![src_project.0, target_project.0],
            false,
        ),
        (moved, single.clone(), vec![src_project.0], true),
        (single, unknown, vec![APP.0], false),
    ] {
        let (stats, rows) = update_checked_bytes(before, after, &changed);
        assert!(stats.sites_resolved > 0, "{changed:?}: {stats:?}");
        if is_exact {
            check_exact(&rows[CALLER.0]);
        } else {
            check_unknown(&rows[CALLER.0]);
        }
    }
}

#[test]
fn csharp_unproven_alias_in_a_checkout_namespace_is_not_external() {
    let files = [
        ("app/App.csproj", "<Project />"),
        ("app/Caller.cs", "using Debug = Startup.CompilerHelpers.StartupDebug; using Remote = NoCheckoutPackage.Api; namespace App; class Caller { void Run() { Debug.Assert(); Remote.Convert(); } }"),
        ("shared/Namespace.cs", "namespace Startup.CompilerHelpers { }"),
        ("other/Names.cs", "namespace Other; interface Names { void Assert(); void Convert(); }"),
    ];
    let rows = records_match_files(&project_checkout(&files, &[]));
    let caller = &rows["app/Caller.cs"];
    let startup = answer(caller, "Assert");
    assert_eq!(
        (startup.unresolved, startup.external),
        (1, 0),
        "{startup:?}"
    );
    assert!(!startup.protected.is_empty());
    external(answer(caller, "Convert"));
}

#[test]
fn java_unindexed_import_in_a_checkout_package_is_not_external() {
    let files = [
        (
            "app/Caller.java",
            "package app; import helpers.Missing; class Caller { void run() { Missing.check(); } }",
        ),
        ("src/helpers/Package.java", "package helpers;"),
        (
            "src/Names.java",
            "package misc; interface Names { void check(); }",
        ),
    ];
    let rows = records_match_files(&project_checkout(&files, &[]));
    let result = answer(&rows["app/Caller.java"], "check");
    assert_eq!((result.unresolved, result.external), (1, 0), "{result:?}");
    let (stats, rows) = update_checked_bytes(
        project_checkout(&[files[0], files[2]], &[]),
        project_checkout(&files, &[]),
        &[files[1].0],
    );
    assert!(stats.sites_resolved > 0, "{stats:?}");
    assert_eq!(answer(&rows["app/Caller.java"], "check"), result);
}

#[test]
fn csharp_project_lookup_skips_aggregators_but_not_unknown_compilers() {
    for aggregator in [
        r#"<Project Sdk="Microsoft.Build.NoTargets" />"#,
        r#"<Project><Sdk Name="Microsoft.Build.Traversal" Version="1.0.0" /></Project>"#,
    ] {
        let files = [CALLER, APP, TYPE, ("app/src/Aggregator.csproj", aggregator)];
        check_exact(&records_match_files(&project_checkout(&files, &[]))[CALLER.0]);
    }
    for unsupported in [
        "<Project><PropertyGroup><EnableDefaultCompileItems>false</EnableDefaultCompileItems></PropertyGroup></Project>",
        "<Project><Unclosed></Project>",
    ] {
        let files = [CALLER, APP, TYPE, ("app/src/Nested.csproj", unsupported)];
        check_unknown(&records_match_files(&project_checkout(&files, &[]))[CALLER.0]);
    }
}

#[test]
fn csharp_project_marker_edit_refresh_matches_cold_bytes() {
    let nested = "app/src/Nested.csproj";
    let before = project_checkout(
        &[
            CALLER,
            APP,
            TYPE,
            (nested, r#"<Project Sdk="Microsoft.Build.NoTargets" />"#),
        ],
        &[],
    );
    let after = project_checkout(&[CALLER, APP, TYPE, (nested, "<Project />")], &[]);
    let (stats, rows) = update_checked_bytes(before, after, &[nested]);
    assert!(stats.sites_resolved > 0, "{stats:?}");
    check_unknown(&rows[CALLER.0]);
}

fn reference_xml(include: &str) -> String {
    format!(r#"<Project><ItemGroup><ProjectReference Include="{include}" /></ItemGroup></Project>"#)
}

#[test]
fn csharp_direct_and_transitive_project_references_prove_type() {
    let caller_project = reference_xml("$(MSBuildThisFileDirectory)../forward/Forward.csproj");
    let forward_project = reference_xml("$(MSBuildProjectDirectory)/../types/Types.csproj");
    let target = ("types/src/Assert.cs", ASSERT);
    for direct in [true, false] {
        let direct_xml = reference_xml("../types/Types.csproj");
        let files = [
            CALLER,
            (APP.0, if direct { &direct_xml } else { &caller_project }),
            ("forward/Forward.csproj", &forward_project),
            ("types/Types.csproj", "<Project />"),
            target,
        ];
        let rows = records_match_files(&project_checkout(&files, &[]));
        for member in ["Check", "AliasCheck"] {
            exact(
                answer(&rows[CALLER.0], member),
                target.0,
                &[&format!("Assert::{member}")],
            );
        }
    }
}

#[test]
fn csharp_imported_conditional_properties_and_wildcards_do_not_prove_reference() {
    let import = reference_xml("../types/Types.csproj");
    for xml in ["<Project><Import Project=\"Directory.Build.props\" /></Project>".to_string(),
        r#"<Project><ItemGroup Condition="'$(Build)' == 'true'"><ProjectReference Include="../types/Types.csproj" /></ItemGroup></Project>"#.into(),
        r#"<Project><ItemGroup><ProjectReference Include="../types/Types.csproj" Condition="'$(Build)' == 'true'" /></ItemGroup></Project>"#.into(),
        reference_xml("$(UnknownRoot)/types/Types.csproj"), reference_xml("../types/*.csproj"), reference_xml("../../types/Types.csproj"), reference_xml("../types/NotTheSame.csproj")
    ] {
        let files = [CALLER, (APP.0,&xml), ("app/Directory.Build.props",&import), ("types/Types.csproj","<Project />"),("types/src/Assert.cs",ASSERT)];
        check_unknown(&records_match_files(&project_checkout(&files,&[]))[CALLER.0]);
    }
}

#[test]
fn csharp_project_reference_cycle_and_reference_edit_have_cold_parity() {
    let app = reference_xml("../bridge/Bridge.csproj");
    let old = reference_xml("../app/App.csproj");
    let next = r#"<Project><ItemGroup><ProjectReference Include="../app/App.csproj" /><ProjectReference Include="../types/Types.csproj" /></ItemGroup></Project>"#;
    let target = ("types/src/Assert.cs", ASSERT);
    let bridge = "bridge/Bridge.csproj";
    let fixture = |bridge_xml: &str| {
        project_checkout(
            &[
                CALLER,
                (APP.0, &app),
                (bridge, bridge_xml),
                ("types/Types.csproj", "<Project />"),
                target,
            ],
            &[],
        )
    };
    let before = fixture(&old);
    let after = fixture(next);
    check_unknown(&records_match_files(&before)[CALLER.0]);
    let (stats, rows) = update_checked_bytes(before.clone(), after.clone(), &[bridge]);
    assert!(stats.sites_resolved > 0, "{stats:?}");
    exact(
        answer(&rows[CALLER.0], "Check"),
        target.0,
        &["Assert::Check"],
    );
    let (stats, rows) = update_checked_bytes(after, before, &[bridge]);
    assert!(stats.sites_resolved > 0, "{stats:?}");
    check_unknown(&rows[CALLER.0]);
}
