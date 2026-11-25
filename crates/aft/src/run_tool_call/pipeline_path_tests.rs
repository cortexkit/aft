use super::*;
use std::{fs, path::Path};

#[test]
fn tool_relative_missing_path_uses_native_separators() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let expected = root.join(".cortexkit").join("no-such-file.txt");
    for input in [
        ".cortexkit/no-such-file.txt",
        ".cortexkit//no-such-file.txt",
    ] {
        let resolved = crate::subc_translate::resolve_path_from_project_root(root, input);
        assert_eq!(resolved.as_os_str(), expected.as_os_str(), "{input}");
    }
}

fn call(root: &Path, name: &str, args: Value) -> ToolCallResult {
    let ctx = AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        crate::config::Config {
            project_root: Some(root.to_owned()),
            restrict_to_project_root: true,
            storage_dir: Some(root.join("storage")),
            ..Default::default()
        },
    );
    let format = crate::subc_format::FormatContext::from_tool_call(name, &args, root);
    let tool_ctx = ToolCallContext {
        project_root: root.to_owned(),
        request_id: format!("test-{name}"),
        ..Default::default()
    };
    let dispatch = |req: RawRequest, ctx: &AppContext| match req.command.as_str() {
        "read" => crate::commands::read::handle_read(&req, ctx),
        "write" => crate::commands::write::handle_write(&req, ctx),
        "grep" => crate::commands::grep::handle_grep(&req, ctx),
        other => panic!("unexpected command: {other}"),
    };
    let ToolCallOutcome::Unary(result) =
        run_tool_call(name, args, &format, &tool_ctx, &ctx, &dispatch, None, None);
    result
}

#[cfg(unix)]
#[test]
fn tool_read_and_write_follow_the_physical_symlink_parent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("pkg/deep")).unwrap();
    fs::write(root.join("pkg/target.txt"), "physical parent\n").unwrap();
    fs::write(root.join("target.txt"), "lexical parent\n").unwrap();
    std::os::unix::fs::symlink("pkg/deep", root.join("link")).unwrap();
    let path = "link/../target.txt";
    let expected = fs::read_to_string(root.join(path)).unwrap();
    assert_eq!(expected, "physical parent\n");

    let read = call(&root, "read", json!({"path":path}));
    assert!(read.response.success, "{:?}", read.response);
    let write = call(&root, "write", json!({"path":path,"content":"changed\n"}));
    assert!(write.response.success, "{:?}", write.response);
    assert_eq!(
        (
            read.text.contains(expected.trim_end()),
            fs::read_to_string(root.join(path)).unwrap(),
            fs::read_to_string(root.join("target.txt")).unwrap()
        ),
        (true, "changed\n".to_owned(), "lexical parent\n".to_owned()),
        "translated read returned {:?}",
        read.text,
    );
}

#[cfg(unix)]
#[test]
fn tool_write_uses_physical_parent_for_missing_targets_and_root_checks() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("project");
    let outside = base.join("outside");
    fs::create_dir_all(root.join("pkg/deep")).unwrap();
    fs::create_dir_all(outside.join("deep")).unwrap();
    std::os::unix::fs::symlink("pkg/deep", root.join("link")).unwrap();
    std::os::unix::fs::symlink(outside.join("deep"), root.join("escape")).unwrap();

    let args = |path| json!({"path":path,"content":"created\n"});
    let created = call(&root, "write", args("link/../new.txt"));
    assert!(created.response.success, "{:?}", created.response);
    assert_eq!(
        fs::read_to_string(root.join("pkg/new.txt")).unwrap(),
        "created\n"
    );
    assert!(!root.join("new.txt").exists());

    let blocked = call(&root, "write", args("escape/../blocked.txt"));
    assert!(!blocked.response.success, "{:?}", blocked.response);
    assert_eq!(blocked.response.data["code"], "path_outside_root");
    assert!(!outside.join("blocked.txt").exists());
    assert!(!root.join("blocked.txt").exists());
}

#[test]
fn tool_grep_does_not_drop_missing_roots() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir
        .path()
        .canonicalize()
        .unwrap()
        .join("project with spaces");
    fs::create_dir(&root).unwrap();
    for (path, marker) in [
        ("first", "one"),
        ("second", "two"),
        ("literal name", "literal"),
    ] {
        fs::create_dir(root.join(path)).unwrap();
        fs::write(
            root.join(path).join("sample.txt"),
            format!("needle {marker}\n"),
        )
        .unwrap();
    }
    let grep = |path: Value| call(&root, "grep", json!({"pattern":"needle","path":path}));

    let missing = grep(json!("first missing"));
    assert!(!missing.response.success, "{:?}", missing.response);
    assert_eq!(missing.response.data["code"], "path_not_found");
    assert!(
        missing
            .text
            .contains(&*root.join("missing").to_string_lossy()),
        "{}",
        missing.text
    );

    let literal = grep(json!("literal name"));
    assert!(
        literal.response.success && literal.text.contains("needle literal"),
        "{}",
        literal.text
    );
    let both = grep(json!("first second"));
    assert!(
        both.response.success
            && both.text.contains("needle one")
            && both.text.contains("needle two"),
        "{}",
        both.text
    );

    let explicit = grep(json!(["literal name", "second"]));
    assert!(explicit.response.success, "{:?}", explicit.response);
    assert!(
        explicit.text.contains("needle literal"),
        "{}",
        explicit.text
    );
    assert!(explicit.text.contains("needle two"), "{}", explicit.text);
    let missing = grep(json!(["first", "missing"]));
    assert_eq!(missing.response.data["code"], "path_not_found");
}
