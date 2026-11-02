use serde_json::json;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

use super::helpers::AftProcess;

fn setup_project(files: &[(&str, &str)]) -> TempDir {
    let temp_dir = crate::test_helpers::tempdir();

    for (relative_path, content) in files {
        let path = temp_dir.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent directories");
        }
        fs::write(path, content).expect("write fixture file");
    }

    temp_dir
}

fn configure(aft: &mut AftProcess, root: &Path) {
    let resp = aft.configure(root);
    assert_eq!(resp["success"], true, "configure should succeed: {resp:?}");
}

fn send(aft: &mut AftProcess, request: serde_json::Value) -> serde_json::Value {
    aft.send(&serde_json::to_string(&request).expect("serialize request"))
}

#[test]
fn test_pascal_outline_and_zoom() {
    let project = setup_project(&[(
        "MyUnit.pas",
        r#"
unit MyUnit;

interface

type
  TMyClass = class
  public
    procedure DoSomething;
  end;

implementation

procedure TMyClass.DoSomething;
begin
  writeln('Hello');
end;

procedure StandaloneProc;
begin
end;

end.
"#,
    )]);

    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let file_path = project.path().join("MyUnit.pas");

    // 1. Test Outline
    let outline_resp = send(
        &mut aft,
        json!({
            "id": "outline-pascal",
            "command": "outline",
            "file": file_path,
        }),
    );

    assert_eq!(
        outline_resp["success"], true,
        "outline should succeed: {:?}",
        outline_resp
    );
    let text = outline_resp["text"].as_str().expect("outline text");
    println!("OUTLINE TEXT:\n{}", text);
    assert!(
        text.contains("MyUnit.pas"),
        "outline should contain filename"
    );
    assert!(
        text.contains("E unit MyUnit"),
        "outline should contain unit"
    );
    assert!(
        text.contains("E TMyClass = class"),
        "outline should contain class"
    );
    assert!(
        text.contains("E procedure DoSomething"),
        "outline should contain method"
    );

    // 2. Test Zoom
    let zoom_resp = send(
        &mut aft,
        json!({
            "id": "zoom-pascal",
            "command": "zoom",
            "file": file_path,
            "symbol": "StandaloneProc",
        }),
    );

    assert_eq!(
        zoom_resp["success"], true,
        "zoom should succeed: {:?}",
        zoom_resp
    );
    assert_eq!(zoom_resp["name"], "StandaloneProc");
    assert_eq!(zoom_resp["kind"], "function");
    let content = zoom_resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("procedure StandaloneProc;"),
        "zoom content should contain method definition"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_pascal_ast_grep_search_and_replace() {
    let project = setup_project(&[(
        "MyUnit.pas",
        r#"
unit MyUnit;

interface

procedure SayHello(name: string);
procedure SayGoodbye(name: string);

implementation

procedure SayHello(name: string);
begin
  writeln('Hello ', name);
end;

procedure SayGoodbye(name: string);
begin
  writeln('Goodbye ', name);
end;

end.
"#,
    )]);

    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    // 1. Test AST Search
    let search_resp = send(
        &mut aft,
        json!({
            "id": "search-pascal",
            "command": "ast_search",
            "pattern": "writeln($MSG, $NAME)",
            "lang": "pascal",
        }),
    );

    println!("SEARCH RESP: {:?}", search_resp);

    assert_eq!(
        search_resp["success"], true,
        "ast_search should succeed: {:?}",
        search_resp
    );
    assert_eq!(search_resp["total_matches"], 2);

    // 2. Test AST Replace
    let replace_resp = send(
        &mut aft,
        json!({
            "id": "replace-pascal",
            "command": "ast_replace",
            "pattern": "writeln($MSG, $NAME)",
            "rewrite": "LogMessage($MSG, $NAME)",
            "lang": "pascal",
            "dry_run": false,
        }),
    );

    assert_eq!(
        replace_resp["success"], true,
        "ast_replace should succeed: {:?}",
        replace_resp
    );

    // Verify file content was updated
    let updated_content = fs::read_to_string(project.path().join("MyUnit.pas")).unwrap();
    assert!(
        updated_content.contains("LogMessage('Hello ', name)"),
        "content should be rewritten"
    );
    assert!(
        updated_content.contains("LogMessage('Goodbye ', name)"),
        "content should be rewritten"
    );

    let status = aft.shutdown();
    assert!(status.success());
}
