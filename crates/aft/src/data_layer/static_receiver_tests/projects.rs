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
