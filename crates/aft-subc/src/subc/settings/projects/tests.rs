use super::*;

mod compatibility;

fn project(id: &str, registered: bool) -> Arc<Project> {
    let user = ApprovedUser::new(validate_user(&json!({})).unwrap());
    let schema = Arc::new(Schema::new(crate::subc::settings::tests::sample_schema()).unwrap());
    let result = Projects::default().slot(id, &user, &schema);
    result.registered.store(registered, Ordering::Release);
    result
}

#[test]
fn settings_unspecified_and_unknown_routes_share_the_roots_project_view() {
    let (_dir, root) = crate::subc::test_support::test_root("settings-shared-project-view");
    let mut roots = RootChannels::default();
    let incumbent = project("registered", true);
    roots.remember_project(&root, incumbent.clone());
    for incoming in [None, Some(project("unknown", false))] {
        let selected = roots.select_project(&root, incoming).unwrap().unwrap();
        assert!(Arc::ptr_eq(&incumbent, &selected));
    }
}

#[test]
fn settings_root_with_user_view_upgrades_when_a_project_route_binds() {
    let (_dir, root) = crate::subc::test_support::test_root("settings-upgrade-project-view");
    let mut roots = RootChannels::default();
    roots
        .channels
        .entry(root.clone())
        .or_default()
        .insert(route_key(1, 1));
    assert!(roots.select_project(&root, None).unwrap().is_none());
    let registered = project("registered", true);
    let selected = roots
        .select_project(&root, Some(registered.clone()))
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&selected, &registered));
    roots.remember_project(&root, selected);
    let subsequent = roots.select_project(&root, None).unwrap().unwrap();
    assert!(Arc::ptr_eq(&subsequent, &registered));
}

#[test]
fn settings_conflicting_explicit_project_id_does_not_replace_the_roots_view() {
    let (_dir, root) = crate::subc::test_support::test_root("settings-conflicting-project-view");
    let mut roots = RootChannels::default();
    let incumbent = project("registered", true);
    roots.remember_project(&root, incumbent.clone());
    assert!(roots
        .select_project(&root, Some(project("other", true)))
        .is_err());
    let selected = roots.select_project(&root, None).unwrap().unwrap();
    assert!(Arc::ptr_eq(&incumbent, &selected));
    roots.remove(&root);
    assert!(roots.select_project(&root, None).unwrap().is_none());
}
