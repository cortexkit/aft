use super::helpers::AftProcess;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn write_file(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, content).unwrap();
    path
}

fn send(aft: &mut AftProcess, request: serde_json::Value) -> serde_json::Value {
    aft.send(&request.to_string())
}

fn outline_text(aft: &mut AftProcess, file: &Path) -> String {
    let resp = send(
        aft,
        json!({
            "id": format!("outline-{}", file.display()),
            "command": "outline",
            "file": file,
        }),
    );

    assert_eq!(resp["success"], true, "outline should succeed: {resp:?}");
    resp["text"].as_str().expect("outline text").to_string()
}

fn assert_symbol_kind(text: &str, kind: &str, name: &str) {
    assert!(
        text.lines()
            .any(|line| { line.split_whitespace().nth(1) == Some(kind) && line.contains(name) }),
        "missing {kind} symbol {name} in outline: {text}"
    );
}

#[test]
fn outline_c_header_symbols_include_macros_types_and_prototypes() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "include/sample.h",
        r#"#define MAX_SIZE 128

typedef unsigned long Count;

struct Config {
    int size;
};

enum Mode {
    MODE_A,
    MODE_B,
};

int compute(int value);
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let text = outline_text(&mut aft, &file);
    for expected in ["sample.h", "MAX_SIZE", "Count", "Config", "Mode", "compute"] {
        assert!(
            text.contains(expected),
            "missing {expected} in outline: {text}"
        );
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_cpp_symbols_include_namespaces_templates_types_and_methods() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "include/sample.hpp",
        r#"namespace math {
class Worker {
public:
    void run();
};

struct Options {
    int count;
};

enum State {
    Ready,
    Busy,
};

template <typename T>
T identity(T value) {
    return value;
}

int add(int left, int right);
}

