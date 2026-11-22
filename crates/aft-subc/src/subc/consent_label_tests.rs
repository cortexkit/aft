use super::*;

#[test]
fn consent_scope_reason_bounds_paths_without_losing_repository_name() {
    let separator = std::path::MAIN_SEPARATOR;
    let deep = format!(
        "{separator}first{separator}{}repository",
        "middle/".repeat(30)
    )
    .replace('/', &separator.to_string());
    let long_name = format!("repository-{}-ending", "界".repeat(100));
    let cases = [
        (
            format!("{separator}first{separator}repository"),
            "repository",
        ),
        (deep, "repository"),
        (format!("{separator}{long_name}"), "repository-"),
    ];
    for (path, identity) in cases {
        let reason = scope_reason(&AccessGrant {
            path: PathBuf::from(&path),
            kind: GrantKind::Subtree,
            operation: AccessOperation::Read,
        });
        assert!(reason.chars().count() <= 160, "{path}: {reason}");
        assert!(reason.starts_with("Allow scope: read-only access for all AFT read tools within the directory tree rooted at "));
        assert!(reason.ends_with(". Protected paths remain denied."));
        assert!(reason.contains(identity), "{reason}");
        if path.contains(&long_name) {
            assert!(reason.contains('…'), "{reason}");
            assert!(reason.contains("-ending"), "{reason}");
        } else if path.contains("middle") {
            assert!(
                reason.contains(&format!("{separator}first{separator}…")),
                "{reason}"
            );
            assert!(
                reason.contains(&format!("middle{separator}repository")),
                "{reason}"
            );
        } else {
            assert!(reason.contains(&path), "{reason}");
        }
    }
}

#[test]
fn consent_scope_reason_shows_home_and_drops_first_component_before_repository() {
    let separator = std::path::MAIN_SEPARATOR;
    let home = PathBuf::from(format!("{separator}home{separator}worker"));
    let path = home.join("middle").join("repository");
    assert_eq!(
        scope_reason_path(&path, &[home], 43),
        format!("~{separator}middle{separator}repository")
    );
    let path = PathBuf::from(format!(
        "{separator}{}{separator}repository",
        "first".repeat(20)
    ));
    assert_eq!(
        scope_reason_path(&path, &[], 43),
        format!("…{separator}repository")
    );
}

pub(super) fn assert_read_tree_scope_label(reason: &str, path: &Path) {
    let prefix =
        "Allow scope: read-only access for all AFT read tools within the directory tree rooted at ";
    let suffix = ". Protected paths remain denied.";
    let shown = reason
        .strip_prefix(prefix)
        .and_then(|text| text.strip_suffix(suffix))
        .expect("the approved scope sentence surrounds the displayed path");
    assert!(reason.chars().count() <= 160, "{reason}");
    assert!(
        shown.ends_with(path.file_name().unwrap().to_str().unwrap()),
        "{reason}"
    );
    let homes = crate::subc::permissions::home_folders();
    let relative = homes.iter().find_map(|home| path.strip_prefix(home).ok());
    let full = if let Some(relative) = relative {
        assert!(shown.starts_with('~'), "{reason}");
        Path::new("~").join(relative).display().to_string()
    } else {
        path.display().to_string()
    };
    let unshortened = format!("{prefix}{full}{suffix}");
    if unshortened.chars().count() <= 160 {
        assert_eq!(reason, unshortened);
    }
}
