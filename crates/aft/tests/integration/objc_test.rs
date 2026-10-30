use serde_json::json;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

use aft::language::LanguageProvider;
use aft::parser::{detect_language, LangId, TreeSitterProvider};
use aft::search_index::SearchIndex;

use super::helpers::AftProcess;

fn setup_project(files: &[(&str, &str)]) -> TempDir {
    let temp_dir = tempfile::tempdir().expect("create temp dir");

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

const GREETER_M: &str = r#"
@interface CKGreeter : NSObject
@property (nonatomic, copy) NSString *title;
+ (instancetype)sharedGreeter;
- (NSString *)greetWithName:(NSString *)name punctuation:(NSString *)punctuation;
@end

@protocol CKRunnable
@property (nonatomic, assign) BOOL enabled;
- (void)run;
@end

typedef NSInteger CKCount;

static NSString *CKFormatName(NSString *name) {
  return name;
}

void CKSendMessage(id obj, id bar) {
  [obj foo:bar];
}

@implementation CKGreeter
+ (instancetype)sharedGreeter {
  return [CKGreeter new];
}

- (NSString *)greetWithName:(NSString *)name punctuation:(NSString *)punctuation {
  NSString *message = [self format:name punctuation:punctuation];
  return message;
}

- (NSString *)format:(NSString *)value punctuation:(NSString *)punctuation {
  return [NSString stringWithFormat:@"%@%@", value, punctuation];
}
@end
"#;

const MIXED_MM: &str = r#"
@implementation CKObjCxxThing
- (void)touchCpp {
#ifdef __cplusplus
  std::string token = "objcxxSearchNeedle";
#endif
}
@end
"#;

#[test]
fn test_objc_outline_zoom_extensions_and_search_index() {
    let project = setup_project(&[
        ("Greeter.m", GREETER_M),
        ("Mixed.mm", MIXED_MM),
        ("Header.h", "@interface HeaderOnly : NSObject\n@end\n"),
    ]);

    assert_eq!(detect_language(Path::new("Greeter.m")), Some(LangId::ObjC));
    assert_eq!(detect_language(Path::new("Mixed.mm")), Some(LangId::ObjC));
    assert_eq!(detect_language(Path::new("Header.h")), Some(LangId::C));

    let provider = TreeSitterProvider::new();
    let symbols = provider
        .list_symbols(&project.path().join("Greeter.m"))
        .expect("list Objective-C symbols");
    let symbol_names = symbols
        .iter()
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>();
    for expected in ["greetWithName:punctuation:", "format:punctuation:"] {
        assert!(
            symbol_names.contains(&expected),
            "missing symbol name {expected}: {symbol_names:?}"
        );
    }

    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let outline_resp = send(
        &mut aft,
        json!({
            "id": "outline-objc",
            "command": "outline",
            "file": project.path().join("Greeter.m"),
        }),
    );
    assert_eq!(
        outline_resp["success"], true,
        "outline should succeed: {outline_resp:?}"
    );
    let text = outline_resp["text"].as_str().expect("outline text");
    for expected in [
        "Greeter.m",
        "CKGreeter",
        "CKRunnable",
        "title",
        "enabled",
        "CKCount",
        "CKFormatName",
        "sharedGreeter",
        "greetWithName",
        "format",
    ] {
        assert!(
            text.contains(expected),
            "missing {expected} in outline: {text}"
        );
    }

    let zoom_resp = send(
        &mut aft,
        json!({
            "id": "zoom-objc-method",
            "command": "zoom",
            "file": project.path().join("Greeter.m"),
            "symbol": "greetWithName:punctuation:",
        }),
    );
    assert_eq!(
        zoom_resp["success"], true,
        "zoom should succeed: {zoom_resp:?}"
    );
    assert_eq!(zoom_resp["name"], "greetWithName:punctuation:");
    assert_eq!(zoom_resp["kind"], "method");
    let content = zoom_resp["content"].as_str().expect("zoom content");
    assert!(
        content.contains("- (NSString *)greetWithName:(NSString *)name")
            && content.contains("[self format:name punctuation:punctuation]"),
        "zoom content should contain method body: {content}"
    );

    let objcxx_outline = send(
        &mut aft,
        json!({
            "id": "outline-objcxx",
            "command": "outline",
            "file": project.path().join("Mixed.mm"),
        }),
    );
    assert_eq!(
        objcxx_outline["success"], true,
        ".mm outline should succeed: {objcxx_outline:?}"
    );
    let objcxx_text = objcxx_outline["text"].as_str().expect(".mm outline text");
    assert!(
        objcxx_text.contains("CKObjCxxThing"),
        ".mm class missing: {objcxx_text}"
    );
    assert!(
        objcxx_text.contains("touchCpp"),
        ".mm method missing: {objcxx_text}"
    );

    let index = SearchIndex::build(project.path());
    let search = index
        .grep("objcxxSearchNeedle", true, &[], &[], project.path(), 10)
        .unwrap();
    assert_eq!(
        search.total_matches, 1,
        ".mm file should be indexed: {search:?}"
    );
    assert!(
        search.matches.iter().any(|m| m.file.ends_with("Mixed.mm")),
        "search index should return Mixed.mm: {search:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn test_objc_ast_grep_search_with_meta_variables() {
    let project = setup_project(&[("Messages.m", GREETER_M)]);

    let mut aft = AftProcess::spawn();
    configure(&mut aft, project.path());

    let search_resp = send(
        &mut aft,
        json!({
            "id": "ast-search-objc",
            "command": "ast_search",
            "pattern": "[obj foo:$ARG]",
            "lang": "objc",
        }),
    );

    assert_eq!(
        search_resp["success"], true,
        "ast_search should succeed: {search_resp:?}"
    );
    assert_eq!(
        search_resp["total_matches"], 1,
        "Objective-C message pattern should match once: {search_resp:?}"
    );
    let captured_arg = search_resp["matches"][0]["meta_variables"]["$ARG"]
        .as_str()
        .expect("captured $ARG");
    assert_eq!(captured_arg, "bar");

    let status = aft.shutdown();
    assert!(status.success());
}