inline void math::Worker::run() {}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let text = outline_text(&mut aft, &file);
    for expected in [
        "sample.hpp",
        "math",
        "Worker",
        "run",
        "Options",
        "State",
        "identity",
        "add",
    ] {
        assert!(
            text.contains(expected),
            "missing {expected} in outline: {text}"
        );
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_html_symbols_include_heading_hierarchy() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "index.html",
        r#"<!DOCTYPE html>
<html>
<head><title>Test Page</title></head>
<body>
  <h1>Main Title</h1>
  <p>Introduction text</p>
  <h2>First Section</h2>
  <p>Content here</p>
  <h3>Subsection A</h3>
  <p>More content</p>
  <h2>Second Section</h2>
  <article>
    <h3>Nested Article</h3>
    <p>Article content</p>
  </article>
</body>
</html>
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let text = outline_text(&mut aft, &file);
    // Should have all headings
    for expected in [
        "Main Title",
        "First Section",
        "Subsection A",
        "Second Section",
        "Nested Article",
    ] {
        assert!(
            text.contains(expected),
            "missing {expected} in outline: {text}"
        );
    }
    // Should show heading kind abbreviation
    assert!(
        text.contains(" h "),
        "should contain heading kind 'h': {text}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_scala3_symbols_include_enums_and_named_givens() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "src/main/scala/demo/Color.scala",
        r#"package demo

enum Color {
  case Red, Green, Blue
  def describe: String = this match { case Red => "r"; case Green => "g"; case Blue => "b" }
}

given Ordering[Int] = Ordering.fromLessThan(_ < _)

given intShow: Show[Int] = Show.show(_.toString)

object Module {
  given stringEq: Eq[String] = (a, b) => a == b
}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let text = outline_text(&mut aft, &file);
    for expected in [
        "E cls  enum Color 3:6",
        "    .E mth  def describe: String = this match { case Red => \"r\"; case Green => \"g\"; case Blue => \"b\" } 5:5",
        "E var  given intShow: Show[Int] = Show.show(_.toString) 10:10",
        "E cls  object Module 12:14",
        "    .E var  given stringEq: Eq[String] = (a, b) => a == b 13:13",
    ] {
        assert!(
            text.contains(expected),
            "missing {expected} in outline: {text}"
        );
    }
    assert!(
        !text.contains("Ordering[Int]"),
        "anonymous given should be skipped: {text}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_java_symbols_include_types_members_and_fields() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "src/main/java/demo/Greeter.java",
        r#"package demo;

public class Greeter {
    private String name;

    public Greeter(String name) {
        this.name = name;
    }

    public String greet(String who) {
        return "Hi " + who;
    }
}

interface Named {
    String name();
}

enum Color { RED, GREEN }

record Point(int x, int y) {}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let text = outline_text(&mut aft, &file);

    assert_symbol_kind(&text, "cls", "Greeter");
    assert_symbol_kind(&text, "var", "name");
    assert_symbol_kind(&text, "mth", "greet");
    assert_symbol_kind(&text, "ifc", "Named");
    assert_symbol_kind(&text, "enum", "Color");
    assert_symbol_kind(&text, "st", "Point");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_ruby_symbols_include_modules_classes_methods_and_constants() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "lib/demo/greeter.rb",
        r#"module Demo
  class Greeter
    DEFAULT_NAME = "world"

    def initialize(name)
      @name = name
    end

    def greet(who)
      "Hi #{who}"
    end

    def self.build
      new(DEFAULT_NAME)
    end
  end
end
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let text = outline_text(&mut aft, &file);

    assert_symbol_kind(&text, "cls", "Demo");
    assert_symbol_kind(&text, "cls", "Greeter");
    assert_symbol_kind(&text, "var", "DEFAULT_NAME");
    assert_symbol_kind(&text, "mth", "greet");
    assert_symbol_kind(&text, "mth", "build");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_kotlin_symbols_include_classes_functions_properties_and_typealiases() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "src/main/kotlin/demo/Greeter.kt",
        r#"package demo

class Greeter(val name: String) {
    fun greet(who: String): String = "Hi $who"
    val label: String = name
}

fun helper(): Greeter = Greeter("world")

typealias Name = String
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let text = outline_text(&mut aft, &file);

    assert_symbol_kind(&text, "cls", "Greeter");
    assert_symbol_kind(&text, "mth", "greet");
    assert_symbol_kind(&text, "var", "label");
    assert_symbol_kind(&text, "fn", "helper");
    assert_symbol_kind(&text, "type", "Name");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_swift_symbols_include_types_protocols_functions_and_properties() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "Sources/Demo/Greeter.swift",
        r#"struct Greeter {
    let name: String

    func greet(_ who: String) -> String {
        return "Hi \(who)"
    }
}

class Service {
    func run() {}
}

protocol Named {
    func name() -> String
}

enum Color { case red, green }

typealias Name = String
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let text = outline_text(&mut aft, &file);

    assert_symbol_kind(&text, "st", "Greeter");
    assert_symbol_kind(&text, "var", "name");
    assert_symbol_kind(&text, "mth", "greet");
    assert_symbol_kind(&text, "cls", "Service");
    assert_symbol_kind(&text, "ifc", "Named");
    assert_symbol_kind(&text, "enum", "Color");
    assert_symbol_kind(&text, "type", "Name");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_php_symbols_include_namespaces_types_functions_methods_and_properties() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "src/Greeter.php",
        r#"<?php
namespace Demo;

interface Named {
    public function name(): string;
}

trait Logs {
    public function log(string $msg): void {}
}

class Greeter {
    private string $name;

    public function greet(string $who): string {
        return "Hi $who";
    }
}

function helper(): void {}

enum Color { case Red; case Green; }
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let text = outline_text(&mut aft, &file);

    assert_symbol_kind(&text, "cls", "Demo");
    assert_symbol_kind(&text, "ifc", "Named");
    assert_symbol_kind(&text, "ifc", "Logs");
    assert_symbol_kind(&text, "cls", "Greeter");
    assert_symbol_kind(&text, "var", "name");
    assert_symbol_kind(&text, "mth", "greet");
    assert_symbol_kind(&text, "fn", "helper");
    assert_symbol_kind(&text, "enum", "Color");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_lua_symbols_include_module_tables_functions_and_methods() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "lua/demo/greeter.lua",
        r#"local M = {}

function M.greet(name)
  return "Hi " .. name
end

function M:run()
end

local function helper()
end

return M
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let text = outline_text(&mut aft, &file);

    assert_symbol_kind(&text, "var", "M");
    assert_symbol_kind(&text, "mth", "greet");
    assert_symbol_kind(&text, "mth", "run");
    assert_symbol_kind(&text, "fn", "helper");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_perl_symbols_include_packages_subroutines_constants_and_variables() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "lib/Demo/Greeter.pm",
        r#"package Demo::Greeter;

use constant DEFAULT_NAME => 'world';

sub new {
    my ($class, $name) = @_;
    bless { name => $name }, $class;
}

sub greet {
    my ($self, $who) = @_;
    return "Hi $who";
}

my $counter = 0;
1;
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let text = outline_text(&mut aft, &file);

    assert_symbol_kind(&text, "cls", "Demo::Greeter");
    assert_symbol_kind(&text, "var", "DEFAULT_NAME");
    assert_symbol_kind(&text, "mth", "new");
    assert_symbol_kind(&text, "mth", "greet");
    assert_symbol_kind(&text, "var", "$counter");

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn zoom_html_heading_returns_content_with_context() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "page.html",
        r#"<html>
<body>
  <h1>Welcome</h1>
  <p>Intro paragraph</p>
  <h2>Features</h2>
  <p>Feature list here</p>
  <h2>About</h2>
  <p>About section</p>
</body>
</html>
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-html",
            "command": "zoom",
            "file": file,
            "symbol": "Features",
        }),
    );

    assert_eq!(resp["success"], true, "zoom should succeed: {resp:?}");
    assert_eq!(resp["name"], "Features");
    assert_eq!(resp["kind"], "heading");
    let content = resp["content"].as_str().unwrap();
    // Section content must include the heading AND the paragraph beneath it.
    assert!(
        content.contains("Features"),
        "content should contain heading text: {content}"
    );
    assert!(
        content.contains("Feature list here"),
        "content should include section body, not just the heading line: {content}"
    );
    // The About section belongs to a different heading — must not bleed in.
    assert!(
        !content.contains("About section"),
        "content should stop before next sibling heading: {content}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn zoom_html_heading_accepts_outline_style_prefix() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "page.html",
        r#"<html>
<body>
  <h1>Welcome</h1>
  <p>Intro paragraph</p>
  <h2 class="section">Features</h2>
  <p>Feature list here</p>
</body>
</html>
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let bare = send(
        &mut aft,
        json!({"id": "zoom-html-bare", "command": "zoom", "file": file, "symbol": "Features"}),
    );
    let prefixed = send(
        &mut aft,
        json!({"id": "zoom-html-prefixed", "command": "zoom", "file": file, "symbol": "<h2 class=\"section\">Features"}),
    );

    assert_eq!(
        bare["success"], true,
        "bare heading should succeed: {bare:?}"
    );
    assert_eq!(
        prefixed["success"], true,
        "prefixed html heading should succeed: {prefixed:?}"
    );
    assert_eq!(prefixed["range"], bare["range"]);
    assert!(prefixed["content"]
        .as_str()
        .unwrap()
        .contains("Feature list here"));

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn zoom_html_last_heading_range_stays_within_file() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "last-heading.html",
        "<html>\n<body>\n<h1>Last Heading</h1>\n<p>Final paragraph</p>\n</body>\n</html>\n",
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-html-last",
            "command": "zoom",
            "file": file,
            "symbol": "Last Heading",
        }),
    );

    assert_eq!(resp["success"], true, "zoom should succeed: {resp:?}");
    assert!(
        resp["content"]
            .as_str()
            .unwrap()
            .contains("Final paragraph"),
        "last heading should include trailing section content: {resp:?}"
    );
    assert!(
        resp["range"]["end_line"].as_u64().unwrap() <= 6,
        "end_line should not point past EOF: {resp:?}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

/// Regression: aft_zoom on an HTML heading must return the full section extent
/// (heading through the line before the next same-or-shallower heading), not
/// just the single heading element line.
#[test]
fn zoom_html_heading_returns_section_extent_not_just_heading_line() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "docs.html",
        r#"<html><body>
<h2>Installation</h2>
<p>Run npm install.</p>
<pre>npm install pkg</pre>
<h2>Configuration</h2>
<p>Set env vars.</p>
<h3>Advanced</h3>
<p>Advanced details here.</p>
<h2>Usage</h2>
<p>Call the API.</p>
</body></html>
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({ "id": "z1", "command": "zoom", "file": file, "symbol": "Configuration" }),
    );
    assert_eq!(resp["success"], true);
    let content = resp["content"].as_str().unwrap();
    // Must include the h2 itself, the <p>, and the nested h3 + its content.
    assert!(content.contains("Configuration"), "missing h2: {content}");
    assert!(
        content.contains("Set env vars"),
        "missing section body: {content}"
    );
    assert!(
        content.contains("Advanced"),
        "nested h3 should be within section: {content}"
    );
    assert!(
        content.contains("Advanced details here"),
        "nested content should be included: {content}"
    );
    // Usage is a sibling h2 — must not bleed in.
    assert!(
        !content.contains("Call the API"),
        "sibling section must not bleed in: {content}"
    );

    // h1 section: should span to EOF when it's the last heading.
    let resp2 = send(
        &mut aft,
        json!({ "id": "z2", "command": "zoom", "file": file, "symbol": "Installation" }),
    );
    assert_eq!(resp2["success"], true);
    let content2 = resp2["content"].as_str().unwrap();
    assert!(
        content2.contains("npm install pkg"),
        "Installation section body missing: {content2}"
    );
    assert!(
        !content2.contains("Set env vars"),
        "Installation must stop before Configuration: {content2}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_zig_symbols_include_containers_consts_tests_and_functions() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "src/sample.zig",
        r#"const PI = 3.14;

const Payload = union {
    int: i32,
    text: []const u8,
};

const Status = enum {
    ready,
    busy,
};

const Config = struct {
    port: u16,

    pub fn init() Config {
        return .{ .port = 80 };
    }
};

fn greet(name: []const u8) void {
    _ = name;
}

test "config init" {}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let text = outline_text(&mut aft, &file);
    for expected in [
        "sample.zig",
        "PI",
        "Payload",
        "Status",
        "Config",
        "init",
        "greet",
        "config init",
    ] {
        assert!(
            text.contains(expected),
            "missing {expected} in outline: {text}"
        );
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_csharp_symbols_include_namespace_types_members_and_properties() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "src/Sample.cs",
        r#"namespace Demo.Tools;

public interface IWorker
{
    string Name { get; }
}

public class Worker
{
    public string Name { get; }
    public int Count { get; set; }

    public void Run() {}
}

public struct Options
{
    public int Count { get; set; }
}

public enum Mode
{
    Fast,
    Slow,
}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let text = outline_text(&mut aft, &file);
    for expected in [
        "Sample.cs",
        "Demo.Tools",
        "IWorker",
        "Worker",
        "Name",
        "Count",
        "Run",
        "Options",
        "Mode",
    ] {
        assert!(
            text.contains(expected),
            "missing {expected} in outline: {text}"
        );
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_supports_requested_new_extensions() {
    let dir = TempDir::new().unwrap();
    let files = vec![
        write_file(
            dir.path(),
            "src/sample.c",
            "int c_file(void) { return 1; }\n",
        ),
        write_file(dir.path(), "include/sample.h", "int h_file(void);\n"),
        write_file(dir.path(), "src/sample.cc", "int cc_file() { return 2; }\n"),
        write_file(
            dir.path(),
            "src/sample.cpp",
            "int cpp_file() { return 3; }\n",
        ),
        write_file(
            dir.path(),
            "include/sample.hpp",
            "struct HppType { int value; };\n",
        ),
        write_file(
            dir.path(),
            "src/sample.cs",
            "class CsType { void Run() {} }\n",
        ),
        write_file(dir.path(), "src/sample.zig", "fn zigFile() void {}\n"),
        write_file(
            dir.path(),
            "contracts/sample.sol",
            "// SPDX-License-Identifier: MIT\npragma solidity ^0.8.20;\ncontract SolType { function solFn() public {} }\n",
        ),
    ];

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "outline-new-exts",
            "command": "outline",
            "files": files,
        }),
    );

    assert_eq!(resp["success"], true, "outline should succeed: {resp:?}");
    let text = resp["text"].as_str().expect("outline text");
    for expected in [
        "sample.c",
        "sample.h",
        "sample.cc",
        "sample.cpp",
        "sample.hpp",
        "sample.cs",
        "sample.zig",
        "sample.sol",
        "c_file",
        "h_file",
        "cc_file",
        "cpp_file",
        "HppType",
        "CsType",
        "zigFile",
        "SolType",
        "solFn",
    ] {
        assert!(
            text.contains(expected),
            "missing {expected} in outline: {text}"
        );
    }

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_bash_symbols_include_functions() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "scripts/deploy.sh",
        r#"#!/bin/bash

APP_NAME="my-app"
export LOG_LEVEL="info"

function setup_environment() {
    local dir="$1"
    mkdir -p "$dir"
}

cleanup() {
    rm -rf /tmp/cache
}

main() {
    setup_environment "/tmp/app"
    echo "Starting $APP_NAME"
}

main "$@"
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let text = outline_text(&mut aft, &file);

    // Should find all three function definitions
    assert!(
        text.contains("setup_environment"),
        "missing setup_environment in bash outline: {text}"
    );
    assert!(
        text.contains("cleanup"),
        "missing cleanup in bash outline: {text}"
    );
    assert!(
        text.contains("main"),
        "missing main in bash outline: {text}"
    );

    // Functions should be marked as functions
    assert!(
        text.contains("fn"),
        "bash functions should have 'fn' kind marker: {text}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_solidity_symbols_include_contracts_functions_events_and_errors() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "contracts/Vault.sol",
        r#"// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

interface IERC20 {
    function transfer(address to, uint256 amount) external returns (bool);
}

library SafeMath {
    function add(uint256 a, uint256 b) internal pure returns (uint256) {
        return a + b;
    }
}

contract Vault {
    address public owner;
    uint256 private _totalDeposits;

    event Deposit(address indexed from, uint256 amount);
    error Unauthorized(address caller);

    struct UserInfo {
        uint256 balance;
        uint256 lastUpdate;
    }

    enum Status { Active, Paused, Closed }

    modifier onlyOwner() {
        if (msg.sender != owner) revert Unauthorized(msg.sender);
        _;
    }

    constructor(address initialOwner) {
        owner = initialOwner;
    }

    function deposit() external payable {
        _totalDeposits += msg.value;
        emit Deposit(msg.sender, msg.value);
    }

    function withdraw(uint256 amount) external onlyOwner {
        payable(owner).transfer(amount);
    }
}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);
    let text = outline_text(&mut aft, &file);

    // Top-level containers
    assert!(
        text.contains("IERC20"),
        "missing IERC20 interface in solidity outline: {text}"
    );
    assert!(
        text.contains("SafeMath"),
        "missing SafeMath library in solidity outline: {text}"
    );
    assert!(
        text.contains("Vault"),
        "missing Vault contract in solidity outline: {text}"
    );

    // Members of the Vault contract
    assert!(
        text.contains("constructor"),
        "missing constructor in solidity outline: {text}"
    );
    assert!(
        text.contains("deposit"),
        "missing deposit function in solidity outline: {text}"
    );
    assert!(
        text.contains("withdraw"),
        "missing withdraw function in solidity outline: {text}"
    );
    assert!(
        text.contains("onlyOwner"),
        "missing onlyOwner modifier in solidity outline: {text}"
    );

    // Events and errors
    assert!(
        text.contains("Deposit"),
        "missing Deposit event in solidity outline: {text}"
    );
    assert!(
        text.contains("Unauthorized"),
        "missing Unauthorized error in solidity outline: {text}"
    );

    // Data types
    assert!(
        text.contains("UserInfo"),
        "missing UserInfo struct in solidity outline: {text}"
    );
    assert!(
        text.contains("Status"),
        "missing Status enum in solidity outline: {text}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}

#[test]
fn outline_solidity_zoom_returns_function_body() {
    let dir = TempDir::new().unwrap();
    let file = write_file(
        dir.path(),
        "contracts/Counter.sol",
        r#"// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

contract Counter {
    uint256 public count;

    function increment() public {
        count += 1;
    }

    function decrement() public {
        require(count > 0, "underflow");
        count -= 1;
    }
}
"#,
    );

    let mut aft = AftProcess::spawn();
    assert_eq!(aft.configure(dir.path())["success"], true);

    let resp = send(
        &mut aft,
        json!({
            "id": "zoom-solidity",
            "command": "zoom",
            "file": file,
            "symbol": "increment",
        }),
    );

    assert_eq!(resp["success"], true, "zoom should succeed: {resp:?}");
    assert_eq!(resp["name"], "increment");
    let content = resp["content"].as_str().unwrap_or("");
    assert!(
        content.contains("count += 1"),
        "zoom content should contain function body: {content}"
    );

    let status = aft.shutdown();
    assert!(status.success());
}
