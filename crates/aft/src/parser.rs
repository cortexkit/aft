use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, RwLock};
use std::time::SystemTime;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor, Tree};

use crate::cache_freshness::{self, FileFreshness, FreshnessVerdict};
use crate::callgraph::resolve_module_path;
use crate::error::AftError;
use crate::symbol_cache_disk;
use crate::symbols::{Range, Symbol, SymbolKind, SymbolMatch};

const MAX_REEXPORT_DEPTH: usize = 10;

// --- Query patterns embedded at compile time ---

const TS_QUERY: &str = r#"
;; function declarations
(function_declaration
  name: (identifier) @fn.name) @fn.def

;; function-like values assigned to const/let/var
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow.name
    value: (arrow_function) @arrow.body) @arrow.decl) @arrow.def
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow.name
    value: (function_expression) @arrow.body) @arrow.decl) @arrow.def
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow.name
    value: (generator_function) @arrow.body) @arrow.decl) @arrow.def

;; anonymous default exports
(export_statement
  value: (function_expression) @default.body) @default.def
(export_statement
  value: (generator_function) @default.body) @default.def
(export_statement
  value: (class) @default.body) @default.def

;; class declarations
(class_declaration
  name: (type_identifier) @class.name) @class.def

;; method definitions inside classes
(class_declaration
  name: (type_identifier) @method.class_name
  body: (class_body
    (method_definition
      name: (property_identifier) @method.name) @method.def))

;; interface declarations
(interface_declaration
  name: (type_identifier) @interface.name) @interface.def

;; enum declarations
(enum_declaration
  name: (identifier) @enum.name) @enum.def

;; type alias declarations
(type_alias_declaration
  name: (type_identifier) @type_alias.name) @type_alias.def

;; top-level const/let variable declarations
(lexical_declaration
  (variable_declarator
    name: (identifier) @var.name) @var.decl) @var.def

;; export statement wrappers (top-level only)
(export_statement) @export.stmt
"#;

const JS_QUERY: &str = r#"
;; function declarations
(function_declaration
  name: (identifier) @fn.name) @fn.def

;; function-like values assigned to const/let/var
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow.name
    value: (arrow_function) @arrow.body) @arrow.decl) @arrow.def
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow.name
    value: (function_expression) @arrow.body) @arrow.decl) @arrow.def
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow.name
    value: (generator_function) @arrow.body) @arrow.decl) @arrow.def

;; anonymous default exports
(export_statement
  value: (function_expression) @default.body) @default.def
(export_statement
  value: (generator_function) @default.body) @default.def
(export_statement
  value: (class) @default.body) @default.def

;; class declarations
(class_declaration
  name: (identifier) @class.name) @class.def

;; method definitions inside classes
(class_declaration
  name: (identifier) @method.class_name
  body: (class_body
    (method_definition
      name: (property_identifier) @method.name) @method.def))

;; top-level const/let variable declarations
(lexical_declaration
  (variable_declarator
    name: (identifier) @var.name) @var.decl) @var.def

;; export statement wrappers (top-level only)
(export_statement) @export.stmt
"#;

const PY_QUERY: &str = r#"
;; function definitions (top-level and nested)
(function_definition
  name: (identifier) @fn.name) @fn.def

;; class definitions
(class_definition
  name: (identifier) @class.name) @class.def

;; decorated definitions (wraps function_definition or class_definition)
(decorated_definition
  (decorator) @dec.decorator) @dec.def
"#;

#[cfg(test)]
const RS_QUERY: &str = r#"
;; free functions (with optional visibility)
(function_item
  name: (identifier) @fn.name) @fn.def

;; struct items
(struct_item
  name: (type_identifier) @struct.name) @struct.def

;; enum items
(enum_item
  name: (type_identifier) @enum.name) @enum.def

;; trait items
(trait_item
  name: (type_identifier) @trait.name) @trait.def

;; impl blocks — capture the whole block to find methods
(impl_item) @impl.def

;; visibility modifiers on any item
(visibility_modifier) @vis.mod
"#;

const GO_QUERY: &str = r#"
;; function declarations
(function_declaration
  name: (identifier) @fn.name) @fn.def

;; method declarations (with receiver)
(method_declaration
  name: (field_identifier) @method.name) @method.def

;; type declarations (struct and interface)
(type_declaration
  (type_spec
    name: (type_identifier) @type.name
    type: (_) @type.body)) @type.def
"#;

const C_QUERY: &str = r#"
;; function definitions
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @fn.name)) @fn.def

;; function declarations / prototypes
(declaration
  declarator: (function_declarator
    declarator: (identifier) @fn.name)) @fn.def

;; struct declarations
(struct_specifier
  name: (type_identifier) @struct.name
  body: (field_declaration_list)) @struct.def

;; enum declarations
(enum_specifier
  name: (type_identifier) @enum.name
  body: (enumerator_list)) @enum.def

;; typedef aliases
(type_definition
  declarator: (type_identifier) @type.name) @type.def

;; macros
(preproc_def
  name: (identifier) @macro.name) @macro.def

(preproc_function_def
  name: (identifier) @macro.name) @macro.def
"#;

const CPP_QUERY: &str = r#"
;; free function definitions
(function_definition
  declarator: (function_declarator
    declarator: (identifier) @fn.name)) @fn.def

;; free function declarations
(declaration
  declarator: (function_declarator
    declarator: (identifier) @fn.name)) @fn.def

;; inline method definitions / declarations inside class bodies
(function_definition
  declarator: (function_declarator
    declarator: (field_identifier) @method.name)) @method.def

(field_declaration
  declarator: (function_declarator
    declarator: (field_identifier) @method.name)) @method.def

;; qualified functions / methods
(function_definition
  declarator: (function_declarator
    declarator: (qualified_identifier
      scope: (_) @qual.scope
      name: (identifier) @qual.name))) @qual.def

(declaration
  declarator: (function_declarator
    declarator: (qualified_identifier
      scope: (_) @qual.scope
      name: (identifier) @qual.name))) @qual.def

;; class / struct / enum / namespace declarations
(class_specifier
  name: (_) @class.name) @class.def

(struct_specifier
  name: (_) @struct.name) @struct.def

(enum_specifier
  name: (_) @enum.name) @enum.def

(namespace_definition
  name: (_) @namespace.name) @namespace.def

;; template declarations
(template_declaration
  (class_specifier
    name: (_) @template.class.name) @template.class.item) @template.class.def

(template_declaration
  (struct_specifier
    name: (_) @template.struct.name) @template.struct.item) @template.struct.def

(template_declaration
  (function_definition
    declarator: (function_declarator
      declarator: (identifier) @template.fn.name)) @template.fn.item) @template.fn.def

(template_declaration
  (function_definition
    declarator: (function_declarator
      declarator: (qualified_identifier
        scope: (_) @template.qual.scope
        name: (identifier) @template.qual.name))) @template.qual.item) @template.qual.def
"#;

const ZIG_QUERY: &str = r#"
;; functions
(function_declaration
  name: (identifier) @fn.name) @fn.def

;; container declarations bound to const names
(variable_declaration
  (identifier) @struct.name
  "="
  (struct_declaration) @struct.body) @struct.def

(variable_declaration
  (identifier) @enum.name
  "="
  (enum_declaration) @enum.body) @enum.def

(variable_declaration
  (identifier) @union.name
  "="
  (union_declaration) @union.body) @union.def

;; const declarations
(variable_declaration
  (identifier) @const.name) @const.def

;; tests
(test_declaration
  (string) @test.name) @test.def

(test_declaration
  (identifier) @test.name) @test.def
"#;

const CSHARP_QUERY: &str = r#"
;; types
(class_declaration
  name: (identifier) @class.name) @class.def

(interface_declaration
  name: (identifier) @interface.name) @interface.def

(struct_declaration
  name: (identifier) @struct.name) @struct.def

(enum_declaration
  name: (identifier) @enum.name) @enum.def

;; members
(method_declaration
  name: (identifier) @method.name) @method.def

(property_declaration
  name: (identifier) @property.name) @property.def

;; namespaces
(namespace_declaration
  name: (_) @namespace.name) @namespace.def

(file_scoped_namespace_declaration
  name: (_) @namespace.name) @namespace.def
"#;

// --- Bash query ---

const BASH_QUERY: &str = r#"
;; function definitions (both `function foo()` and `foo()` styles)
(function_definition
  name: (word) @fn.name) @fn.def
"#;

// --- Solidity query ---

const SOL_QUERY: &str = r#"
;; contracts / libraries / interfaces
(contract_declaration
  name: (identifier) @contract.name) @contract.def

(library_declaration
  name: (identifier) @library.name) @library.def

(interface_declaration
  name: (identifier) @interface.name) @interface.def

;; functions, modifiers, constructors
(function_definition
  name: (identifier) @fn.name) @fn.def

(modifier_definition
  name: (identifier) @modifier.name) @modifier.def

(constructor_definition) @constructor.def

(fallback_receive_definition) @fallback_receive.def

;; events / errors
(event_definition
  name: (identifier) @event.name) @event.def

(error_declaration
  name: (identifier) @error.name) @error.def

;; data types
(struct_declaration
  name: (identifier) @struct.name) @struct.def

(enum_declaration
  name: (identifier) @enum.name) @enum.def

;; state variables (top-level inside a contract)
(state_variable_declaration
  name: (identifier) @var.name) @var.def
"#;

const PASCAL_QUERY: &str = r#"
;; program / unit
(program (moduleName (identifier) @program.name)) @program.def
(unit (moduleName (identifier) @unit.name)) @unit.def

;; type declarations
(declType (identifier) @type.name) @type.def

;; const / var declarations
(declConst (identifier) @const.name) @const.def
(declVar (identifier) @var.name) @var.def

;; procedure / function definitions (implementation)
(defProc
  (declProc
    [
      (identifier) @proc.name
      (genericDot) @proc.name
    ])) @proc.def

;; procedure / function declarations (interface / class)
(declProc
  [
    (identifier) @proc.name
    (genericDot) @proc.name
  ]) @proc.def
"#;

const R_QUERY: &str = r#"
;; R represents assignments as binary operators. The extractor filters these
;; broad captures down to top-level assignment operators and classifies
;; function-valued assignments as functions. Rightward function
;; assignment parses as a function_definition whose body is a binary_operator.
(binary_operator) @assign.def
(function_definition) @function.def
"#;

const GROOVY_QUERY: &str = r#"
;; types
(class_declaration
  name: (identifier) @class.name) @class.def
(interface_declaration
  name: (identifier) @interface.name) @interface.def
(trait_declaration
  name: (identifier) @trait.name) @trait.def
(enum_declaration
  name: (identifier) @enum.name) @enum.def

;; methods and top-level script functions
(method_declaration
  name: [(identifier) (quoted_identifier)] @fn.name) @fn.def

;; fields and properties
(field_declaration
  (variable_declarator
    name: (identifier) @var.name)) @var.def

;; Jenkins declarative pipeline root
(pipeline_statement) @pipeline.def
"#;

const OBJC_QUERY: &str = r#"
;; Objective-C class and protocol containers. The extractor derives names from
;; the first identifier child because the grammar does not field-name them.
(class_interface) @class.def
(class_implementation) @class.def
(protocol_declaration) @interface.def

;; Method bodies, property declarations, C functions, and typedefs in .m/.mm files.
(method_definition) @method.def
(property_declaration) @property.def
(function_definition) @fn.def
(type_definition) @type.def
"#;

const SCSS_QUERY: &str = r#"
;; SCSS definitions
(mixin_statement
  name: (identifier) @mixin.name) @mixin.def

(function_statement
  name: (identifier) @fn.name) @fn.def

(declaration
  (property_name) @var.name) @var.def

(rule_set
  (selectors) @selector.name) @selector.def
"#;

const SCALA_QUERY: &str = r#"
;; classes / objects / traits
(class_definition
  name: (identifier) @class.name) @class.def
(object_definition
  name: (identifier) @object.name) @object.def
(enum_definition
  name: (_) @enum.name) @enum.def
(trait_definition
  name: (identifier) @trait.name) @trait.def
;; methods (def)
(function_definition
  name: (identifier) @fn.name) @fn.def
(function_declaration
  name: (identifier) @fn.name) @fn.def
;; vals / vars / type aliases
(val_definition
  pattern: (identifier) @val.name) @val.def
(var_definition
  pattern: (identifier) @var.name) @var.def
(given_definition
  name: (_) @given.name) @given.def
(type_definition
  name: (type_identifier) @type.name) @type.def
"#;

const JAVA_QUERY: &str = r#"
;; types
(class_declaration
  name: (identifier) @class.name) @class.def
(interface_declaration
  name: (identifier) @interface.name) @interface.def
(annotation_type_declaration
  name: (identifier) @interface.name) @interface.def
(enum_declaration
  name: (identifier) @enum.name) @enum.def
(record_declaration
  name: (identifier) @struct.name) @struct.def

;; members
(method_declaration
  name: (identifier) @fn.name) @fn.def
(constructor_declaration
  name: (identifier) @fn.name) @fn.def
(field_declaration
  declarator: (variable_declarator
    name: (identifier) @var.name)) @var.def
"#;

const RUBY_QUERY: &str = r#"
;; modules / classes
(module
  name: (constant) @module.name) @module.def
(class
  name: (constant) @class.name) @class.def

;; methods
(method
  name: (_) @fn.name) @fn.def
(singleton_method
  name: (_) @fn.name) @fn.def

;; constants
(assignment
  left: (constant) @var.name) @var.def
"#;

const KOTLIN_QUERY: &str = r#"
;; declarations
(class_declaration
  (type_identifier) @class.name) @class.def
(object_declaration
  (type_identifier) @object.name) @object.def
(function_declaration
  (simple_identifier) @fn.name) @fn.def
(property_declaration
  (variable_declaration
    (simple_identifier) @var.name)) @var.def
(type_alias
  (type_identifier) @type.name) @type.def
"#;

const SWIFT_QUERY: &str = r#"
;; types
(class_declaration
  name: (type_identifier) @class.name) @class.def
(protocol_declaration
  name: (type_identifier) @interface.name) @interface.def

;; functions and members
(function_declaration
  name: (simple_identifier) @fn.name) @fn.def
(protocol_function_declaration
  name: (simple_identifier) @fn.name) @fn.def
(property_declaration
  name: (pattern
    bound_identifier: (simple_identifier) @var.name)) @var.def
(typealias_declaration
  name: (type_identifier) @type.name) @type.def
"#;

const PHP_QUERY: &str = r#"
;; namespaces and types
(namespace_definition
  name: (namespace_name) @namespace.name) @namespace.def
(class_declaration
  name: (name) @class.name) @class.def
(interface_declaration
  name: (name) @interface.name) @interface.def
(trait_declaration
  name: (name) @trait.name) @trait.def
(enum_declaration
  name: (name) @enum.name) @enum.def

;; functions and members
(function_definition
  name: (name) @fn.name) @fn.def
(method_declaration
  name: (name) @fn.name) @fn.def
(property_declaration
  (property_element
    name: (variable_name (name) @var.name))) @var.def
"#;

const LUA_QUERY: &str = r#"
;; functions
(function_declaration
  name: (identifier) @fn.name) @fn.def
(function_declaration
  name: (dot_index_expression
    field: (identifier) @fn.name)) @fn.def
(function_declaration
  name: (method_index_expression
    method: (identifier) @fn.name)) @fn.def

;; locals / module tables
(variable_declaration
  (assignment_statement
    (variable_list
      name: (identifier) @var.name))) @var.def
(variable_declaration
  (assignment_statement
    (variable_list
      name: (variable) @var.name))) @var.def
(variable_declaration
  (variable_list
    name: (identifier) @var.name)) @var.def
(variable_declaration
  (variable_list
    name: (variable) @var.name)) @var.def
"#;

const PERL_QUERY: &str = r#"
;; packages and subroutines
(package_statement
  name: (package) @package.name) @package.def
(subroutine_declaration_statement
  name: (bareword) @fn.name) @fn.def
(method_declaration_statement
  name: (bareword) @fn.name) @fn.def

;; constants: `use constant NAME => ...;` — the `constant` pragma is filtered in
;; Rust (the tree-sitter binding does not evaluate `#eq?` predicates).
(use_statement
  module: (package) @const.pragma
  (list_expression (autoquoted_bareword) @const.name)) @const.def

;; lexical / package variables: capture the sigil-bearing variable node so the
;; symbol name matches `$counter`, not the bare `counter`.
(variable_declaration
  variable: (_) @var.name) @var.def
"#;

/// Supported language identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LangId {
    TypeScript,
    Tsx,
    JavaScript,
    Python,
    Rust,
    Go,
    C,
    Cpp,
    Zig,
    CSharp,
    Bash,
    Html,
    Markdown,
    Solidity,
    Scss,
    Vue,
    Json,
    Scala,
    Java,
    Ruby,
    Kotlin,
    Swift,
    Php,
    Lua,
    Perl,
    Yaml,
    Pascal,
    R,
    Groovy,
    ObjC,
}

/// Maps file extension to language identifier.
pub fn detect_language(path: &Path) -> Option<LangId> {
    if path.file_name().and_then(|name| name.to_str()) == Some("Jenkinsfile") {
        return Some(LangId::Groovy);
    }

    let ext = path.extension()?.to_str()?;
    match ext {
        "ts" | "mts" | "cts" => Some(LangId::TypeScript),
        "tsx" => Some(LangId::Tsx),
        "js" | "jsx" | "mjs" | "cjs" => Some(LangId::JavaScript),
        "py" | "pyi" => Some(LangId::Python),
        "rs" => Some(LangId::Rust),
        "go" => Some(LangId::Go),
        "c" | "h" => Some(LangId::C),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => Some(LangId::Cpp),
        "zig" => Some(LangId::Zig),
        "cs" => Some(LangId::CSharp),
        "sh" | "bash" | "zsh" => Some(LangId::Bash),
        "html" | "htm" => Some(LangId::Html),
        "md" | "markdown" | "mdx" | "qmd" | "Qmd" | "rmd" | "Rmd" => Some(LangId::Markdown),
        "sol" => Some(LangId::Solidity),
        "scss" => Some(LangId::Scss),
        "vue" => Some(LangId::Vue),
        "json" | "jsonc" => Some(LangId::Json),
        "scala" | "sc" => Some(LangId::Scala),
        "java" => Some(LangId::Java),
        "rb" => Some(LangId::Ruby),
        "kt" | "kts" => Some(LangId::Kotlin),
        "swift" => Some(LangId::Swift),
        "inc" | "php" => Some(LangId::Php),
        "lua" => Some(LangId::Lua),
        "pl" | "pm" | "t" => Some(LangId::Perl),
        "yaml" | "yml" => Some(LangId::Yaml),
        "pas" | "pp" | "dpr" | "dpk" | "lpr" => Some(LangId::Pascal),
        "R" | "r" => Some(LangId::R),
        "groovy" | "gvy" | "gy" | "gsh" | "gradle" => Some(LangId::Groovy),
        "m" | "mm" => Some(LangId::ObjC),
        _ => None,
    }
}

/// Returns the tree-sitter Language grammar for a given LangId.
pub fn grammar_for(lang: LangId) -> Language {
    match lang {
        LangId::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        LangId::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        LangId::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        LangId::Python => tree_sitter_python::LANGUAGE.into(),
        LangId::Rust => tree_sitter_rust::LANGUAGE.into(),
        LangId::Go => tree_sitter_go::LANGUAGE.into(),
        LangId::C => tree_sitter_c::LANGUAGE.into(),
        LangId::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        LangId::Zig => tree_sitter_zig::LANGUAGE.into(),
        LangId::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
        LangId::Bash => tree_sitter_bash::LANGUAGE.into(),
        LangId::Html => tree_sitter_html::LANGUAGE.into(),
        LangId::Markdown => tree_sitter_md::LANGUAGE.into(),
        LangId::Solidity => tree_sitter_solidity::LANGUAGE.into(),
        LangId::Scss => tree_sitter_scss::language(),
        LangId::Vue => tree_sitter_vue::LANGUAGE.into(),
        LangId::Json => tree_sitter_json::LANGUAGE.into(),
        LangId::Scala => tree_sitter_scala::LANGUAGE.into(),
        LangId::Java => tree_sitter_java::LANGUAGE.into(),
        LangId::Ruby => tree_sitter_ruby::LANGUAGE.into(),
        LangId::Kotlin => tree_sitter_kotlin_sg::LANGUAGE.into(),
        LangId::Swift => tree_sitter_swift::LANGUAGE.into(),
        LangId::Php => tree_sitter_php::LANGUAGE_PHP.into(),
        LangId::Lua => tree_sitter_lua::LANGUAGE.into(),
        LangId::Perl => tree_sitter_perl::LANGUAGE.into(),
        LangId::Yaml => tree_sitter_yaml::LANGUAGE.into(),
        LangId::Pascal => tree_sitter_pascal::LANGUAGE.into(),
        LangId::R => tree_sitter_r::LANGUAGE.into(),
        LangId::Groovy => dekobon_tree_sitter_groovy::LANGUAGE.into(),
        LangId::ObjC => tree_sitter_objc::LANGUAGE.into(),
    }
}

/// Returns the query pattern string for a given LangId, if implemented.
fn query_for(lang: LangId) -> Option<&'static str> {
    match lang {
        LangId::TypeScript | LangId::Tsx => Some(TS_QUERY),
        LangId::JavaScript => Some(JS_QUERY),
        LangId::Python => Some(PY_QUERY),
        LangId::Rust => None,
        LangId::Go => Some(GO_QUERY),
        LangId::C => Some(C_QUERY),
        LangId::Cpp => Some(CPP_QUERY),
        LangId::Zig => Some(ZIG_QUERY),
        LangId::CSharp => Some(CSHARP_QUERY),
        LangId::Bash => Some(BASH_QUERY),
        LangId::Html => None, // HTML uses direct tree walking like Markdown
        LangId::Markdown => None,
        LangId::Solidity => Some(SOL_QUERY),
        LangId::Scss => Some(SCSS_QUERY),
        LangId::Vue => None,
        LangId::Json => None,
        LangId::Scala => Some(SCALA_QUERY),
        LangId::Java => Some(JAVA_QUERY),
        LangId::Ruby => Some(RUBY_QUERY),
        LangId::Kotlin => Some(KOTLIN_QUERY),
        LangId::Swift => Some(SWIFT_QUERY),
        LangId::Php => Some(PHP_QUERY),
        LangId::Lua => Some(LUA_QUERY),
        LangId::Perl => Some(PERL_QUERY),
        LangId::Yaml => None, // YAML uses direct tree walking like JSON
        LangId::Pascal => Some(PASCAL_QUERY),
        LangId::R => Some(R_QUERY),
        LangId::Groovy => Some(GROOVY_QUERY),
        LangId::ObjC => Some(OBJC_QUERY),
    }
}

static TS_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::TypeScript));
static TSX_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Tsx));
static JS_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::JavaScript));
static PY_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Python));
static GO_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Go));
static C_QUERY_CACHE: LazyLock<Result<Query, String>> = LazyLock::new(|| compile_query(LangId::C));
static CPP_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Cpp));
static ZIG_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Zig));
static CSHARP_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::CSharp));
static BASH_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Bash));
static SOL_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Solidity));
static SCSS_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Scss));
static SCALA_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Scala));
static JAVA_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Java));
static RUBY_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Ruby));
static KOTLIN_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Kotlin));
static SWIFT_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Swift));
static PHP_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Php));
static LUA_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Lua));
static PERL_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Perl));
static PASCAL_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Pascal));
static R_QUERY_CACHE: LazyLock<Result<Query, String>> = LazyLock::new(|| compile_query(LangId::R));
static GROOVY_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Groovy));
static OBJC_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::ObjC));

fn compile_query(lang: LangId) -> Result<Query, String> {
    let query_src = query_for(lang).ok_or_else(|| format!("missing query for {lang:?}"))?;
    let grammar = grammar_for(lang);
    Query::new(&grammar, query_src)
        .map_err(|error| format!("query compile error for {lang:?}: {error}"))
}

fn cached_query_for(lang: LangId) -> Result<Option<&'static Query>, AftError> {
    let query = match lang {
        LangId::TypeScript => Some(&*TS_QUERY_CACHE),
        LangId::Tsx => Some(&*TSX_QUERY_CACHE),
        LangId::JavaScript => Some(&*JS_QUERY_CACHE),
        LangId::Python => Some(&*PY_QUERY_CACHE),
        LangId::Go => Some(&*GO_QUERY_CACHE),
        LangId::C => Some(&*C_QUERY_CACHE),
        LangId::Cpp => Some(&*CPP_QUERY_CACHE),
        LangId::Zig => Some(&*ZIG_QUERY_CACHE),
        LangId::CSharp => Some(&*CSHARP_QUERY_CACHE),
        LangId::Bash => Some(&*BASH_QUERY_CACHE),
        LangId::Solidity => Some(&*SOL_QUERY_CACHE),
        LangId::Scss => Some(&*SCSS_QUERY_CACHE),
        LangId::Scala => Some(&*SCALA_QUERY_CACHE),
        LangId::Java => Some(&*JAVA_QUERY_CACHE),
        LangId::Ruby => Some(&*RUBY_QUERY_CACHE),
        LangId::Kotlin => Some(&*KOTLIN_QUERY_CACHE),
        LangId::Swift => Some(&*SWIFT_QUERY_CACHE),
        LangId::Php => Some(&*PHP_QUERY_CACHE),
        LangId::Lua => Some(&*LUA_QUERY_CACHE),
        LangId::Perl => Some(&*PERL_QUERY_CACHE),
        LangId::Pascal => Some(&*PASCAL_QUERY_CACHE),
        LangId::R => Some(&*R_QUERY_CACHE),
        LangId::Groovy => Some(&*GROOVY_QUERY_CACHE),
        LangId::ObjC => Some(&*OBJC_QUERY_CACHE),
        LangId::Rust
        | LangId::Html
        | LangId::Markdown
        | LangId::Vue
        | LangId::Json
        | LangId::Yaml => None,
    };

    query
        .map(|result| {
            result.as_ref().map_err(|message| AftError::ParseError {
                message: message.clone(),
            })
        })
        .transpose()
}

thread_local! {
    // A Parser is mutable and not Sync, so each semantic/rayon worker retains its
    // own language parsers without a global lock or cross-thread sharing.
    static REUSABLE_PARSERS: RefCell<HashMap<LangId, Parser>> = RefCell::new(HashMap::new());
}

/// Parse source with a parser retained by the current worker thread.
pub(crate) fn parse_source_with_cached_parser(
    path: &Path,
    source: &str,
    lang: LangId,
) -> Result<Tree, AftError> {
    REUSABLE_PARSERS.with(|parsers| {
        let mut parsers = parsers.borrow_mut();
        if let std::collections::hash_map::Entry::Vacant(entry) = parsers.entry(lang) {
            let grammar = grammar_for(lang);
            let mut parser = Parser::new();
            parser.set_language(&grammar).map_err(|error| {
                crate::slog_error!("grammar init failed for {:?}: {}", lang, error);
                AftError::ParseError {
                    message: format!("grammar init failed for {:?}: {}", lang, error),
                }
            })?;
            entry.insert(parser);
        }

        parsers
            .get_mut(&lang)
            .expect("parser inserted for language")
            .parse(source, None)
            .ok_or_else(|| AftError::ParseError {
                message: format!("tree-sitter parse returned None for {}", path.display()),
            })
    })
}

/// Cached parse result: mtime at parse time + the tree.
struct CachedTree {
    mtime: SystemTime,
    size: u64,
    content_hash: blake3::Hash,
    tree: Tree,
}

/// Cached symbol extraction result: mtime at extraction time + symbols.
#[derive(Clone, PartialEq, Eq)]
struct CachedSymbols {
    mtime: SystemTime,
    size: u64,
    content_hash: blake3::Hash,
    symbols: Vec<Symbol>,
}

fn content_hash_for_source(source: &str) -> blake3::Hash {
    if source.len() as u64 > cache_freshness::CONTENT_HASH_SIZE_CAP {
        cache_freshness::zero_hash()
    } else {
        cache_freshness::hash_bytes(source.as_bytes())
    }
}

fn cached_file_is_fresh(
    path: &Path,
    cached_mtime: SystemTime,
    cached_size: u64,
    cached_content_hash: blake3::Hash,
    fallback_mtime: SystemTime,
) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    let current_size = metadata.len();
    if current_size != cached_size {
        return false;
    }

    let current_mtime = metadata.modified().unwrap_or(fallback_mtime);
    // Matching size and mtime is the steady-state fast path. If only mtime moved,
    // hash below to keep touched or no-op rewritten files cached when content is identical.
    if current_mtime == cached_mtime {
        return true;
    }
    if current_size > cache_freshness::CONTENT_HASH_SIZE_CAP {
        return false;
    }

    matches!(
        cache_freshness::hash_file_if_small(path, current_size),
        Ok(Some(hash)) if hash == cached_content_hash
    )
}

/// Shared symbol cache that can be pre-warmed in a background thread
/// and read by all parser instances.
#[derive(Clone, Default)]
pub struct SymbolCache {
    entries: HashMap<PathBuf, CachedSymbols>,
    generation: u64,
    // Revisions distinguish real entry changes from successful parses that reuse cached data.
    mutation_revision: u64,
    persisted_revision: u64,
    project_root: Option<PathBuf>,
}

pub type SharedSymbolCache = Arc<RwLock<SymbolCache>>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct SymbolCacheLoadOutcome {
    pub(crate) loaded: usize,
    pub(crate) needs_persistence: bool,
}

fn symbols_estimated_bytes(symbols: &[Symbol]) -> u64 {
    symbols.iter().fold(0u64, |bytes, symbol| {
        let scope_bytes = symbol.scope_chain.iter().fold(0u64, |scope_bytes, scope| {
            scope_bytes
                .saturating_add(std::mem::size_of::<String>() as u64)
                .saturating_add(crate::memory::usize_to_u64(scope.len()))
        });
        bytes
            .saturating_add(std::mem::size_of::<Symbol>() as u64)
            .saturating_add(crate::memory::usize_to_u64(symbol.name.len()))
            .saturating_add(
                symbol
                    .signature
                    .as_ref()
                    .map(|signature| crate::memory::usize_to_u64(signature.len()))
                    .unwrap_or(0),
            )
            .saturating_add(scope_bytes)
            .saturating_add(
                symbol
                    .parent
                    .as_ref()
                    .map(|parent| crate::memory::usize_to_u64(parent.len()))
                    .unwrap_or(0),
            )
    })
}

impl SymbolCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            generation: 0,
            mutation_revision: 0,
            persisted_revision: 0,
            project_root: None,
        }
    }

    /// Set the project root used for disk persistence.
    pub fn set_project_root(&mut self, project_root: PathBuf) {
        debug_assert!(project_root.is_absolute());
        self.project_root = Some(project_root);
    }

    /// Set the project root only when the caller still belongs to the active
    /// cache generation.
    pub fn set_project_root_for_generation(
        &mut self,
        generation: u64,
        project_root: PathBuf,
    ) -> bool {
        if self.generation != generation {
            return false;
        }
        self.set_project_root(project_root);
        true
    }

    /// Insert pre-warmed symbols for a file.
    pub fn insert(
        &mut self,
        path: PathBuf,
        mtime: SystemTime,
        size: u64,
        content_hash: blake3::Hash,
        symbols: Vec<Symbol>,
    ) -> bool {
        let entry = CachedSymbols {
            mtime,
            size,
            content_hash,
            symbols,
        };
        if self.entries.get(&path) == Some(&entry) {
            return false;
        }
        self.entries.insert(path, entry);
        self.mutation_revision = self.mutation_revision.wrapping_add(1);
        true
    }

    /// Insert symbols only when the caller still belongs to the active cache generation.
    pub fn insert_for_generation(
        &mut self,
        generation: u64,
        path: PathBuf,
        mtime: SystemTime,
        size: u64,
        content_hash: blake3::Hash,
        symbols: Vec<Symbol>,
    ) -> bool {
        if self.generation != generation {
            return false;
        }
        self.insert(path, mtime, size, content_hash, symbols)
    }

    /// Return cached symbols when the source file is still fresh.
    pub fn get(&self, path: &Path, mtime: SystemTime) -> Option<Vec<Symbol>> {
        self.entries.get(path).and_then(|cached| {
            cached_file_is_fresh(path, cached.mtime, cached.size, cached.content_hash, mtime)
                .then(|| cached.symbols.clone())
        })
    }

    /// Return a cached symbol count when file metadata exactly matches the cache entry.
    ///
    /// This is the fast path for directory file-tree summaries: when mtime and
    /// size are unchanged, callers can use the count without cloning symbols or
    /// re-reading the file to verify a content hash.
    pub fn symbol_count_if_metadata_matches(
        &self,
        path: &Path,
        mtime: SystemTime,
        size: u64,
    ) -> Option<usize> {
        self.entries.get(path).and_then(|cached| {
            (cached.mtime == mtime && cached.size == size).then_some(cached.symbols.len())
        })
    }

    /// Whether the cache has a still-valid entry for the given file mtime.
    pub fn contains_path_with_mtime(&self, path: &Path, mtime: SystemTime) -> bool {
        self.entries
            .get(path)
            .is_some_and(|cached| cached.mtime == mtime)
    }

    /// Load valid symbol entries from disk, dropping only entries whose source file changed.
    pub fn load_from_disk(
        &mut self,
        storage_dir: &Path,
        project_key: &str,
        current_root: &Path,
    ) -> usize {
        self.load_from_disk_with_outcome(storage_dir, project_key, current_root)
            .loaded
    }

    fn load_from_disk_with_outcome(
        &mut self,
        storage_dir: &Path,
        project_key: &str,
        current_root: &Path,
    ) -> SymbolCacheLoadOutcome {
        debug_assert!(current_root.is_absolute());
        let Some(cache) = symbol_cache_disk::read_from_disk(storage_dir, project_key) else {
            return SymbolCacheLoadOutcome::default();
        };

        self.project_root = Some(current_root.to_path_buf());
        self.entries.clear();
        let mut outcome = SymbolCacheLoadOutcome::default();

        for entry in cache.entries {
            let Some(path) =
                crate::search_index::cached_path_under_root(current_root, &entry.relative_path)
            else {
                outcome.needs_persistence = true;
                continue;
            };
            let cached_freshness = FileFreshness {
                mtime: entry.mtime,
                size: entry.size,
                content_hash: entry.content_hash,
            };
            let mtime = match cache_freshness::verify_file(&path, &cached_freshness) {
                FreshnessVerdict::HotFresh => entry.mtime,
                FreshnessVerdict::ContentFresh { new_mtime, .. } => {
                    outcome.needs_persistence = true;
                    new_mtime
                }
                FreshnessVerdict::Stale | FreshnessVerdict::Deleted => {
                    outcome.needs_persistence = true;
                    continue;
                }
            };

            self.entries.insert(
                path,
                CachedSymbols {
                    mtime,
                    size: entry.size,
                    content_hash: entry.content_hash,
                    symbols: entry.symbols,
                },
            );
            outcome.loaded += 1;
        }

        self.mutation_revision = if outcome.needs_persistence { 1 } else { 0 };
        self.persisted_revision = 0;
        outcome
    }

    /// Load valid symbol entries from disk only when the caller still belongs
    /// to the active cache generation.
    pub fn load_from_disk_for_generation(
        &mut self,
        generation: u64,
        storage_dir: &Path,
        project_key: &str,
        current_root: &Path,
    ) -> usize {
        self.load_from_disk_for_generation_with_outcome(
            generation,
            storage_dir,
            project_key,
            current_root,
        )
        .loaded
    }

    pub(crate) fn load_from_disk_for_generation_with_outcome(
        &mut self,
        generation: u64,
        storage_dir: &Path,
        project_key: &str,
        current_root: &Path,
    ) -> SymbolCacheLoadOutcome {
        if self.generation != generation {
            return SymbolCacheLoadOutcome::default();
        }
        self.load_from_disk_with_outcome(storage_dir, project_key, current_root)
    }

    /// Invalidate cached symbols for a specific file.
    pub fn invalidate(&mut self, path: &Path) {
        if self.entries.remove(path).is_some() {
            self.mutation_revision = self.mutation_revision.wrapping_add(1);
        }
    }

    /// Clear all entries and advance the generation to ignore stale background writers.
    pub fn reset(&mut self) -> u64 {
        self.entries.clear();
        self.project_root = None;
        self.generation = self.generation.wrapping_add(1);
        self.mutation_revision = 0;
        self.persisted_revision = 0;
        self.generation
    }

    /// Current generation token.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn needs_persistence(&self) -> bool {
        self.mutation_revision != self.persisted_revision
    }

    pub(crate) fn persistence_revision(&self) -> u64 {
        self.mutation_revision
    }

    pub(crate) fn mark_persisted_for_generation(
        &mut self,
        generation: u64,
        persisted_revision: u64,
    ) -> bool {
        if self.generation != generation || self.mutation_revision != persisted_revision {
            return false;
        }
        self.persisted_revision = persisted_revision;
        true
    }

    /// Whether the cache has an entry for a file.
    pub fn contains_key(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Estimate the resident hot symbol map from cached paths and the strings
    /// owned by each symbol. Tree-sitter parse trees are request-local and are
    /// therefore not part of this long-lived cache estimate.
    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        if self.entries.is_empty() {
            return crate::memory::MemoryEstimate::estimated(0)
                .count("entries", 0)
                .count("symbols", 0);
        }
        let entry_bytes = self.entries.iter().fold(0u64, |bytes, (path, cached)| {
            bytes
                .saturating_add(std::mem::size_of::<PathBuf>() as u64)
                .saturating_add(crate::memory::path_bytes(path))
                .saturating_add(std::mem::size_of::<CachedSymbols>() as u64)
                .saturating_add(symbols_estimated_bytes(&cached.symbols))
        });
        let project_root_bytes = self
            .project_root
            .as_deref()
            .map(crate::memory::path_bytes)
            .unwrap_or(0);
        let symbol_count = self
            .entries
            .values()
            .map(|cached| cached.symbols.len())
            .fold(0usize, usize::saturating_add);
        crate::memory::MemoryEstimate::estimated(entry_bytes.saturating_add(project_root_bytes))
            .count("entries", self.entries.len())
            .count("symbols", symbol_count)
    }

    pub(crate) fn project_root(&self) -> Option<PathBuf> {
        self.project_root.clone()
    }

    pub(crate) fn disk_entries(
        &self,
    ) -> Vec<(&PathBuf, SystemTime, u64, blake3::Hash, &Vec<Symbol>)> {
        // Persist EVERY parsed entry, including files that legitimately parse to
        // zero symbols (e.g. a TS file with only imports, a config-shaped file).
        // Previously these were filtered out, so they were never written to disk;
        // on the next spawn `load_from_disk` couldn't reload them, the prewarm
        // skip-check (`contains_path_with_mtime`) missed, and they were re-parsed
        // on every startup forever (the "N new" churn in issue #86). An empty
        // entry serializes to `symbols: []` — a few bytes — and makes the prewarm
        // skip it. It also makes the "persisted symbol cache: N files" count
        // accurate (was overstated by the filtered-out empties).
        self.entries
            .iter()
            .map(|(path, cached)| {
                (
                    path,
                    cached.mtime,
                    cached.size,
                    cached.content_hash,
                    &cached.symbols,
                )
            })
            .collect()
    }
}

/// Core parsing engine. Handles language detection, parse tree caching,
/// symbol table caching, and query pattern execution via tree-sitter.
pub struct FileParser {
    cache: HashMap<PathBuf, CachedTree>,
    parsers: HashMap<LangId, Parser>,
    symbol_cache: SharedSymbolCache,
    symbol_cache_generation: Option<u64>,
}

impl FileParser {
    /// Create a new `FileParser` with an empty parse cache.
    pub fn new() -> Self {
        Self::with_symbol_cache(Arc::new(RwLock::new(SymbolCache::new())))
    }

    /// Create a new `FileParser` backed by a shared symbol cache.
    pub fn with_symbol_cache(symbol_cache: SharedSymbolCache) -> Self {
        Self::with_symbol_cache_generation(symbol_cache, None)
    }

    /// Create a new `FileParser` backed by a shared symbol cache generation.
    pub fn with_symbol_cache_generation(
        symbol_cache: SharedSymbolCache,
        symbol_cache_generation: Option<u64>,
    ) -> Self {
        Self {
            cache: HashMap::new(),
            parsers: HashMap::new(),
            symbol_cache,
            symbol_cache_generation,
        }
    }

    fn parser_for(&mut self, lang: LangId) -> Result<&mut Parser, AftError> {
        use std::collections::hash_map::Entry;

        match self.parsers.entry(lang) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => {
                let grammar = grammar_for(lang);
                let mut parser = Parser::new();
                parser.set_language(&grammar).map_err(|e| {
                    crate::slog_error!("grammar init failed for {:?}: {}", lang, e);
                    AftError::ParseError {
                        message: format!("grammar init failed for {:?}: {}", lang, e),
                    }
                })?;
                Ok(entry.insert(parser))
            }
        }
    }

    /// Number of entries in the shared symbol cache.
    pub fn symbol_cache_len(&self) -> usize {
        self.symbol_cache
            .read()
            .map(|cache| cache.len())
            .unwrap_or(0)
    }

    /// Shared symbol cache backing this parser.
    pub fn symbol_cache(&self) -> SharedSymbolCache {
        Arc::clone(&self.symbol_cache)
    }

    /// Parse a file, returning the tree and detected language. Uses cache if
    /// the file hasn't been modified since last parse.
    pub fn parse(&mut self, path: &Path) -> Result<(&Tree, LangId), AftError> {
        let lang = detect_language(path).ok_or_else(|| AftError::InvalidRequest {
            message: format!(
                "unsupported file extension: {}",
                path.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("<none>")
            ),
        })?;

        let canon = path.to_path_buf();
        let current_mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|e| AftError::FileNotFound {
                path: format!("{}: {}", path.display(), e),
            })?;

        // Check cache validity. Mtime alone is not enough: editors and tests can
        // restore timestamps after changing file contents, so small files fall
        // back to a Blake3 content hash when size does not prove staleness.
        let needs_reparse = match self.cache.get(&canon) {
            Some(cached) => !cached_file_is_fresh(
                path,
                cached.mtime,
                cached.size,
                cached.content_hash,
                current_mtime,
            ),
            None => true,
        };

        if needs_reparse {
            let source = std::fs::read_to_string(path).map_err(|e| AftError::FileNotFound {
                path: format!("{}: {}", path.display(), e),
            })?;

            let tree = self.parser_for(lang)?.parse(&source, None).ok_or_else(|| {
                crate::slog_error!("parse failed for {}", path.display());
                AftError::ParseError {
                    message: format!("tree-sitter parse returned None for {}", path.display()),
                }
            })?;

            self.cache.insert(
                canon.clone(),
                CachedTree {
                    mtime: current_mtime,
                    size: source.len() as u64,
                    content_hash: content_hash_for_source(&source),
                    tree,
                },
            );
        }

        let cached = self.cache.get(&canon).ok_or_else(|| AftError::ParseError {
            message: format!("parser cache missing entry for {}", path.display()),
        })?;
        Ok((&cached.tree, lang))
    }

    /// Like [`FileParser::parse`] but returns an owned `Tree` clone.
    ///
    /// Useful when the caller needs to hold the tree while also calling
    /// other mutable methods on this parser.
    pub fn parse_cloned(&mut self, path: &Path) -> Result<(Tree, LangId), AftError> {
        let (tree, lang) = self.parse(path)?;
        Ok((tree.clone(), lang))
    }

    /// Like [`FileParser::parse`] but reuses caller-provided file contents and
    /// metadata instead of reading + stat'ing the file again.
    ///
    /// `parse()` reads the file from disk to build the tree. A caller that has
    /// ALREADY read the source (e.g. [`FileParser::extract_symbols`], which
    /// reads it to hash + extract symbols) would otherwise pay a second
    /// `read_to_string` for the exact same bytes on every cold-cache file — a
    /// real cost when warming the symbol cache / call graph over a large repo.
    /// This variant takes the already-read `source` and the already-computed
    /// `current_mtime`/`size`/`content_hash` so the cold path does one read and
    /// one hash. Tree-cache freshness semantics are identical to `parse()`.
    fn parse_with_source(
        &mut self,
        path: &Path,
        source: &str,
        current_mtime: std::time::SystemTime,
        size: u64,
        content_hash: blake3::Hash,
    ) -> Result<(&Tree, LangId), AftError> {
        let lang = detect_language(path).ok_or_else(|| AftError::InvalidRequest {
            message: format!(
                "unsupported file extension: {}",
                path.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("<none>")
            ),
        })?;

        let canon = path.to_path_buf();
        let needs_reparse = match self.cache.get(&canon) {
            Some(cached) => !cached_file_is_fresh(
                path,
                cached.mtime,
                cached.size,
                cached.content_hash,
                current_mtime,
            ),
            None => true,
        };

        if needs_reparse {
            let tree = self.parser_for(lang)?.parse(source, None).ok_or_else(|| {
                crate::slog_error!("parse failed for {}", path.display());
                AftError::ParseError {
                    message: format!("tree-sitter parse returned None for {}", path.display()),
                }
            })?;

            self.cache.insert(
                canon.clone(),
                CachedTree {
                    mtime: current_mtime,
                    size,
                    content_hash,
                    tree,
                },
            );
        }

        let cached = self.cache.get(&canon).ok_or_else(|| AftError::ParseError {
            message: format!("parser cache missing entry for {}", path.display()),
        })?;
        Ok((&cached.tree, lang))
    }

    /// Extract symbols from a file using language-specific query patterns.
    /// Results are cached by `(path, mtime)` — subsequent calls for unchanged
    /// files return the cached symbol table without re-parsing.
    pub fn extract_symbols(&mut self, path: &Path) -> Result<Vec<Symbol>, AftError> {
        self.extract_symbols_with_cache_status(path)
            .map(|(symbols, _)| symbols)
    }

    pub(crate) fn extract_symbols_with_cache_status(
        &mut self,
        path: &Path,
    ) -> Result<(Vec<Symbol>, bool), AftError> {
        let canon = path.to_path_buf();
        let current_mtime = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|e| AftError::FileNotFound {
                path: format!("{}: {}", path.display(), e),
            })?;

        // Return cached symbols if file hasn't changed.
        if let Some(symbols) = self
            .symbol_cache
            .read()
            .map_err(|_| AftError::ParseError {
                message: "symbol cache lock poisoned".to_string(),
            })?
            .get(&canon, current_mtime)
        {
            return Ok((symbols, false));
        }

        let source = std::fs::read_to_string(path).map_err(|e| AftError::FileNotFound {
            path: format!("{}: {}", path.display(), e),
        })?;
        let size = source.len() as u64;
        let content_hash = content_hash_for_source(&source);

        let symbols = {
            // Reuse the source we just read instead of letting parse() read the
            // same file a second time.
            let (tree, lang) =
                self.parse_with_source(path, &source, current_mtime, size, content_hash)?;
            extract_symbols_from_tree(&source, tree, lang)?
        };

        let mut symbol_cache = self
            .symbol_cache
            .write()
            .map_err(|_| AftError::ParseError {
                message: "symbol cache lock poisoned".to_string(),
            })?;
        let cache_changed = if let Some(generation) = self.symbol_cache_generation {
            symbol_cache.insert_for_generation(
                generation,
                canon,
                current_mtime,
                size,
                content_hash,
                symbols.clone(),
            )
        } else {
            symbol_cache.insert(canon, current_mtime, size, content_hash, symbols.clone())
        };

        Ok((symbols, cache_changed))
    }

    /// Invalidate cached symbols for a specific file (e.g., after an edit).
    pub fn invalidate_symbols(&mut self, path: &Path) {
        if let Ok(mut symbol_cache) = self.symbol_cache.write() {
            symbol_cache.invalidate(path);
        }
        self.cache.remove(path);
    }
}

/// Extract symbols from an already-parsed tree without reparsing.
///
/// Callers that already have a `tree_sitter::Tree` (e.g. callgraph::build_file_data)
/// should use this instead of `list_symbols(path)` to avoid the redundant parse.
pub fn extract_symbols_from_tree(
    source: &str,
    tree: &Tree,
    lang: LangId,
) -> Result<Vec<Symbol>, AftError> {
    let root = tree.root_node();

    if lang == LangId::Rust {
        return extract_rs_symbols(source, &root);
    }
    if lang == LangId::Html {
        return extract_html_symbols(source, &root);
    }
    if lang == LangId::Markdown {
        return extract_md_symbols(source, &root);
    }
    if lang == LangId::Vue {
        return extract_vue_symbols(source, &root);
    }
    if lang == LangId::Json {
        return extract_json_symbols(source, &root);
    }
    if lang == LangId::Yaml {
        return extract_yaml_symbols(source, &root);
    }

    let query = cached_query_for(lang)?.ok_or_else(|| AftError::InvalidRequest {
        message: format!("no query patterns implemented for {:?} yet", lang),
    })?;

    match lang {
        LangId::TypeScript | LangId::Tsx => extract_ts_symbols(source, &root, query),
        LangId::JavaScript => extract_js_symbols(source, &root, query),
        LangId::Python => extract_py_symbols(source, &root, query),
        LangId::Go => extract_go_symbols(source, &root, query),
        LangId::C => extract_c_symbols(source, &root, query),
        LangId::Cpp => extract_cpp_symbols(source, &root, query),
        LangId::Zig => extract_zig_symbols(source, &root, query),
        LangId::CSharp => extract_csharp_symbols(source, &root, query),
        LangId::Bash => extract_bash_symbols(source, &root, query),
        LangId::Solidity => extract_solidity_symbols(source, &root, query),
        LangId::Scss => extract_scss_symbols(source, &root, query),
        LangId::Scala => extract_scala_symbols(source, &root, query),
        LangId::Java => extract_java_symbols(source, &root, query),
        LangId::Ruby => extract_ruby_symbols(source, &root, query),
        LangId::Kotlin => extract_kotlin_symbols(source, &root, query),
        LangId::Swift => extract_swift_symbols(source, &root, query),
        LangId::Php => extract_php_symbols(source, &root, query),
        LangId::Lua => extract_lua_symbols(source, &root, query),
        LangId::Perl => extract_perl_symbols(source, &root, query),
        LangId::Pascal => extract_pascal_symbols(source, &root, query),
        LangId::R => extract_r_symbols(source, &root, query),
        LangId::Groovy => extract_groovy_symbols(source, &root, query),
        LangId::ObjC => extract_objc_symbols(source, &root, query),
        LangId::Rust
        | LangId::Html
        | LangId::Markdown
        | LangId::Vue
        | LangId::Json
        | LangId::Yaml => unreachable!("handled before query lookup"),
    }
}

/// Build a Range from a tree-sitter Node.
pub(crate) fn node_range(node: &Node) -> Range {
    let start = node.start_position();
    let end = node.end_position();
    Range {
        start_line: start.row as u32,
        start_col: start.column as u32,
        end_line: end.row as u32,
        end_col: end.column as u32,
    }
}

/// Build a Range from a tree-sitter Node, expanding upward to include
/// preceding attributes, decorators, and doc comments that belong to the symbol.
///
/// This ensures that when agents edit/replace a symbol, they get the full
/// declaration including `#[test]`, `#[derive(...)]`, `/// doc`, `@decorator`, etc.
pub(crate) fn node_range_with_decorators(node: &Node, source: &str, lang: LangId) -> Range {
    if matches!(lang, LangId::Python) {
        if let Some(parent) = node.parent() {
            if parent.kind() == "decorated_definition" {
                return node_range(&parent);
            }
        }
    }

    // TypeScript / JavaScript / TSX: `export function foo() {}` parses as
    // `export_statement > function_declaration`. The function/class/etc.
    // node alone starts AFTER the `export ` keyword, so symbol replace would
    // produce a syntactically-broken `export export function foo() {}` if the
    // agent's replacement content includes its own `export` (it almost always
    // does, because they're replacing the *declaration*). Walk up to the
    // export_statement so the range covers `export ...`/`export default ...`.
    if matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
        if let Some(parent) = node.parent() {
            if parent.kind() == "export_statement" {
                return node_range_with_decorators_inner(&parent, source, lang);
            }
        }
    }

    node_range_with_decorators_inner(node, source, lang)
}

/// Inner walk that handles preceding-sibling expansion (decorators, doc comments).
/// Split from `node_range_with_decorators` so the export-statement parent path can
/// recurse without re-checking the export wrapper.
fn node_range_with_decorators_inner(node: &Node, source: &str, lang: LangId) -> Range {
    let mut range = node_range(node);

    let mut current = *node;
    while let Some(prev) = current.prev_sibling() {
        let kind = prev.kind();
        let should_include = match lang {
            LangId::Rust => {
                // Include #[...] attributes
                kind == "attribute_item"
                    // Include /// doc comments (but not regular // comments)
                    || (kind == "line_comment"
                        && node_text(source, &prev).starts_with("///"))
                    // Include /** ... */ doc comments
                    || (kind == "block_comment"
                        && node_text(source, &prev).starts_with("/**"))
            }
            LangId::TypeScript | LangId::Tsx | LangId::JavaScript => {
                // Include @decorator
                kind == "decorator"
                    // Include adjacent /** JSDoc */ comments, but do not let a
                    // blank line attach file-level docs to the following symbol.
                    || (kind == "comment"
                        && node_text(source, &prev).starts_with("/**")
                        && is_adjacent_line(&prev, &current, source))
            }
            LangId::Go
            | LangId::C
            | LangId::Cpp
            | LangId::ObjC
            | LangId::Zig
            | LangId::CSharp
            | LangId::Bash
            | LangId::Pascal => {
                // Include doc comments only if immediately above (no blank line gap)
                kind == "comment" && is_adjacent_line(&prev, &current, source)
            }
            LangId::Solidity
            | LangId::Scss
            | LangId::Scala
            | LangId::Java
            | LangId::Kotlin
            | LangId::Swift
            | LangId::Php
            | LangId::Groovy => {
                // Include `///` doc comments and `/** */` doc blocks if immediately above
                let text = node_text(source, &prev);
                (kind == "comment" || kind == "line_comment" || kind == "block_comment")
                    && (text.starts_with("///") || text.starts_with("/**"))
                    && is_adjacent_line(&prev, &current, source)
            }
            LangId::Ruby | LangId::Lua | LangId::R => {
                // Include adjacent `#`/`---` style comments used as documentation.
                let text = node_text(source, &prev);
                kind == "comment"
                    && (text.starts_with('#') || text.starts_with("---"))
                    && is_adjacent_line(&prev, &current, source)
            }
            LangId::Perl => false,
            LangId::Python => {
                // Decorators are handled by decorated_definition capture
                false
            }
            LangId::Html | LangId::Markdown | LangId::Vue | LangId::Json | LangId::Yaml => false,
        };

        if should_include {
            range.start_line = prev.start_position().row as u32;
            range.start_col = prev.start_position().column as u32;
            current = prev;
        } else {
            break;
        }
    }

    range
}

/// Check if two nodes are on adjacent lines (no blank line between them).
fn is_adjacent_line(upper: &Node, lower: &Node, source: &str) -> bool {
    let upper_end = upper.end_position().row;
    let lower_start = lower.start_position().row;

    if lower_start == 0 || lower_start <= upper_end {
        return true;
    }

    // Check that there's no blank line between them
    let lines: Vec<&str> = source.lines().collect();
    for row in (upper_end + 1)..lower_start {
        if row < lines.len() && lines[row].trim().is_empty() {
            return false;
        }
    }
    true
}

/// Extract the text of a node from source.
pub(crate) fn node_text<'a>(source: &'a str, node: &Node) -> &'a str {
    &source[node.byte_range()]
}

fn lexical_declaration_has_function_value(node: &Node) -> bool {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return false;
    }

    loop {
        let child = cursor.node();
        if matches!(
            child.kind(),
            "arrow_function" | "function_expression" | "generator_function"
        ) {
            return true;
        }

        if lexical_declaration_has_function_value(&child) {
            return true;
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    false
}

fn variable_declarator_has_function_value(node: &Node) -> bool {
    node.child_by_field_name("value").is_some_and(|value| {
        matches!(
            value.kind(),
            "arrow_function" | "function_expression" | "generator_function"
        )
    })
}

/// Collect byte ranges of all export_statement nodes from query matches.
fn collect_export_ranges(source: &str, root: &Node, query: &Query) -> Vec<std::ops::Range<usize>> {
    let export_idx = query
        .capture_names()
        .iter()
        .position(|n| *n == "export.stmt");
    let export_idx = match export_idx {
        Some(i) => i as u32,
        None => return vec![],
    };

    let mut cursor = QueryCursor::new();
    let mut ranges = Vec::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        for cap in m.captures {
            if cap.index == export_idx {
                ranges.push(cap.node.byte_range());
            }
        }
    }
    ranges
}

/// Check if a node's byte range is contained within any export statement.
fn is_exported(node: &Node, export_ranges: &[std::ops::Range<usize>]) -> bool {
    let r = node.byte_range();
    export_ranges
        .iter()
        .any(|er| er.start <= r.start && r.end <= er.end)
}

/// Pre-order walk of the whole tree collecting exported names.
///
/// Iterative with one cursor on purpose: the recursive form spent a stack
/// frame per tree depth, and a large non-TypeScript document parsed by the
/// TypeScript grammar (an HTML page whose URL ended in `.d.ts`) produced an
/// error-recovery tree deep enough to overflow the 2 MiB executor worker
/// stack and abort the daemon - twice, 55 s apart, on the same request.
fn collect_exported_symbol_names(source: &str, root: &Node) -> HashSet<String> {
    let mut exported = HashSet::new();
    let mut cursor = root.walk();
    loop {
        let node = cursor.node();
        if node.kind() == "export_statement" {
            collect_names_from_export_statement(source, &node, &mut exported);
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return exported;
            }
        }
    }
}

fn collect_names_from_export_statement(source: &str, node: &Node, exported: &mut HashSet<String>) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    let mut saw_default = false;
    loop {
        let child = cursor.node();
        match child.kind() {
            "default" => saw_default = true,
            "export_clause" => collect_names_from_export_clause(source, &child, exported),
            "identifier" | "type_identifier" | "property_identifier" if saw_default => {
                exported.insert(node_text(source, &child).to_string());
                return;
            }
            _ => {}
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn collect_names_from_export_clause(source: &str, node: &Node, exported: &mut HashSet<String>) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "export_specifier" {
            if let Some(exported_name) = last_identifier_text(source, &child) {
                exported.insert(exported_name);
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn last_identifier_text(source: &str, node: &Node) -> Option<String> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    let mut last = None;
    loop {
        let child = cursor.node();
        if matches!(
            child.kind(),
            "identifier"
                | "type_identifier"
                | "property_identifier"
                | "shorthand_property_identifier"
        ) {
            last = Some(node_text(source, &child).to_string());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    last
}

fn mark_named_exports(symbols: &mut [Symbol], exported_names: &HashSet<String>) {
    for symbol in symbols {
        if symbol.scope_chain.is_empty()
            && symbol.parent.is_none()
            && exported_names.contains(&symbol.name)
        {
            symbol.exported = true;
        }
    }
}

/// Extract the first line of a node as its signature.
fn extract_signature(source: &str, node: &Node) -> String {
    let text = node_text(source, node);
    let first_line = text.lines().next().unwrap_or(text);
    // Trim trailing opening brace if present
    let trimmed = first_line.trim_end();
    let trimmed = trimmed.strip_suffix('{').unwrap_or(trimmed).trim_end();
    trimmed.to_string()
}

fn push_default_export_symbol(
    symbols: &mut Vec<Symbol>,
    source: &str,
    lang: LangId,
    body_node: Node,
    def_node: Node,
) {
    let kind = if body_node.kind() == "class" {
        SymbolKind::Class
    } else {
        SymbolKind::Function
    };

    symbols.push(Symbol {
        name: "default".to_string(),
        kind,
        range: node_range_with_decorators(&def_node, source, lang),
        signature: Some(extract_signature(source, &def_node)),
        scope_chain: vec![],
        exported: true,
        parent: None,
    });
}

/// Extract symbols from TypeScript / TSX source.
fn extract_ts_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::TypeScript;
    let capture_names = query.capture_names();

    let export_ranges = collect_export_ranges(source, root, query);
    let exported_names = collect_exported_symbol_names(source, root);

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        // Determine what kind of match this is by looking at capture names
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut arrow_name_node = None;
        let mut arrow_def_node = None;
        let mut arrow_decl_node = None;
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut method_class_name_node = None;
        let mut method_name_node = None;
        let mut method_def_node = None;
        let mut interface_name_node = None;
        let mut interface_def_node = None;
        let mut enum_name_node = None;
        let mut enum_def_node = None;
        let mut type_alias_name_node = None;
        let mut type_alias_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;
        let mut var_decl_node = None;
        let mut default_body_node = None;
        let mut default_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "arrow.name" => arrow_name_node = Some(cap.node),
                "arrow.def" => arrow_def_node = Some(cap.node),
                "arrow.decl" => arrow_decl_node = Some(cap.node),
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "method.class_name" => method_class_name_node = Some(cap.node),
                "method.name" => method_name_node = Some(cap.node),
                "method.def" => method_def_node = Some(cap.node),
                "interface.name" => interface_name_node = Some(cap.node),
                "interface.def" => interface_def_node = Some(cap.node),
                "enum.name" => enum_name_node = Some(cap.node),
                "enum.def" => enum_def_node = Some(cap.node),
                "type_alias.name" => type_alias_name_node = Some(cap.node),
                "type_alias.def" => type_alias_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                "var.decl" => var_decl_node = Some(cap.node),
                "default.body" => default_body_node = Some(cap.node),
                "default.def" => default_def_node = Some(cap.node),
                _ => {}
            }
        }

        // Function declaration
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Function,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        // Arrow/function expression declarator
        if let (Some(name_node), Some(def_node)) = (arrow_name_node, arrow_def_node) {
            let range_node = arrow_decl_node.unwrap_or(def_node);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Function,
                range: node_range_with_decorators(&range_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        // Anonymous/default function or class expression
        if let (Some(body_node), Some(def_node)) = (default_body_node, default_def_node) {
            push_default_export_symbol(&mut symbols, source, lang, body_node, def_node);
        }

        // Class declaration
        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        // Method definition
        if let (Some(class_name_node), Some(name_node), Some(def_node)) =
            (method_class_name_node, method_name_node, method_def_node)
        {
            let class_name = node_text(source, &class_name_node).to_string();
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Method,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![class_name.clone()],
                exported: false, // methods inherit export from class
                parent: Some(class_name),
            });
        }

        // Interface declaration
        if let (Some(name_node), Some(def_node)) = (interface_name_node, interface_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Interface,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        // Enum declaration
        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Enum,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        // Type alias
        if let (Some(name_node), Some(def_node)) = (type_alias_name_node, type_alias_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::TypeAlias,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        // Top-level const/let variable declaration (not arrow functions — those are handled above)
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            // Only include module-scope variables (parent is program/export_statement, not inside a function)
            let is_top_level = def_node
                .parent()
                .map(|p| p.kind() == "program" || p.kind() == "export_statement")
                .unwrap_or(false);
            let range_node = var_decl_node.unwrap_or(def_node);
            let is_function_like = if range_node.kind() == "variable_declarator" {
                variable_declarator_has_function_value(&range_node)
            } else {
                lexical_declaration_has_function_value(&def_node)
            };
            let name = node_text(source, &name_node).to_string();
            let already_captured = symbols.iter().any(|s| s.name == name);
            if is_top_level && !is_function_like && !already_captured {
                symbols.push(Symbol {
                    name,
                    kind: SymbolKind::Variable,
                    range: node_range_with_decorators(&range_node, source, lang),
                    signature: Some(extract_signature(source, &def_node)),
                    scope_chain: vec![],
                    exported: is_exported(&def_node, &export_ranges),
                    parent: None,
                });
            }
        }
    }

    mark_named_exports(&mut symbols, &exported_names);

    // Deduplicate: methods can appear as both class and method captures
    dedup_symbols(&mut symbols);
    Ok(symbols)
}

/// Extract symbols from JavaScript source.
fn extract_js_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::JavaScript;
    let capture_names = query.capture_names();

    let export_ranges = collect_export_ranges(source, root, query);
    let exported_names = collect_exported_symbol_names(source, root);

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut arrow_name_node = None;
        let mut arrow_def_node = None;
        let mut arrow_decl_node = None;
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut method_class_name_node = None;
        let mut method_name_node = None;
        let mut method_def_node = None;
        let mut default_body_node = None;
        let mut default_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;
        let mut var_decl_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "arrow.name" => arrow_name_node = Some(cap.node),
                "arrow.def" => arrow_def_node = Some(cap.node),
                "arrow.decl" => arrow_decl_node = Some(cap.node),
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "method.class_name" => method_class_name_node = Some(cap.node),
                "method.name" => method_name_node = Some(cap.node),
                "method.def" => method_def_node = Some(cap.node),
                "default.body" => default_body_node = Some(cap.node),
                "default.def" => default_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                "var.decl" => var_decl_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Function,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (arrow_name_node, arrow_def_node) {
            let range_node = arrow_decl_node.unwrap_or(def_node);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Function,
                range: node_range_with_decorators(&range_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        if let (Some(body_node), Some(def_node)) = (default_body_node, default_def_node) {
            push_default_export_symbol(&mut symbols, source, lang, body_node, def_node);
        }

        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        if let (Some(class_name_node), Some(name_node), Some(def_node)) =
            (method_class_name_node, method_name_node, method_def_node)
        {
            let class_name = node_text(source, &class_name_node).to_string();
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Method,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![class_name.clone()],
                exported: false,
                parent: Some(class_name),
            });
        }

        // Top-level const/let/var declarations. JS_QUERY captures these the same
        // way TS does, but the JS extractor previously dropped them (they fell
        // into the `_ => {}` arm), so e.g. `export const VERSION = "1.0"` in a
        // .js file produced no symbol for outline/dead_code/callgraph. Mirror the
        // TS extractor: module-scope, non-function-valued, not already captured.
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            let is_top_level = def_node
                .parent()
                .map(|p| p.kind() == "program" || p.kind() == "export_statement")
                .unwrap_or(false);
            let range_node = var_decl_node.unwrap_or(def_node);
            let is_function_like = if range_node.kind() == "variable_declarator" {
                variable_declarator_has_function_value(&range_node)
            } else {
                lexical_declaration_has_function_value(&def_node)
            };
            let name = node_text(source, &name_node).to_string();
            let already_captured = symbols.iter().any(|s| s.name == name);
            if is_top_level && !is_function_like && !already_captured {
                symbols.push(Symbol {
                    name,
                    kind: SymbolKind::Variable,
                    range: node_range_with_decorators(&range_node, source, lang),
                    signature: Some(extract_signature(source, &def_node)),
                    scope_chain: vec![],
                    exported: is_exported(&def_node, &export_ranges),
                    parent: None,
                });
            }
        }
    }

    mark_named_exports(&mut symbols, &exported_names);
    dedup_symbols(&mut symbols);
    Ok(symbols)
}

/// Walk parent nodes to build a scope chain for Python symbols.
/// A function inside `class_definition > block` gets the class name in its scope.
fn py_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        if parent.kind() == "class_definition" {
            if let Some(name_node) = parent.child_by_field_name("name") {
                chain.push(node_text(source, &name_node).to_string());
            }
        }
        current = parent.parent();
    }
    chain.reverse();
    chain
}

/// Extract symbols from Python source.
fn extract_py_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Python;
    let capture_names = query.capture_names();

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    // Track decorated definitions to avoid double-counting
    let mut decorated_fn_lines = std::collections::HashSet::new();

    // First pass: collect decorated definition info
    {
        let mut cursor2 = QueryCursor::new();
        let mut matches2 = cursor2.matches(query, *root, source.as_bytes());
        while let Some(m) = {
            matches2.advance();
            matches2.get()
        } {
            let mut dec_def_node = None;
            let mut dec_decorator_node = None;

            for cap in m.captures {
                let Some(&name) = capture_names.get(cap.index as usize) else {
                    continue;
                };
                match name {
                    "dec.def" => dec_def_node = Some(cap.node),
                    "dec.decorator" => dec_decorator_node = Some(cap.node),
                    _ => {}
                }
            }

            if let (Some(def_node), Some(_dec_node)) = (dec_def_node, dec_decorator_node) {
                // Find the inner function_definition or class_definition
                let mut child_cursor = def_node.walk();
                if child_cursor.goto_first_child() {
                    loop {
                        let child = child_cursor.node();
                        if child.kind() == "function_definition"
                            || child.kind() == "class_definition"
                        {
                            decorated_fn_lines.insert(child.start_position().row);
                        }
                        if !child_cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }
        }
    }

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut class_name_node = None;
        let mut class_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                _ => {}
            }
        }

        // Function definition
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope = py_scope_chain(&def_node, source);
            let is_method = !scope.is_empty();
            let name = node_text(source, &name_node).to_string();
            // Skip __init__ and other dunders as separate symbols — they're methods
            let kind = if is_method {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };

            // Build signature — include decorator if this is a decorated function
            let sig = if decorated_fn_lines.contains(&def_node.start_position().row) {
                // Find the decorated_definition parent to get decorator text
                let mut sig_parts = Vec::new();
                let mut parent = def_node.parent();
                while let Some(p) = parent {
                    if p.kind() == "decorated_definition" {
                        // Get decorator lines
                        let mut dc = p.walk();
                        if dc.goto_first_child() {
                            loop {
                                if dc.node().kind() == "decorator" {
                                    sig_parts.push(node_text(source, &dc.node()).to_string());
                                }
                                if !dc.goto_next_sibling() {
                                    break;
                                }
                            }
                        }
                        break;
                    }
                    parent = p.parent();
                }
                sig_parts.push(extract_signature(source, &def_node));
                Some(sig_parts.join("\n"))
            } else {
                Some(extract_signature(source, &def_node))
            };

            symbols.push(Symbol {
                name,
                kind,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: sig,
                scope_chain: scope.clone(),
                exported: false, // Python has no export concept
                parent: scope.last().cloned(),
            });
        }

        // Class definition
        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            let scope = py_scope_chain(&def_node, source);

            // Build signature — include decorator if decorated
            let sig = if decorated_fn_lines.contains(&def_node.start_position().row) {
                let mut sig_parts = Vec::new();
                let mut parent = def_node.parent();
                while let Some(p) = parent {
                    if p.kind() == "decorated_definition" {
                        let mut dc = p.walk();
                        if dc.goto_first_child() {
                            loop {
                                if dc.node().kind() == "decorator" {
                                    sig_parts.push(node_text(source, &dc.node()).to_string());
                                }
                                if !dc.goto_next_sibling() {
                                    break;
                                }
                            }
                        }
                        break;
                    }
                    parent = p.parent();
                }
                sig_parts.push(extract_signature(source, &def_node));
                Some(sig_parts.join("\n"))
            } else {
                Some(extract_signature(source, &def_node))
            };

            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: sig,
                scope_chain: scope.clone(),
                exported: false,
                parent: scope.last().cloned(),
            });
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn rust_mod_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut scopes = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        if parent.kind() == "mod_item" {
            if let Some(name_node) = parent.child_by_field_name("name") {
                scopes.push(node_text(source, &name_node).to_string());
            }
        }
        current = parent.parent();
    }

    scopes.reverse();
    scopes
}

/// Rust function attributes that expose the function to code outside the Rust
/// call graph. This list is deliberately conservative: it covers known ABI,
/// wasm, constructor/destructor, and Tauri command entry points, but it does not
/// guess route macro shapes from web frameworks.
pub(crate) const RUST_ENTRY_POINT_ATTRIBUTES: &[&str] = &[
    "tauri::command",
    "wasm_bindgen",
    "no_mangle",
    "export_name",
    "ctor",
    "dtor",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RustAttributeEntryPoint {
    pub name: String,
    pub scoped_name: String,
    pub attribute: &'static str,
}

/// Return Rust functions whose attributes make them external entry points.
pub(crate) fn rust_attribute_entry_points<'a>(
    source: &str,
    root: Node<'a>,
) -> Vec<RustAttributeEntryPoint> {
    let bare_command_imported = rust_file_imports_tauri_command(source, root);
    let mut function_nodes = Vec::new();
    rust_collect_function_items(root, &mut function_nodes);

    let mut entry_points = BTreeMap::new();
    for function_node in function_nodes {
        let Some(attribute) =
            rust_entry_point_attribute_for_function(source, &function_node, bare_command_imported)
        else {
            continue;
        };
        let Some((name, scoped_name)) = rust_function_identity(source, &function_node) else {
            continue;
        };
        entry_points
            .entry(scoped_name.clone())
            .or_insert(RustAttributeEntryPoint {
                name,
                scoped_name,
                attribute,
            });
    }

    entry_points.into_values().collect()
}

fn rust_collect_function_items<'a>(node: Node<'a>, function_nodes: &mut Vec<Node<'a>>) {
    if node.kind() == "function_item" {
        function_nodes.push(node);
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            rust_collect_function_items(cursor.node(), function_nodes);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

fn rust_function_identity(source: &str, function_node: &Node<'_>) -> Option<(String, String)> {
    let name_node = function_node.child_by_field_name("name")?;
    let name = node_text(source, &name_node).to_string();
    let declaration_list_owner = rust_function_declaration_list_owner(function_node);

    match declaration_list_owner.as_ref().map(Node::kind) {
        Some("impl_item") => {
            let scope_name = rust_impl_scope_name(declaration_list_owner.as_ref().unwrap(), source);
            let scoped_name = if scope_name.is_empty() {
                name.clone()
            } else {
                format!("{scope_name}::{name}")
            };
            Some((name, scoped_name))
        }
        Some(owner_kind) if owner_kind != "mod_item" => None,
        _ => {
            let scope_chain = rust_mod_scope_chain(function_node, source);
            let scoped_name = if scope_chain.is_empty() {
                name.clone()
            } else {
                format!("{}::{name}", scope_chain.join("::"))
            };
            Some((name, scoped_name))
        }
    }
}

fn rust_function_declaration_list_owner<'a>(function_node: &Node<'a>) -> Option<Node<'a>> {
    function_node
        .parent()
        .filter(|parent| parent.kind() == "declaration_list")
        .and_then(|parent| parent.parent())
}

fn rust_impl_scope_name(impl_node: &Node, source: &str) -> String {
    let mut type_names: Vec<String> = Vec::new();
    let mut child_cursor = impl_node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            if child.kind() == "type_identifier" || child.kind() == "generic_type" {
                type_names.push(node_text(source, &child).to_string());
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }

    if type_names.len() >= 2 {
        format!("{} for {}", type_names[0], type_names[1])
    } else if type_names.len() == 1 {
        type_names[0].clone()
    } else {
        String::new()
    }
}

fn rust_entry_point_attribute_for_function(
    source: &str,
    function_node: &Node,
    bare_command_imported: bool,
) -> Option<&'static str> {
    let mut current = *function_node;
    while let Some(prev) = current.prev_sibling() {
        let kind = prev.kind();
        match kind {
            "attribute_item" => {
                if let Some(attribute) =
                    rust_entry_point_attribute_from_node(source, &prev, bare_command_imported)
                {
                    return Some(attribute);
                }
                current = prev;
            }
            "line_comment" if node_text(source, &prev).starts_with("///") => {
                current = prev;
            }
            "block_comment" if node_text(source, &prev).starts_with("/**") => {
                current = prev;
            }
            _ => break,
        }
    }

    None
}

fn rust_entry_point_attribute_from_node(
    source: &str,
    attribute_node: &Node,
    bare_command_imported: bool,
) -> Option<&'static str> {
    let text = node_text(source, attribute_node);
    let path = rust_attribute_path(text)?;
    rust_entry_point_attribute_for_path(path, bare_command_imported)
}

fn rust_attribute_path(attribute_text: &str) -> Option<&str> {
    let inner = attribute_text
        .trim()
        .strip_prefix("#[")?
        .strip_suffix(']')?
        .trim();
    let effective = inner
        .strip_prefix("unsafe(")
        .and_then(|wrapped| wrapped.strip_suffix(')'))
        .map(str::trim)
        .unwrap_or(inner);
    effective
        .split(|ch: char| matches!(ch, '(' | '=' | ','))
        .next()
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

fn rust_entry_point_attribute_for_path(
    path: &str,
    bare_command_imported: bool,
) -> Option<&'static str> {
    let compact = path
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    if compact == "tauri::command" {
        return Some("tauri::command");
    }
    if compact == "command" && bare_command_imported {
        return Some("tauri::command");
    }

    let last_segment = compact.rsplit("::").next().unwrap_or(compact.as_str());
    RUST_ENTRY_POINT_ATTRIBUTES
        .iter()
        .copied()
        .find(|attribute| *attribute != "tauri::command" && *attribute == last_segment)
}

fn rust_file_imports_tauri_command(source: &str, root: Node<'_>) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "use_declaration"
            && rust_use_declaration_imports_tauri_command(node_text(source, &node))
        {
            return true;
        }

        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                stack.push(cursor.node());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    false
}

fn rust_use_declaration_imports_tauri_command(use_text: &str) -> bool {
    let compact = use_text
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<String>();
    let Some(use_index) = compact.find("use") else {
        return false;
    };
    let path = compact[use_index + "use".len()..].trim_end_matches(';');
    if path == "tauri::command" {
        return true;
    }

    let Some(inner) = path
        .strip_prefix("tauri::{")
        .and_then(|inner| inner.strip_suffix('}'))
    else {
        return false;
    };

    split_rust_use_items(inner)
        .into_iter()
        .any(|item| item == "command" || item == "commandascommand")
}

fn split_rust_use_items(inner: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (idx, ch) in inner.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(inner[start..idx].trim());
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    if start <= inner.len() {
        let tail = inner[start..].trim();
        if !tail.is_empty() {
            items.push(tail);
        }
    }
    items
}

/// Extract symbols from Rust source without compiling a tree-sitter query.
/// Handles: free functions, struct, enum, trait (as Interface), impl methods with scope chains.
fn extract_rs_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Rust;
    let is_pub = |node: &Node| -> bool {
        let mut child_cursor = node.walk();
        if child_cursor.goto_first_child() {
            loop {
                if child_cursor.node().kind() == "visibility_modifier" {
                    return true;
                }
                if !child_cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        false
    };

    let item_symbol = |node: Node<'_>, kind: SymbolKind| -> Option<Symbol> {
        let name_node = node.child_by_field_name("name")?;
        Some(Symbol {
            name: node_text(source, &name_node).to_string(),
            kind,
            range: node_range_with_decorators(&node, source, lang),
            signature: Some(extract_signature(source, &node)),
            scope_chain: Vec::new(),
            exported: is_pub(&node),
            parent: None,
        })
    };

    let mut symbols = Vec::new();
    let mut pending = vec![*root];
    while let Some(node) = pending.pop() {
        match node.kind() {
            "function_item" => {
                let declaration_list_owner = node
                    .parent()
                    .filter(|parent| parent.kind() == "declaration_list")
                    .and_then(|parent| parent.parent());
                let in_non_module_declaration_list = declaration_list_owner
                    .as_ref()
                    .is_some_and(|owner| owner.kind() != "mod_item");

                if !in_non_module_declaration_list {
                    if let Some(name_node) = node.child_by_field_name("name") {
                        let scope_chain = rust_mod_scope_chain(&node, source);
                        symbols.push(Symbol {
                            name: node_text(source, &name_node).to_string(),
                            kind: SymbolKind::Function,
                            range: node_range_with_decorators(&node, source, lang),
                            signature: Some(extract_signature(source, &node)),
                            scope_chain: scope_chain.clone(),
                            exported: is_pub(&node),
                            parent: scope_chain.last().cloned(),
                        });
                    }
                }
            }
            "struct_item" => {
                if let Some(symbol) = item_symbol(node, SymbolKind::Struct) {
                    symbols.push(symbol);
                }
            }
            "enum_item" => {
                if let Some(symbol) = item_symbol(node, SymbolKind::Enum) {
                    symbols.push(symbol);
                }
            }
            "trait_item" => {
                if let Some(symbol) = item_symbol(node, SymbolKind::Interface) {
                    symbols.push(symbol);
                }
            }
            "impl_item" => {
                let mut type_names = Vec::new();
                let mut child_cursor = node.walk();
                if child_cursor.goto_first_child() {
                    loop {
                        let child = child_cursor.node();
                        if child.kind() == "type_identifier" || child.kind() == "generic_type" {
                            type_names.push(node_text(source, &child).to_string());
                        }
                        if !child_cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }

                let scope_name = if type_names.len() >= 2 {
                    format!("{} for {}", type_names[0], type_names[1])
                } else {
                    type_names.first().cloned().unwrap_or_default()
                };
                let parent_name = type_names.last().cloned().unwrap_or_default();

                let mut child_cursor = node.walk();
                if child_cursor.goto_first_child() {
                    loop {
                        let child = child_cursor.node();
                        if child.kind() == "declaration_list" {
                            let mut method_cursor = child.walk();
                            if method_cursor.goto_first_child() {
                                loop {
                                    let method = method_cursor.node();
                                    if method.kind() == "function_item" {
                                        if let Some(name_node) = method.child_by_field_name("name")
                                        {
                                            symbols.push(Symbol {
                                                name: node_text(source, &name_node).to_string(),
                                                kind: SymbolKind::Method,
                                                range: node_range_with_decorators(
                                                    &method, source, lang,
                                                ),
                                                signature: Some(extract_signature(source, &method)),
                                                scope_chain: if scope_name.is_empty() {
                                                    Vec::new()
                                                } else {
                                                    vec![scope_name.clone()]
                                                },
                                                exported: is_pub(&method),
                                                parent: if parent_name.is_empty() {
                                                    None
                                                } else {
                                                    Some(parent_name.clone())
                                                },
                                            });
                                        }
                                    }
                                    if !method_cursor.goto_next_sibling() {
                                        break;
                                    }
                                }
                            }
                        }
                        if !child_cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }
            _ => {}
        }

        let mut child_cursor = node.walk();
        let mut children = Vec::new();
        if child_cursor.goto_first_child() {
            loop {
                children.push(child_cursor.node());
                if !child_cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        pending.extend(children.into_iter().rev());
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

/// Extract symbols from Go source.
/// Handles: functions, methods (with receiver scope chain), struct/interface types,
/// uppercase-first-letter export detection.
fn extract_go_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Go;
    let capture_names = query.capture_names();

    let is_go_exported = |name: &str| -> bool {
        name.chars()
            .next()
            .map(|c| c.is_uppercase())
            .unwrap_or(false)
    };

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut method_name_node = None;
        let mut method_def_node = None;
        let mut type_name_node = None;
        let mut type_body_node = None;
        let mut type_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "method.name" => method_name_node = Some(cap.node),
                "method.def" => method_def_node = Some(cap.node),
                "type.name" => type_name_node = Some(cap.node),
                "type.body" => type_body_node = Some(cap.node),
                "type.def" => type_def_node = Some(cap.node),
                _ => {}
            }
        }

        // Function declaration
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let name = node_text(source, &name_node).to_string();
            symbols.push(Symbol {
                exported: is_go_exported(&name),
                name,
                kind: SymbolKind::Function,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                parent: None,
            });
        }

        // Method declaration (with receiver)
        if let (Some(name_node), Some(def_node)) = (method_name_node, method_def_node) {
            let name = node_text(source, &name_node).to_string();

            // Extract receiver type from the first parameter_list
            let receiver_type = extract_go_receiver_type(&def_node, source);
            let scope_chain = if let Some(ref rt) = receiver_type {
                vec![rt.clone()]
            } else {
                vec![]
            };

            symbols.push(Symbol {
                exported: is_go_exported(&name),
                name,
                kind: SymbolKind::Method,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain,
                parent: receiver_type,
            });
        }

        // Type declarations (struct or interface)
        if let (Some(name_node), Some(body_node), Some(def_node)) =
            (type_name_node, type_body_node, type_def_node)
        {
            let name = node_text(source, &name_node).to_string();
            let kind = match body_node.kind() {
                "struct_type" => SymbolKind::Struct,
                "interface_type" => SymbolKind::Interface,
                _ => SymbolKind::TypeAlias,
            };

            symbols.push(Symbol {
                exported: is_go_exported(&name),
                name,
                kind,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                parent: None,
            });
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

/// Extract the receiver type from a Go method_declaration node.
/// e.g. `func (m *MyStruct) String()` → Some("MyStruct")
fn extract_go_receiver_type(method_node: &Node, source: &str) -> Option<String> {
    // The first parameter_list is the receiver
    let mut child_cursor = method_node.walk();
    if child_cursor.goto_first_child() {
        loop {
            let child = child_cursor.node();
            if child.kind() == "parameter_list" {
                // Walk into parameter_list to find type_identifier
                return find_type_identifier_recursive(&child, source);
            }
            if !child_cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

fn split_scope_text(text: &str, separator: &str) -> Vec<String> {
    text.split(separator)
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn last_scope_segment(text: &str, separator: &str) -> String {
    split_scope_text(text, separator)
        .pop()
        .unwrap_or_else(|| text.trim().to_string())
}

fn zig_container_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "struct_declaration" | "enum_declaration" | "union_declaration" | "opaque_declaration"
        ) {
            if let Some(container) = parent.parent() {
                if container.kind() == "variable_declaration" {
                    let mut cursor = container.walk();
                    if cursor.goto_first_child() {
                        loop {
                            let child = cursor.node();
                            if child.kind() == "identifier" {
                                chain.push(node_text(source, &child).to_string());
                                break;
                            }
                            if !cursor.goto_next_sibling() {
                                break;
                            }
                        }
                    }
                }
            }
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

fn csharp_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        match parent.kind() {
            "namespace_declaration" | "file_scoped_namespace_declaration" => {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    chain.push(node_text(source, &name_node).to_string());
                }
            }
            "class_declaration"
            | "interface_declaration"
            | "struct_declaration"
            | "record_declaration" => {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    chain.push(node_text(source, &name_node).to_string());
                }
            }
            _ => {}
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

fn cpp_parent_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        match parent.kind() {
            "namespace_definition" => {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    chain.push(node_text(source, &name_node).to_string());
                }
            }
            "class_specifier" | "struct_specifier" => {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    chain.push(last_scope_segment(node_text(source, &name_node), "::"));
                }
            }
            _ => {}
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

fn template_signature(source: &str, template_node: &Node, item_node: &Node) -> String {
    format!(
        "{}\n{}",
        extract_signature(source, template_node),
        extract_signature(source, item_node)
    )
}

/// Extract symbols from C source.
fn extract_c_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::C;
    let capture_names = query.capture_names();

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut struct_name_node = None;
        let mut struct_def_node = None;
        let mut enum_name_node = None;
        let mut enum_def_node = None;
        let mut type_name_node = None;
        let mut type_def_node = None;
        let mut macro_name_node = None;
        let mut macro_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "struct.name" => struct_name_node = Some(cap.node),
                "struct.def" => struct_def_node = Some(cap.node),
                "enum.name" => enum_name_node = Some(cap.node),
                "enum.def" => enum_def_node = Some(cap.node),
                "type.name" => type_name_node = Some(cap.node),
                "type.def" => type_def_node = Some(cap.node),
                "macro.name" => macro_name_node = Some(cap.node),
                "macro.def" => macro_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Function,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: false,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (struct_name_node, struct_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Struct,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: false,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Enum,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: false,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (type_name_node, type_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::TypeAlias,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: false,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (macro_name_node, macro_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Variable,
                range: node_range(&def_node),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: false,
                parent: None,
            });
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

/// Extract symbols from C++ source.
fn extract_cpp_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Cpp;
    let capture_names = query.capture_names();

    let mut type_names = HashSet::new();
    {
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(query, *root, source.as_bytes());
        while let Some(m) = {
            matches.advance();
            matches.get()
        } {
            for cap in m.captures {
                let Some(&name) = capture_names.get(cap.index as usize) else {
                    continue;
                };
                match name {
                    "class.name"
                    | "struct.name"
                    | "template.class.name"
                    | "template.struct.name" => {
                        type_names.insert(last_scope_segment(node_text(source, &cap.node), "::"));
                    }
                    _ => {}
                }
            }
        }
    }

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut method_name_node = None;
        let mut method_def_node = None;
        let mut qual_scope_node = None;
        let mut qual_name_node = None;
        let mut qual_def_node = None;
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut struct_name_node = None;
        let mut struct_def_node = None;
        let mut enum_name_node = None;
        let mut enum_def_node = None;
        let mut namespace_name_node = None;
        let mut namespace_def_node = None;
        let mut template_class_name_node = None;
        let mut template_class_def_node = None;
        let mut template_class_item_node = None;
        let mut template_struct_name_node = None;
        let mut template_struct_def_node = None;
        let mut template_struct_item_node = None;
        let mut template_fn_name_node = None;
        let mut template_fn_def_node = None;
        let mut template_fn_item_node = None;
        let mut template_qual_scope_node = None;
        let mut template_qual_name_node = None;
        let mut template_qual_def_node = None;
        let mut template_qual_item_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "method.name" => method_name_node = Some(cap.node),
                "method.def" => method_def_node = Some(cap.node),
                "qual.scope" => qual_scope_node = Some(cap.node),
                "qual.name" => qual_name_node = Some(cap.node),
                "qual.def" => qual_def_node = Some(cap.node),
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "struct.name" => struct_name_node = Some(cap.node),
                "struct.def" => struct_def_node = Some(cap.node),
                "enum.name" => enum_name_node = Some(cap.node),
                "enum.def" => enum_def_node = Some(cap.node),
                "namespace.name" => namespace_name_node = Some(cap.node),
                "namespace.def" => namespace_def_node = Some(cap.node),
                "template.class.name" => template_class_name_node = Some(cap.node),
                "template.class.def" => template_class_def_node = Some(cap.node),
                "template.class.item" => template_class_item_node = Some(cap.node),
                "template.struct.name" => template_struct_name_node = Some(cap.node),
                "template.struct.def" => template_struct_def_node = Some(cap.node),
                "template.struct.item" => template_struct_item_node = Some(cap.node),
                "template.fn.name" => template_fn_name_node = Some(cap.node),
                "template.fn.def" => template_fn_def_node = Some(cap.node),
                "template.fn.item" => template_fn_item_node = Some(cap.node),
                "template.qual.scope" => template_qual_scope_node = Some(cap.node),
                "template.qual.name" => template_qual_name_node = Some(cap.node),
                "template.qual.def" => template_qual_def_node = Some(cap.node),
                "template.qual.item" => template_qual_item_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let in_template = def_node
                .parent()
                .map(|parent| parent.kind() == "template_declaration")
                .unwrap_or(false);
            if !in_template {
                let scope_chain = cpp_parent_scope_chain(&def_node, source);
                symbols.push(Symbol {
                    name: node_text(source, &name_node).to_string(),
                    kind: SymbolKind::Function,
                    range: node_range_with_decorators(&def_node, source, lang),
                    signature: Some(extract_signature(source, &def_node)),
                    scope_chain: scope_chain.clone(),
                    exported: false,
                    parent: scope_chain.last().cloned(),
                });
            }
        }

        if let (Some(name_node), Some(def_node)) = (method_name_node, method_def_node) {
            let scope_chain = cpp_parent_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Method,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(scope_node), Some(name_node), Some(def_node)) =
            (qual_scope_node, qual_name_node, qual_def_node)
        {
            let in_template = def_node
                .parent()
                .map(|parent| parent.kind() == "template_declaration")
                .unwrap_or(false);
            if !in_template {
                let scope_text = node_text(source, &scope_node);
                let scope_chain = split_scope_text(scope_text, "::");
                let parent = scope_chain.last().cloned();
                let kind = if parent
                    .as_ref()
                    .map(|segment| type_names.contains(segment))
                    .unwrap_or(false)
                {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };

                symbols.push(Symbol {
                    name: node_text(source, &name_node).to_string(),
                    kind,
                    range: node_range_with_decorators(&def_node, source, lang),
                    signature: Some(extract_signature(source, &def_node)),
                    scope_chain,
                    exported: false,
                    parent,
                });
            }
        }

        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            let in_template = def_node
                .parent()
                .map(|parent| parent.kind() == "template_declaration")
                .unwrap_or(false);
            if !in_template {
                let scope_chain = cpp_parent_scope_chain(&def_node, source);
                let name = last_scope_segment(node_text(source, &name_node), "::");
                symbols.push(Symbol {
                    name: name.clone(),
                    kind: SymbolKind::Class,
                    range: node_range_with_decorators(&def_node, source, lang),
                    signature: Some(extract_signature(source, &def_node)),
                    scope_chain: scope_chain.clone(),
                    exported: false,
                    parent: scope_chain.last().cloned(),
                });
            }
        }

        if let (Some(name_node), Some(def_node)) = (struct_name_node, struct_def_node) {
            let in_template = def_node
                .parent()
                .map(|parent| parent.kind() == "template_declaration")
                .unwrap_or(false);
            if !in_template {
                let scope_chain = cpp_parent_scope_chain(&def_node, source);
                let name = last_scope_segment(node_text(source, &name_node), "::");
                symbols.push(Symbol {
                    name: name.clone(),
                    kind: SymbolKind::Struct,
                    range: node_range_with_decorators(&def_node, source, lang),
                    signature: Some(extract_signature(source, &def_node)),
                    scope_chain: scope_chain.clone(),
                    exported: false,
                    parent: scope_chain.last().cloned(),
                });
            }
        }

        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            let scope_chain = cpp_parent_scope_chain(&def_node, source);
            let name = last_scope_segment(node_text(source, &name_node), "::");
            symbols.push(Symbol {
                name: name.clone(),
                kind: SymbolKind::Enum,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (namespace_name_node, namespace_def_node) {
            let scope_chain = cpp_parent_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::TypeAlias,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node), Some(item_node)) = (
            template_class_name_node,
            template_class_def_node,
            template_class_item_node,
        ) {
            let scope_chain = cpp_parent_scope_chain(&def_node, source);
            let name = last_scope_segment(node_text(source, &name_node), "::");
            symbols.push(Symbol {
                name: name.clone(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(template_signature(source, &def_node, &item_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node), Some(item_node)) = (
            template_struct_name_node,
            template_struct_def_node,
            template_struct_item_node,
        ) {
            let scope_chain = cpp_parent_scope_chain(&def_node, source);
            let name = last_scope_segment(node_text(source, &name_node), "::");
            symbols.push(Symbol {
                name: name.clone(),
                kind: SymbolKind::Struct,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(template_signature(source, &def_node, &item_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node), Some(item_node)) = (
            template_fn_name_node,
            template_fn_def_node,
            template_fn_item_node,
        ) {
            let scope_chain = cpp_parent_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Function,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(template_signature(source, &def_node, &item_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(scope_node), Some(name_node), Some(def_node), Some(item_node)) = (
            template_qual_scope_node,
            template_qual_name_node,
            template_qual_def_node,
            template_qual_item_node,
        ) {
            let scope_chain = split_scope_text(node_text(source, &scope_node), "::");
            let parent = scope_chain.last().cloned();
            let kind = if parent
                .as_ref()
                .map(|segment| type_names.contains(segment))
                .unwrap_or(false)
            {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };

            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(template_signature(source, &def_node, &item_node)),
                scope_chain,
                exported: false,
                parent,
            });
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

/// Extract symbols from Zig source.
fn extract_zig_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Zig;
    let capture_names = query.capture_names();

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut struct_name_node = None;
        let mut struct_def_node = None;
        let mut enum_name_node = None;
        let mut enum_def_node = None;
        let mut union_name_node = None;
        let mut union_def_node = None;
        let mut const_name_node = None;
        let mut const_def_node = None;
        let mut test_name_node = None;
        let mut test_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "struct.name" => struct_name_node = Some(cap.node),
                "struct.def" => struct_def_node = Some(cap.node),
                "enum.name" => enum_name_node = Some(cap.node),
                "enum.def" => enum_def_node = Some(cap.node),
                "union.name" => union_name_node = Some(cap.node),
                "union.def" => union_def_node = Some(cap.node),
                "const.name" => const_name_node = Some(cap.node),
                "const.def" => const_def_node = Some(cap.node),
                "test.name" => test_name_node = Some(cap.node),
                "test.def" => test_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope_chain = zig_container_scope_chain(&def_node, source);
            let kind = if scope_chain.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (struct_name_node, struct_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Struct,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: false,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Enum,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: false,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (union_name_node, union_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::TypeAlias,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: false,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (const_name_node, const_def_node) {
            let signature = extract_signature(source, &def_node);
            let is_container = signature.contains("= struct")
                || signature.contains("= enum")
                || signature.contains("= union")
                || signature.contains("= opaque");
            let is_const = signature.trim_start().starts_with("const ");
            let name = node_text(source, &name_node).to_string();
            let already_captured = symbols.iter().any(|symbol| symbol.name == name);
            if is_const && !is_container && !already_captured {
                symbols.push(Symbol {
                    name,
                    kind: SymbolKind::Variable,
                    range: node_range_with_decorators(&def_node, source, lang),
                    signature: Some(signature),
                    scope_chain: vec![],
                    exported: false,
                    parent: None,
                });
            }
        }

        if let (Some(name_node), Some(def_node)) = (test_name_node, test_def_node) {
            let scope_chain = zig_container_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).trim_matches('"').to_string(),
                kind: SymbolKind::Function,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

/// Extract symbols from C# source.
fn extract_csharp_symbols(
    source: &str,
    root: &Node,
    query: &Query,
) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::CSharp;
    let capture_names = query.capture_names();

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut interface_name_node = None;
        let mut interface_def_node = None;
        let mut struct_name_node = None;
        let mut struct_def_node = None;
        let mut enum_name_node = None;
        let mut enum_def_node = None;
        let mut method_name_node = None;
        let mut method_def_node = None;
        let mut property_name_node = None;
        let mut property_def_node = None;
        let mut namespace_name_node = None;
        let mut namespace_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "interface.name" => interface_name_node = Some(cap.node),
                "interface.def" => interface_def_node = Some(cap.node),
                "struct.name" => struct_name_node = Some(cap.node),
                "struct.def" => struct_def_node = Some(cap.node),
                "enum.name" => enum_name_node = Some(cap.node),
                "enum.def" => enum_def_node = Some(cap.node),
                "method.name" => method_name_node = Some(cap.node),
                "method.def" => method_def_node = Some(cap.node),
                "property.name" => property_name_node = Some(cap.node),
                "property.def" => property_def_node = Some(cap.node),
                "namespace.name" => namespace_name_node = Some(cap.node),
                "namespace.def" => namespace_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            let scope_chain = csharp_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (interface_name_node, interface_def_node) {
            let scope_chain = csharp_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Interface,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (struct_name_node, struct_def_node) {
            let scope_chain = csharp_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Struct,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            let scope_chain = csharp_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Enum,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (method_name_node, method_def_node) {
            let scope_chain = csharp_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Method,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (property_name_node, property_def_node) {
            let scope_chain = csharp_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Variable,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (namespace_name_node, namespace_def_node) {
            let scope_chain = csharp_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::TypeAlias,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scope_chain.clone(),
                exported: false,
                parent: scope_chain.last().cloned(),
            });
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

/// Recursively find the first type_identifier node in a subtree.
fn find_type_identifier_recursive(node: &Node, source: &str) -> Option<String> {
    if node.kind() == "type_identifier" {
        return Some(node_text(source, node).to_string());
    }
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            if let Some(result) = find_type_identifier_recursive(&cursor.node(), source) {
                return Some(result);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

/// Extract HTML headings (h1-h6) as symbols.
/// Each heading becomes a symbol with kind `Heading`, and its range covers
/// the element itself. Headings are nested based on their level.
fn extract_bash_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Bash;
    let capture_names = query.capture_names();

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut fn_name_node = None;
        let mut fn_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Function,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: false,
                parent: None,
            });
        }
    }

    Ok(symbols)
}

fn extract_pascal_symbols(
    source: &str,
    root: &Node,
    query: &Query,
) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Pascal;
    let capture_names = query.capture_names();

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut program_name_node = None;
        let mut program_def_node = None;
        let mut unit_name_node = None;
        let mut unit_def_node = None;
        let mut type_name_node = None;
        let mut type_def_node = None;
        let mut const_name_node = None;
        let mut const_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;
        let mut proc_name_node = None;
        let mut proc_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "program.name" => program_name_node = Some(cap.node),
                "program.def" => program_def_node = Some(cap.node),
                "unit.name" => unit_name_node = Some(cap.node),
                "unit.def" => unit_def_node = Some(cap.node),
                "type.name" => type_name_node = Some(cap.node),
                "type.def" => type_def_node = Some(cap.node),
                "const.name" => const_name_node = Some(cap.node),
                "const.def" => const_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                "proc.name" => proc_name_node = Some(cap.node),
                "proc.def" => proc_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (program_name_node, program_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: true,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (unit_name_node, unit_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: true,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (type_name_node, type_def_node) {
            let kind = pascal_type_kind(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: true,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (const_name_node, const_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Variable,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: true,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Variable,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: true,
                parent: None,
            });
        }

        if let (Some(name_node), Some(def_node)) = (proc_name_node, proc_def_node) {
            if def_node.kind() == "declProc" {
                if let Some(parent) = def_node.parent() {
                    if parent.kind() == "defProc" {
                        continue;
                    }
                }
            }

            let (name, scope_chain) = extract_pascal_name_and_scope(&name_node, &def_node, source);
            let kind = if scope_chain.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };

            symbols.push(Symbol {
                name,
                kind,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn pascal_type_kind(node: &Node, _source: &str) -> SymbolKind {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "declClass" {
            let mut child_cursor = child.walk();
            for grandchild in child.children(&mut child_cursor) {
                if grandchild.kind() == "kRecord" {
                    return SymbolKind::Struct;
                }
            }
            return SymbolKind::Class;
        }
        if child.kind() == "declIntf" {
            return SymbolKind::Interface;
        }
        if child.kind() == "type" {
            let mut child_cursor = child.walk();
            for grandchild in child.children(&mut child_cursor) {
                if grandchild.kind() == "declEnum" {
                    return SymbolKind::Enum;
                }
            }
        }
    }
    SymbolKind::TypeAlias
}

fn extract_pascal_name_and_scope(
    name_node: &Node,
    def_node: &Node,
    source: &str,
) -> (String, Vec<String>) {
    let mut scope_chain = Vec::new();
    let mut name = node_text(source, name_node).to_string();

    if name_node.kind() == "genericDot" {
        let mut cursor = name_node.walk();
        let idents: Vec<Node> = name_node
            .children(&mut cursor)
            .filter(|c| c.kind() == "identifier")
            .collect();
        if idents.len() >= 2 {
            let class_name = node_text(source, &idents[0]).to_string();
            scope_chain.push(class_name);
            name = node_text(source, &idents[1]).to_string();
        }
    }

    let mut current = def_node.parent();
    while let Some(parent) = current {
        if parent.kind() == "declType" {
            if let Some(type_name_node) = parent.child_by_field_name("name").or_else(|| {
                let mut cursor = parent.walk();
                let mut found = None;
                for child in parent.children(&mut cursor) {
                    if child.kind() == "identifier" {
                        found = Some(child);
                        break;
                    }
                }
                found
            }) {
                scope_chain.insert(0, node_text(source, &type_name_node).to_string());
            }
        }
        current = parent.parent();
    }

    (name, scope_chain)
}

/// Walk up from `node` and collect the names of any enclosing
/// contract / library / interface, outermost first.
fn solidity_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        match parent.kind() {
            "contract_declaration" | "library_declaration" | "interface_declaration" => {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    chain.push(node_text(source, &name_node).to_string());
                }
            }
            _ => {}
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

fn extract_solidity_symbols(
    source: &str,
    root: &Node,
    query: &Query,
) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Solidity;
    let capture_names = query.capture_names();

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut contract_name_node = None;
        let mut contract_def_node = None;
        let mut library_name_node = None;
        let mut library_def_node = None;
        let mut interface_name_node = None;
        let mut interface_def_node = None;
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut modifier_name_node = None;
        let mut modifier_def_node = None;
        let mut constructor_def_node = None;
        let mut fallback_receive_def_node = None;
        let mut event_name_node = None;
        let mut event_def_node = None;
        let mut error_name_node = None;
        let mut error_def_node = None;
        let mut struct_name_node = None;
        let mut struct_def_node = None;
        let mut enum_name_node = None;
        let mut enum_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "contract.name" => contract_name_node = Some(cap.node),
                "contract.def" => contract_def_node = Some(cap.node),
                "library.name" => library_name_node = Some(cap.node),
                "library.def" => library_def_node = Some(cap.node),
                "interface.name" => interface_name_node = Some(cap.node),
                "interface.def" => interface_def_node = Some(cap.node),
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "modifier.name" => modifier_name_node = Some(cap.node),
                "modifier.def" => modifier_def_node = Some(cap.node),
                "constructor.def" => constructor_def_node = Some(cap.node),
                "fallback_receive.def" => fallback_receive_def_node = Some(cap.node),
                "event.name" => event_name_node = Some(cap.node),
                "event.def" => event_def_node = Some(cap.node),
                "error.name" => error_name_node = Some(cap.node),
                "error.def" => error_def_node = Some(cap.node),
                "struct.name" => struct_name_node = Some(cap.node),
                "struct.def" => struct_def_node = Some(cap.node),
                "enum.name" => enum_name_node = Some(cap.node),
                "enum.def" => enum_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                _ => {}
            }
        }

        // Contract
        if let (Some(name_node), Some(def_node)) = (contract_name_node, contract_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: true,
                parent: None,
            });
        }

        // Library (treated like a contract — class-shaped container)
        if let (Some(name_node), Some(def_node)) = (library_name_node, library_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: true,
                parent: None,
            });
        }

        // Interface
        if let (Some(name_node), Some(def_node)) = (interface_name_node, interface_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Interface,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: true,
                parent: None,
            });
        }

        // Function — Method when inside a contract/library/interface, Function otherwise
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope_chain = solidity_scope_chain(&def_node, source);
            let kind = if scope_chain.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }

        // Modifier — always inside a contract/library/interface, treat as Method
        if let (Some(name_node), Some(def_node)) = (modifier_name_node, modifier_def_node) {
            let scope_chain = solidity_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Method,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }

        // Constructor — synthetic name "constructor", parent is the enclosing contract
        if let Some(def_node) = constructor_def_node {
            let scope_chain = solidity_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: "constructor".to_string(),
                kind: SymbolKind::Method,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }

        // receive() / fallback() — synthetic names, parent is the enclosing contract
        if let Some(def_node) = fallback_receive_def_node {
            let scope_chain = solidity_scope_chain(&def_node, source);
            let signature = extract_signature(source, &def_node);
            let name = if signature.trim_start().starts_with("receive") {
                "receive"
            } else {
                "fallback"
            };
            symbols.push(Symbol {
                name: name.to_string(),
                kind: SymbolKind::Method,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(signature),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }

        // Event
        if let (Some(name_node), Some(def_node)) = (event_name_node, event_def_node) {
            let scope_chain = solidity_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Function,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }

        // Error (custom error declaration)
        if let (Some(name_node), Some(def_node)) = (error_name_node, error_def_node) {
            let scope_chain = solidity_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::TypeAlias,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }

        // Struct
        if let (Some(name_node), Some(def_node)) = (struct_name_node, struct_def_node) {
            let scope_chain = solidity_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Struct,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }

        // Enum
        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            let scope_chain = solidity_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Enum,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }

        // State variable
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            let scope_chain = solidity_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Variable,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn extract_r_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::R;
    let capture_names = query.capture_names();

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut assign_node = None;
        let mut function_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "assign.def" => assign_node = Some(cap.node),
                "function.def" => function_node = Some(cap.node),
                _ => {}
            }
        }

        if let Some(def_node) = function_node {
            if let Some(symbol) = r_rightward_function_assignment_symbol(source, &def_node, lang) {
                symbols.push(symbol);
            }
            continue;
        }

        let Some(def_node) = assign_node else {
            continue;
        };
        let Some(assignment) = r_assignment_parts(source, &def_node) else {
            continue;
        };
        let Some(name_node) = r_assignment_name_node(&assignment) else {
            continue;
        };
        if name_node.kind() != "identifier" {
            continue;
        }

        let value_is_function = r_node_contains_kind(&assignment.value, "function_definition");
        if !value_is_function && !r_is_top_level_assignment(&def_node) {
            continue;
        }

        symbols.push(Symbol {
            name: node_text(source, &name_node).to_string(),
            kind: if value_is_function {
                SymbolKind::Function
            } else {
                SymbolKind::Variable
            },
            range: node_range_with_decorators(&def_node, source, lang),
            signature: Some(extract_signature(source, &def_node)),
            scope_chain: vec![],
            exported: true,
            parent: None,
        });
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn extract_groovy_symbols(
    source: &str,
    root: &Node,
    query: &Query,
) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Groovy;
    let capture_names = query.capture_names();
    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut interface_name_node = None;
        let mut interface_def_node = None;
        let mut trait_name_node = None;
        let mut trait_def_node = None;
        let mut enum_name_node = None;
        let mut enum_def_node = None;
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;
        let mut pipeline_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "interface.name" => interface_name_node = Some(cap.node),
                "interface.def" => interface_def_node = Some(cap.node),
                "trait.name" => trait_name_node = Some(cap.node),
                "trait.def" => trait_def_node = Some(cap.node),
                "enum.name" => enum_name_node = Some(cap.node),
                "enum.def" => enum_def_node = Some(cap.node),
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                "pipeline.def" => pipeline_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Class,
                groovy_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (interface_name_node, interface_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Interface,
                groovy_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (trait_name_node, trait_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Interface,
                groovy_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Enum,
                groovy_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope_chain = groovy_scope_chain(&def_node, source);
            let kind = if scope_chain.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                kind,
                scope_chain,
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Variable,
                groovy_scope_chain(&def_node, source),
                true,
            );
        }
        if let Some(def_node) = pipeline_def_node {
            symbols.push(Symbol {
                name: "pipeline".to_string(),
                kind: SymbolKind::Function,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: true,
                parent: None,
            });
        }
    }

    collect_groovy_task_symbols(source, root, &mut symbols);
    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn r_rightward_function_assignment_symbol(
    source: &str,
    function_node: &Node,
    lang: LangId,
) -> Option<Symbol> {
    let body = function_node.child_by_field_name("body")?;
    let assignment = r_assignment_parts(source, &body)?;
    if !matches!(assignment.operator.as_str(), "->" | "->>") {
        return None;
    }
    let function_text = node_text(source, function_node);
    let name = r_rightward_assignment_name_from_text(function_text).or_else(|| {
        let name_node = assignment.right;
        (name_node.kind() == "identifier").then(|| node_text(source, &name_node).to_string())
    })?;

    let signature = r_rightward_function_signature(source, function_node, &name);

    Some(Symbol {
        name,
        kind: SymbolKind::Function,
        range: node_range_with_decorators(function_node, source, lang),
        signature: Some(signature),
        scope_chain: vec![],
        exported: true,
        parent: None,
    })
}

fn r_rightward_function_signature(source: &str, function_node: &Node, name: &str) -> String {
    let signature = extract_signature(source, function_node);
    if signature.contains(name) {
        signature
    } else {
        format!("{signature} -> {name}")
    }
}

fn r_rightward_assignment_name_from_text(text: &str) -> Option<String> {
    let (_, after) = text.rsplit_once("->>").or_else(|| text.rsplit_once("->"))?;
    let trimmed = after.trim_start();
    let name: String = trimmed
        .chars()
        .take_while(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '.')
        .collect();
    (!name.is_empty()).then_some(name)
}

struct RAssignmentParts<'tree> {
    operator: String,
    left: Node<'tree>,
    right: Node<'tree>,
    value: Node<'tree>,
}

fn r_assignment_name_node<'tree>(assignment: &RAssignmentParts<'tree>) -> Option<Node<'tree>> {
    match assignment.operator.as_str() {
        "<-" | "=" | "<<-" => Some(assignment.left),
        "->" | "->>" => Some(assignment.right),
        _ => None,
    }
}

fn r_assignment_parts<'tree>(source: &str, node: &Node<'tree>) -> Option<RAssignmentParts<'tree>> {
    if node.kind() != "binary_operator" {
        return None;
    }

    let mut operator = None;
    let mut operator_index = None;
    for index in 0..node.child_count() {
        let child = node.child(index as u32)?;
        let text = node_text(source, &child).trim();
        if matches!(text, "<-" | "=" | "<<-" | "->" | "->>") {
            operator = Some(text.to_string());
            operator_index = Some(index);
            break;
        }
    }

    let operator = operator?;
    let operator_index = operator_index?;

    let left = r_nearest_named_child_before(node, operator_index)
        .or_else(|| node.child_by_field_name("lhs"))
        .or_else(|| node.child_by_field_name("left"))?;
    let right = r_nearest_named_child_after(node, operator_index)
        .or_else(|| node.child_by_field_name("rhs"))
        .or_else(|| node.child_by_field_name("right"))?;

    let value = match operator.as_str() {
        "<-" | "=" | "<<-" => right,
        "->" | "->>" => left,
        _ => return None,
    };

    Some(RAssignmentParts {
        operator,
        left,
        right,
        value,
    })
}

fn r_nearest_named_child_before<'tree>(node: &Node<'tree>, index: usize) -> Option<Node<'tree>> {
    (0..index)
        .rev()
        .filter_map(|child_index| node.child(child_index as u32))
        .find(|child| child.is_named())
}

fn r_nearest_named_child_after<'tree>(node: &Node<'tree>, index: usize) -> Option<Node<'tree>> {
    ((index + 1)..node.child_count())
        .filter_map(|child_index| node.child(child_index as u32))
        .find(|child| child.is_named())
}

fn r_node_contains_kind(node: &Node, kind: &str) -> bool {
    if node.kind() == kind {
        return true;
    }

    let mut cursor = node.walk();
    let found = node
        .children(&mut cursor)
        .any(|child| r_node_contains_kind(&child, kind));
    found
}

fn r_is_top_level_assignment(node: &Node) -> bool {
    let mut current = node.parent();
    while let Some(parent) = current {
        match parent.kind() {
            "program" => return true,
            "braced_expression" | "function_definition" => return false,
            _ => current = parent.parent(),
        }
    }
    false
}

fn extract_objc_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::ObjC;
    let capture_names = query.capture_names();

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };

            let def_node = cap.node;
            match name {
                "class.def" => {
                    if let Some(symbol_name) = objc_container_name(source, &def_node) {
                        let scope_chain = objc_scope_chain(&def_node, source);
                        symbols.push(Symbol {
                            name: symbol_name,
                            kind: SymbolKind::Class,
                            range: node_range_with_decorators(&def_node, source, lang),
                            signature: Some(extract_signature(source, &def_node)),
                            parent: scope_chain.last().cloned(),
                            scope_chain,
                            exported: false,
                        });
                    }
                }
                "interface.def" => {
                    if let Some(symbol_name) = objc_container_name(source, &def_node) {
                        let scope_chain = objc_scope_chain(&def_node, source);
                        symbols.push(Symbol {
                            name: symbol_name,
                            kind: SymbolKind::Interface,
                            range: node_range_with_decorators(&def_node, source, lang),
                            signature: Some(extract_signature(source, &def_node)),
                            parent: scope_chain.last().cloned(),
                            scope_chain,
                            exported: false,
                        });
                    }
                }
                "method.def" => {
                    if let Some(symbol_name) = objc_method_selector(source, &def_node) {
                        let scope_chain = objc_scope_chain(&def_node, source);
                        symbols.push(Symbol {
                            name: symbol_name,
                            kind: SymbolKind::Method,
                            range: node_range_with_decorators(&def_node, source, lang),
                            signature: Some(extract_signature(source, &def_node)),
                            parent: scope_chain.last().cloned(),
                            scope_chain,
                            exported: false,
                        });
                    }
                }
                "property.def" => {
                    if let Some(symbol_name) = objc_property_name(source, &def_node) {
                        let scope_chain = objc_scope_chain(&def_node, source);
                        symbols.push(Symbol {
                            name: symbol_name,
                            kind: SymbolKind::Variable,
                            range: node_range_with_decorators(&def_node, source, lang),
                            signature: Some(extract_signature(source, &def_node)),
                            parent: scope_chain.last().cloned(),
                            scope_chain,
                            exported: false,
                        });
                    }
                }
                "fn.def" => {
                    if let Some(symbol_name) = objc_function_name(source, &def_node) {
                        let scope_chain = objc_scope_chain(&def_node, source);
                        symbols.push(Symbol {
                            name: symbol_name,
                            kind: SymbolKind::Function,
                            range: node_range_with_decorators(&def_node, source, lang),
                            signature: Some(extract_signature(source, &def_node)),
                            parent: scope_chain.last().cloned(),
                            scope_chain,
                            exported: false,
                        });
                    }
                }
                "type.def" => {
                    if let Some(symbol_name) = objc_typedef_name(source, &def_node) {
                        let scope_chain = objc_scope_chain(&def_node, source);
                        symbols.push(Symbol {
                            name: symbol_name,
                            kind: SymbolKind::TypeAlias,
                            range: node_range_with_decorators(&def_node, source, lang),
                            signature: Some(extract_signature(source, &def_node)),
                            parent: scope_chain.last().cloned(),
                            scope_chain,
                            exported: false,
                        });
                    }
                }
                _ => {}
            }
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn objc_container_name(source: &str, node: &Node) -> Option<String> {
    objc_first_direct_identifier(source, node)
}

fn objc_method_selector(source: &str, node: &Node) -> Option<String> {
    let mut bare_selector = None;
    let mut keyword_segments = Vec::new();
    let mut cursor = node.walk();

    let direct_named_children = node.named_children(&mut cursor).collect::<Vec<_>>();
    for (index, child) in direct_named_children.iter().enumerate() {
        match child.kind() {
            "identifier" => {
                let next_kind = direct_named_children.get(index + 1).map(|next| next.kind());
                if next_kind == Some("method_parameter") {
                    keyword_segments.push(format!(
                        "{}:",
                        node_text(source, child).trim_end_matches(':')
                    ));
                } else {
                    bare_selector.get_or_insert_with(|| node_text(source, child).to_string());
                }
            }
            "keyword_declarator" => {
                if let Some(segment) = objc_first_direct_identifier(source, child) {
                    keyword_segments.push(format!("{}:", segment.trim_end_matches(':')));
                }
            }
            _ => {}
        }
    }

    if keyword_segments.is_empty() {
        bare_selector
    } else {
        Some(keyword_segments.join(""))
    }
}

fn objc_property_name(source: &str, node: &Node) -> Option<String> {
    find_child_by_kind(*node, "atomic_declaration")
        .as_ref()
        .and_then(|declaration| objc_declarator_name(source, declaration))
        .or_else(|| objc_declarator_name(source, node))
}

fn objc_function_name(source: &str, node: &Node) -> Option<String> {
    node.child_by_field_name("declarator")
        .as_ref()
        .and_then(|declarator| objc_declarator_name(source, declarator))
}

fn objc_typedef_name(source: &str, node: &Node) -> Option<String> {
    node.child_by_field_name("declarator")
        .as_ref()
        .and_then(|declarator| objc_declarator_name(source, declarator))
}

fn objc_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut scopes = Vec::new();
    let mut current = node.parent();
    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "class_interface" | "class_implementation" | "protocol_declaration"
        ) {
            if let Some(name) = objc_container_name(source, &parent) {
                scopes.push(name);
            }
        }
        current = parent.parent();
    }
    scopes.reverse();
    scopes
}

fn objc_first_direct_identifier(source: &str, node: &Node) -> Option<String> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "identifier" {
            return Some(node_text(source, &child).to_string());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

fn objc_declarator_name(source: &str, node: &Node) -> Option<String> {
    if matches!(
        node.kind(),
        "identifier" | "field_identifier" | "type_identifier"
    ) {
        return Some(node_text(source, node).to_string());
    }

    if let Some(declarator) = node.child_by_field_name("declarator") {
        if let Some(name) = objc_declarator_name(source, &declarator) {
            return Some(name);
        }
    }

    (0..node.child_count()).rev().find_map(|child_index| {
        let child = node.child(child_index as u32)?;
        if child.is_named() {
            objc_declarator_name(source, &child)
        } else {
            None
        }
    })
}

/// Return the first non-comment value in a JSON document.
///
/// JSONC permits comments before the document value. Tree-sitter exposes those
/// comments as named children, so callers must not assume `named_child(0)` is the
/// object or array that contains the document's data.
pub(crate) fn json_document_value<'a>(root: &Node<'a>) -> Option<Node<'a>> {
    let mut cursor = root.walk();
    let value = root
        .named_children(&mut cursor)
        .find(|child| child.kind() != "comment");
    value
}

fn extract_json_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let Some(value) = json_document_value(root) else {
        return Ok(Vec::new());
    };

    if value.kind() != "object" {
        return Ok(Vec::new());
    }

    let mut symbols = Vec::new();
    let mut cursor = value.walk();
    for child in value.named_children(&mut cursor) {
        if child.kind() != "pair" {
            continue;
        }
        let Some(key_node) = child.child_by_field_name("key") else {
            continue;
        };
        let name = node_text(source, &key_node).trim_matches('"').to_string();
        if name.is_empty() {
            continue;
        }
        symbols.push(Symbol {
            name,
            kind: SymbolKind::Variable,
            range: node_range_with_decorators(&child, source, LangId::Json),
            signature: None,
            scope_chain: vec![],
            exported: false,
            parent: None,
        });
    }

    Ok(symbols)
}

/// Read a YAML scalar/key node as trimmed text, stripping surrounding quotes.
fn yaml_scalar_text(source: &str, node: &Node) -> String {
    node_text(source, node)
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}

/// Find the mapping that is the direct payload of `node`, descending only
/// through wrapper nodes (`document`, `block_node`, `flow_node`) rather than
/// arbitrary nested mappings. This keeps detection anchored to the document's
/// top-level mapping instead of latching onto a mapping buried inside a
/// sequence or nested value.
fn yaml_find_mapping<'a>(node: &Node<'a>) -> Option<Node<'a>> {
    let mut current = *node;
    loop {
        match current.kind() {
            "block_mapping" | "flow_mapping" => return Some(current),
            "document" | "block_node" | "flow_node" => {
                let mut cursor = current.walk();
                let next = current
                    .named_children(&mut cursor)
                    .find(|child| !matches!(child.kind(), "tag" | "anchor"));
                match next {
                    Some(inner) => current = inner,
                    None => return None,
                }
            }
            _ => return None,
        }
    }
}

/// Look up a top-level key in a YAML mapping and return its value node.
fn yaml_get<'a>(mapping: &Node<'a>, source: &str, key: &str) -> Option<Node<'a>> {
    let mut cursor = mapping.walk();
    for pair in mapping.named_children(&mut cursor) {
        if pair.kind() != "block_mapping_pair" && pair.kind() != "flow_pair" {
            continue;
        }
        let Some(key_node) = pair.child_by_field_name("key") else {
            continue;
        };
        if yaml_scalar_text(source, &key_node) == key {
            return pair.child_by_field_name("value");
        }
    }
    None
}

/// Flatten a YAML value node to clean text for embed_text. Scalars return their
/// trimmed text; block/flow sequences are joined as comma-separated scalar items
/// (so `verbs: [get, list, watch]` becomes `get,list,watch` instead of raw
/// multi-line `- get\n- list` text). Nested mappings are ignored here.
fn yaml_unwrap<'a>(node: &Node<'a>) -> Node<'a> {
    let mut current = *node;
    while current.kind() == "block_node" || current.kind() == "flow_node" {
        let mut cursor = current.walk();
        let next = current
            .named_children(&mut cursor)
            .find(|child| !matches!(child.kind(), "tag" | "anchor"));
        match next {
            Some(inner) => current = inner,
            None => break,
        }
    }
    current
}

fn yaml_flatten_value(source: &str, node: &Node) -> String {
    let node = &yaml_unwrap(node);
    match node.kind() {
        "block_sequence" | "flow_sequence" => {
            let mut items = Vec::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                // block_sequence_item wraps a value; flow_sequence holds nodes directly.
                let value = if child.kind() == "block_sequence_item" {
                    child.named_child(0)
                } else {
                    Some(child)
                };
                if let Some(v) = value {
                    let v = yaml_unwrap(&v);
                    // Only flatten scalar leaves; skip mappings (handled elsewhere).
                    if v.kind() != "block_mapping" && v.kind() != "flow_mapping" {
                        let text = yaml_scalar_text(source, &v);
                        if !text.is_empty() {
                            items.push(text);
                        }
                    }
                }
            }
            items.join(",")
        }
        // Mapping values (e.g. container `resources:` block) are not flattened
        // here — their high-signal leaves (cpu/memory) are collected separately.
        // Returning empty avoids dumping raw multi-line YAML into embed_text.
        "block_mapping" | "flow_mapping" => String::new(),
        _ => yaml_scalar_text(source, node),
    }
}

/// Recursively collect `key=value` pairs for the given keys (e.g. image, cpu,
/// memory, verbs) to enrich embed_text for semantic search. Sequence values are
/// flattened to comma-joined scalars. Capped to avoid runaway output.
fn yaml_collect_values(
    source: &str,
    node: &Node,
    keys: &[&str],
    out: &mut Vec<String>,
    cap: usize,
) {
    if out.len() >= cap {
        return;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "block_mapping_pair" || child.kind() == "flow_pair" {
            if let Some(key_node) = child.child_by_field_name("key") {
                let key_text = yaml_scalar_text(source, &key_node);
                if keys.contains(&key_text.as_str()) {
                    if let Some(value_node) = child.child_by_field_name("value") {
                        let value_text = yaml_flatten_value(source, &value_node);
                        if !value_text.is_empty() && out.len() < cap {
                            out.push(format!("{}={}", key_text, value_text));
                        }
                    }
                }
            }
        }
        yaml_collect_values(source, &child, keys, out, cap);
    }
}

/// Collect the `name:` field from every item of a `<parent_key>:` sequence of
/// mappings. The bare `name` key is too generic to match globally (it collides
/// with metadata/container names), so these are gathered by parent key and
/// emitted as `<label>=A,B,...`. Handles k8s `env: [{name,value}]` and Argo
/// Workflow `templates: [{name, ...}]`. Capped.
fn yaml_collect_named_items(
    source: &str,
    node: &Node,
    parent_key: &str,
    label: &str,
    out: &mut Vec<String>,
    cap: usize,
) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if (child.kind() == "block_mapping_pair" || child.kind() == "flow_pair")
            && child
                .child_by_field_name("key")
                .map(|k| yaml_scalar_text(source, &k))
                .as_deref()
                == Some(parent_key)
        {
            if let Some(seq) = child.child_by_field_name("value") {
                let seq = yaml_unwrap(&seq);
                let mut names = Vec::new();
                let mut seq_cursor = seq.walk();
                for item in seq.named_children(&mut seq_cursor) {
                    let value = if item.kind() == "block_sequence_item" {
                        item.named_child(0)
                    } else {
                        Some(item)
                    };
                    if let Some(v) = value {
                        if let Some(map) = yaml_find_mapping(&v) {
                            if let Some(name_node) = yaml_get(&map, source, "name") {
                                let n = yaml_scalar_text(source, &name_node);
                                if !n.is_empty() && names.len() < 16 {
                                    names.push(n);
                                }
                            }
                        }
                    }
                }
                if !names.is_empty() && out.len() < cap {
                    out.push(format!("{}={}", label, names.join(",")));
                }
            }
        }
        yaml_collect_named_items(source, &child, parent_key, label, out, cap);
    }
}

/// Tier 1: if a document is a Kubernetes resource (has both apiVersion + kind),
/// emit one rich symbol named `<ns>/<Kind>/<name>` with enriched signature.
/// Generalizes to arbitrary CRDs since apiVersion+kind is the CRD contract.
fn yaml_k8s_resource_symbol(source: &str, doc: &Node, mapping: &Node) -> Option<Symbol> {
    let api_version =
        yaml_get(mapping, source, "apiVersion").map(|n| yaml_scalar_text(source, &n))?;
    let kind = yaml_get(mapping, source, "kind").map(|n| yaml_scalar_text(source, &n))?;
    if api_version.is_empty() || kind.is_empty() {
        return None;
    }

    let (name, namespace) = match yaml_get(mapping, source, "metadata") {
        Some(meta) => match yaml_find_mapping(&meta) {
            Some(meta_map) => {
                // Prefer `name`; fall back to `generateName` (common in Argo
                // Workflows submitted without a fixed name) so the symbol still
                // carries an identifier instead of collapsing to bare <Kind>.
                let name = yaml_get(&meta_map, source, "name")
                    .map(|n| yaml_scalar_text(source, &n))
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        yaml_get(&meta_map, source, "generateName")
                            .map(|n| yaml_scalar_text(source, &n))
                            .filter(|s| !s.is_empty())
                    });
                (
                    name,
                    yaml_get(&meta_map, source, "namespace").map(|n| yaml_scalar_text(source, &n)),
                )
            }
            None => (None, None),
        },
        None => (None, None),
    };

    let res_name = name.clone().filter(|s| !s.is_empty());
    let sym_name = match (&namespace, &res_name) {
        (Some(ns), Some(n)) if !ns.is_empty() => format!("{}/{}/{}", ns, kind, n),
        (_, Some(n)) => format!("{}/{}", kind, n),
        _ => kind.clone(),
    };

    let mut sig = format!("apiVersion={} kind={}", api_version, kind);
    if let Some(ns) = namespace.as_ref().filter(|s| !s.is_empty()) {
        sig.push_str(&format!(" namespace={}", ns));
    }
    if let Some(n) = res_name.as_ref() {
        sig.push_str(&format!(" name={}", n));
    }
    // Enrich with high-signal spec fields so intent queries match. Covers
    // containers (image/ports), resource limits/requests (cpu/memory), RBAC
    // rules (verbs/resources/apiGroups), and storage (volumeMounts mountPath).
    let mut extras = Vec::new();
    yaml_collect_values(
        source,
        mapping,
        &[
            "image",
            "containerPort",
            "port",
            "targetPort",
            "cpu",
            "memory",
            "storage",
            "verbs",
            "resources",
            "apiGroups",
            "mountPath",
            "replicas",
            // Argo Workflow high-signal scalars.
            "entrypoint",
            "command",
            "args",
            "schedule",
        ],
        &mut extras,
        24,
    );
    // Sequence-of-mappings whose `name` key is ambiguous: collect by parent key.
    // k8s env vars and Argo Workflow templates both use the `{name: ...}` shape.
    yaml_collect_named_items(source, mapping, "env", "env", &mut extras, 24);
    yaml_collect_named_items(source, mapping, "templates", "templates", &mut extras, 24);
    if !extras.is_empty() {
        sig.push(' ');
        sig.push_str(&extras.join(" "));
    }

    Some(Symbol {
        name: sym_name,
        kind: SymbolKind::Class,
        range: node_range(doc),
        signature: Some(sig),
        scope_chain: vec![],
        exported: true,
        parent: None,
    })
}

/// Tier 2: generic YAML — emit top-level mapping keys as Variable symbols
/// (docker-compose services, CI jobs, Helm values.yaml, etc.).
fn yaml_generic_keys(source: &str, mapping: &Node, symbols: &mut Vec<Symbol>) {
    let mut cursor = mapping.walk();
    for pair in mapping.named_children(&mut cursor) {
        if pair.kind() != "block_mapping_pair" && pair.kind() != "flow_pair" {
            continue;
        }
        let Some(key_node) = pair.child_by_field_name("key") else {
            continue;
        };
        let name = yaml_scalar_text(source, &key_node);
        if name.is_empty() {
            continue;
        }
        symbols.push(Symbol {
            name,
            kind: SymbolKind::Variable,
            range: node_range(&pair),
            signature: None,
            scope_chain: vec![],
            exported: false,
            parent: None,
        });
    }
}

/// Extract symbols from a YAML stream. Handles multi-document (`---`) streams:
/// each document becomes a Kubernetes resource symbol (Tier 1) when it carries
/// apiVersion+kind, otherwise its top-level keys are emitted (Tier 2). Helm/Go
/// templated YAML that yields no parseable mapping degrades gracefully (the
/// document is skipped rather than failing the whole file).
fn extract_yaml_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let mut symbols = Vec::new();
    let mut cursor = root.walk();
    for doc in root.named_children(&mut cursor) {
        if doc.kind() != "document" {
            continue;
        }
        let Some(mapping) = yaml_find_mapping(&doc) else {
            continue;
        };
        if let Some(symbol) = yaml_k8s_resource_symbol(source, &doc, &mapping) {
            symbols.push(symbol);
        } else {
            yaml_generic_keys(source, &mapping, &mut symbols);
        }
    }
    Ok(symbols)
}

fn scala_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        match parent.kind() {
            "class_definition" | "object_definition" | "enum_definition" | "trait_definition" => {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    chain.push(node_text(source, &name_node).to_string());
                }
            }
            _ => {}
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

fn extract_scala_symbols(
    source: &str,
    root: &Node,
    query: &Query,
) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Scala;
    let capture_names = query.capture_names();

    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut object_name_node = None;
        let mut object_def_node = None;
        let mut enum_name_node = None;
        let mut enum_def_node = None;
        let mut trait_name_node = None;
        let mut trait_def_node = None;
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut val_name_node = None;
        let mut val_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;
        let mut type_name_node = None;
        let mut type_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "object.name" => object_name_node = Some(cap.node),
                "object.def" => object_def_node = Some(cap.node),
                "enum.name" => enum_name_node = Some(cap.node),
                "enum.def" => enum_def_node = Some(cap.node),
                "trait.name" => trait_name_node = Some(cap.node),
                "trait.def" => trait_def_node = Some(cap.node),
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "val.name" => val_name_node = Some(cap.node),
                "val.def" => val_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                "given.name" => val_name_node = Some(cap.node),
                "given.def" => val_def_node = Some(cap.node),
                "type.name" => type_name_node = Some(cap.node),
                "type.def" => type_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scala_scope_chain(&def_node, source),
                exported: true,
                parent: scala_scope_chain(&def_node, source).last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (object_name_node, object_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scala_scope_chain(&def_node, source),
                exported: true,
                parent: scala_scope_chain(&def_node, source).last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Enum,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scala_scope_chain(&def_node, source),
                exported: true,
                parent: scala_scope_chain(&def_node, source).last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (trait_name_node, trait_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Interface,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: scala_scope_chain(&def_node, source),
                exported: true,
                parent: scala_scope_chain(&def_node, source).last().cloned(),
            });
        }

        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope_chain = scala_scope_chain(&def_node, source);
            let kind = if scope_chain.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }

        if let (Some(name_node), Some(def_node)) = (val_name_node, val_def_node) {
            let scope_chain = scala_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Variable,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }

        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            let scope_chain = scala_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Variable,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }

        if let (Some(name_node), Some(def_node)) = (type_name_node, type_def_node) {
            let scope_chain = scala_scope_chain(&def_node, source);
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::TypeAlias,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                parent: scope_chain.last().cloned(),
                scope_chain,
                exported: true,
            });
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn child_text_by_field_or_kind(
    node: &Node,
    source: &str,
    field_name: &str,
    kinds: &[&str],
) -> Option<String> {
    if let Some(name_node) = node.child_by_field_name(field_name) {
        return Some(node_text(source, &name_node).to_string());
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let child = cursor.node();
        if kinds.contains(&child.kind()) {
            return Some(node_text(source, &child).to_string());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

fn push_captured_symbol(
    symbols: &mut Vec<Symbol>,
    source: &str,
    lang: LangId,
    name_node: Node,
    def_node: Node,
    kind: SymbolKind,
    scope_chain: Vec<String>,
    exported: bool,
) {
    symbols.push(Symbol {
        name: node_text(source, &name_node).to_string(),
        kind,
        range: node_range_with_decorators(&def_node, source, lang),
        signature: Some(extract_signature(source, &def_node)),
        parent: scope_chain.last().cloned(),
        scope_chain,
        exported,
    });
}

fn range_spanning_nodes(start_node: &Node, end_node: &Node) -> Range {
    let start = start_node.start_position();
    let end = end_node.end_position();
    Range {
        start_line: start.row as u32,
        start_col: start.column as u32,
        end_line: end.row as u32,
        end_col: end.column as u32,
    }
}

fn extract_signature_between(source: &str, start_node: &Node, end_node: &Node) -> String {
    let start = start_node.start_byte();
    let end = end_node.end_byte();
    let text = source.get(start..end).unwrap_or_default();
    let first_line = text.lines().next().unwrap_or(text);
    let trimmed = first_line.trim_end();
    let trimmed = trimmed.strip_suffix('{').unwrap_or(trimmed).trim_end();
    trimmed.to_string()
}

fn groovy_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "class_declaration"
                | "interface_declaration"
                | "trait_declaration"
                | "enum_declaration"
                | "record_declaration"
                | "annotation_type_declaration"
        ) {
            if let Some(name_node) = parent.child_by_field_name("name") {
                chain.push(node_text(source, &name_node).to_string());
            }
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

// Gradle's `task foo { ... }` syntax parses as an expression statement for the
// `task` keyword followed by a separate command-chain node for the task name and
// body. Pair those adjacent siblings so outline shows the task as one symbol.
fn groovy_keyword_task_symbol(
    source: &str,
    keyword_stmt: Node,
    declaration: Node,
) -> Option<Symbol> {
    if keyword_stmt.kind() != "expression_statement" || declaration.kind() != "command_chain" {
        return None;
    }

    let keyword = keyword_stmt.named_child(0)?;
    if keyword.kind() != "identifier" || node_text(source, &keyword) != "task" {
        return None;
    }

    let name_node = declaration.child_by_field_name("receiver")?;
    if name_node.kind() != "identifier" {
        return None;
    }

    let body_node = declaration.child_by_field_name("argument")?;
    if body_node.kind() != "closure" {
        return None;
    }

    Some(Symbol {
        name: node_text(source, &name_node).to_string(),
        kind: SymbolKind::Function,
        range: range_spanning_nodes(&keyword_stmt, &declaration),
        signature: Some(extract_signature_between(
            source,
            &keyword_stmt,
            &declaration,
        )),
        scope_chain: vec![],
        exported: true,
        parent: None,
    })
}

// `tasks.register("foo") { ... }` likewise arrives as two adjacent siblings:
// the registration call and a trailing closure expression. Pair them into one
// task symbol without scanning beyond the immediate statement pair.
fn groovy_registered_task_symbol(
    source: &str,
    invocation_stmt: Node,
    closure_stmt: Node,
) -> Option<Symbol> {
    if invocation_stmt.kind() != "expression_statement"
        || closure_stmt.kind() != "expression_statement"
    {
        return None;
    }

    let invocation = invocation_stmt.named_child(0)?;
    if invocation.kind() != "method_invocation" {
        return None;
    }

    let function = invocation.child_by_field_name("function")?;
    if function.kind() != "field_access" {
        return None;
    }

    let object = function.child_by_field_name("object")?;
    let field = function.child_by_field_name("field")?;
    if node_text(source, &object) != "tasks" || node_text(source, &field) != "register" {
        return None;
    }

    let arguments = invocation.child_by_field_name("arguments")?;
    let name_node = arguments.named_child(0)?;
    if name_node.kind() != "string_literal" {
        return None;
    }

    let closure = closure_stmt.named_child(0)?;
    if closure.kind() != "closure" {
        return None;
    }

    Some(Symbol {
        name: string_content(source, &name_node)?,
        kind: SymbolKind::Function,
        range: range_spanning_nodes(&invocation_stmt, &closure_stmt),
        signature: Some(extract_signature_between(
            source,
            &invocation_stmt,
            &closure_stmt,
        )),
        scope_chain: vec![],
        exported: true,
        parent: None,
    })
}

fn collect_groovy_task_symbols(source: &str, root: &Node, symbols: &mut Vec<Symbol>) {
    let children: Vec<Node> = root.named_children(&mut root.walk()).collect();
    let mut index = 0;
    while index + 1 < children.len() {
        if let Some(symbol) =
            groovy_keyword_task_symbol(source, children[index], children[index + 1])
        {
            symbols.push(symbol);
            index += 2;
            continue;
        }

        if let Some(symbol) =
            groovy_registered_task_symbol(source, children[index], children[index + 1])
        {
            symbols.push(symbol);
            index += 2;
            continue;
        }

        index += 1;
    }
}

fn java_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        if matches!(
            parent.kind(),
            "class_declaration"
                | "interface_declaration"
                | "annotation_type_declaration"
                | "enum_declaration"
                | "record_declaration"
        ) {
            if let Some(name_node) = parent.child_by_field_name("name") {
                chain.push(node_text(source, &name_node).to_string());
            }
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

fn extract_java_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Java;
    let capture_names = query.capture_names();
    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut interface_name_node = None;
        let mut interface_def_node = None;
        let mut enum_name_node = None;
        let mut enum_def_node = None;
        let mut struct_name_node = None;
        let mut struct_def_node = None;
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "interface.name" => interface_name_node = Some(cap.node),
                "interface.def" => interface_def_node = Some(cap.node),
                "enum.name" => enum_name_node = Some(cap.node),
                "enum.def" => enum_def_node = Some(cap.node),
                "struct.name" => struct_name_node = Some(cap.node),
                "struct.def" => struct_def_node = Some(cap.node),
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Class,
                java_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (interface_name_node, interface_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Interface,
                java_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Enum,
                java_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (struct_name_node, struct_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Struct,
                java_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope_chain = java_scope_chain(&def_node, source);
            let kind = if scope_chain.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                kind,
                scope_chain,
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Variable,
                java_scope_chain(&def_node, source),
                true,
            );
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn ruby_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        if matches!(parent.kind(), "class" | "module") {
            if let Some(name_node) = parent.child_by_field_name("name") {
                chain.push(node_text(source, &name_node).to_string());
            }
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

fn extract_ruby_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Ruby;
    let capture_names = query.capture_names();
    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut module_name_node = None;
        let mut module_def_node = None;
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "module.name" => module_name_node = Some(cap.node),
                "module.def" => module_def_node = Some(cap.node),
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (module_name_node, module_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Class,
                ruby_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Class,
                ruby_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope_chain = ruby_scope_chain(&def_node, source);
            let kind = if scope_chain.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                kind,
                scope_chain,
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Variable,
                ruby_scope_chain(&def_node, source),
                true,
            );
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn kotlin_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        if matches!(parent.kind(), "class_declaration" | "object_declaration") {
            if let Some(name) =
                child_text_by_field_or_kind(&parent, source, "name", &["type_identifier"])
            {
                chain.push(name);
            }
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

fn extract_kotlin_symbols(
    source: &str,
    root: &Node,
    query: &Query,
) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Kotlin;
    let capture_names = query.capture_names();
    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut object_name_node = None;
        let mut object_def_node = None;
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;
        let mut type_name_node = None;
        let mut type_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "object.name" => object_name_node = Some(cap.node),
                "object.def" => object_def_node = Some(cap.node),
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                "type.name" => type_name_node = Some(cap.node),
                "type.def" => type_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Class,
                kotlin_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (object_name_node, object_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Class,
                kotlin_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope_chain = kotlin_scope_chain(&def_node, source);
            let kind = if scope_chain.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                kind,
                scope_chain,
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Variable,
                kotlin_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (type_name_node, type_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::TypeAlias,
                kotlin_scope_chain(&def_node, source),
                true,
            );
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn swift_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        if matches!(parent.kind(), "class_declaration" | "protocol_declaration") {
            if let Some(name_node) = parent.child_by_field_name("name") {
                chain.push(node_text(source, &name_node).to_string());
            }
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

fn swift_type_kind(source: &str, node: &Node) -> SymbolKind {
    match node
        .child_by_field_name("declaration_kind")
        .map(|kind_node| node_text(source, &kind_node))
    {
        Some("struct") => SymbolKind::Struct,
        Some("enum") => SymbolKind::Enum,
        _ => SymbolKind::Class,
    }
}

fn extract_swift_symbols(
    source: &str,
    root: &Node,
    query: &Query,
) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Swift;
    let capture_names = query.capture_names();
    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut interface_name_node = None;
        let mut interface_def_node = None;
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;
        let mut type_name_node = None;
        let mut type_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "interface.name" => interface_name_node = Some(cap.node),
                "interface.def" => interface_def_node = Some(cap.node),
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                "type.name" => type_name_node = Some(cap.node),
                "type.def" => type_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            let kind = swift_type_kind(source, &def_node);
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                kind,
                swift_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (interface_name_node, interface_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Interface,
                swift_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope_chain = swift_scope_chain(&def_node, source);
            let kind = if scope_chain.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                kind,
                scope_chain,
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Variable,
                swift_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (type_name_node, type_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::TypeAlias,
                swift_scope_chain(&def_node, source),
                true,
            );
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn php_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        match parent.kind() {
            "namespace_definition"
            | "class_declaration"
            | "interface_declaration"
            | "trait_declaration"
            | "enum_declaration" => {
                if let Some(name_node) = parent.child_by_field_name("name") {
                    chain.push(node_text(source, &name_node).to_string());
                }
            }
            _ => {}
        }
        current = parent.parent();
    }

    chain.reverse();
    chain
}

fn extract_scss_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Scss;
    let capture_names = query.capture_names();
    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut mixin_name_node = None;
        let mut mixin_def_node = None;
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;
        let mut selector_name_node = None;
        let mut selector_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "mixin.name" => mixin_name_node = Some(cap.node),
                "mixin.def" => mixin_def_node = Some(cap.node),
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                "selector.name" => selector_name_node = Some(cap.node),
                "selector.def" => selector_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (mixin_name_node, mixin_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Function,
                vec![],
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Function,
                vec![],
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            if !node_text(source, &name_node).starts_with('$') {
                continue;
            }
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Variable,
                vec![],
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (selector_name_node, selector_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Class,
                vec![],
                true,
            );
        }
    }

    Ok(symbols)
}

fn extract_php_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Php;
    let capture_names = query.capture_names();
    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut namespace_name_node = None;
        let mut namespace_def_node = None;
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut interface_name_node = None;
        let mut interface_def_node = None;
        let mut trait_name_node = None;
        let mut trait_def_node = None;
        let mut enum_name_node = None;
        let mut enum_def_node = None;
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "namespace.name" => namespace_name_node = Some(cap.node),
                "namespace.def" => namespace_def_node = Some(cap.node),
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "interface.name" => interface_name_node = Some(cap.node),
                "interface.def" => interface_def_node = Some(cap.node),
                "trait.name" => trait_name_node = Some(cap.node),
                "trait.def" => trait_def_node = Some(cap.node),
                "enum.name" => enum_name_node = Some(cap.node),
                "enum.def" => enum_def_node = Some(cap.node),
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (namespace_name_node, namespace_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Class,
                php_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Class,
                php_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (interface_name_node, interface_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Interface,
                php_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (trait_name_node, trait_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Interface,
                php_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Enum,
                php_scope_chain(&def_node, source),
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope_chain = php_scope_chain(&def_node, source);
            let kind = if scope_chain.is_empty() || def_node.kind() == "function_definition" {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                kind,
                scope_chain,
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Variable,
                php_scope_chain(&def_node, source),
                true,
            );
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn lua_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();

    if node.kind() == "function_declaration" {
        if let Some(name_node) = node.child_by_field_name("name") {
            match name_node.kind() {
                "dot_index_expression" | "method_index_expression" => {
                    if let Some(table_node) = name_node.child_by_field_name("table") {
                        chain.push(node_text(source, &table_node).to_string());
                    }
                }
                _ => {}
            }
        }
    }

    chain
}

fn extract_lua_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Lua;
    let capture_names = query.capture_names();
    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                _ => {}
            }
        }

        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope_chain = lua_scope_chain(&def_node, source);
            let kind = if scope_chain.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                kind,
                scope_chain,
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Variable,
                vec![],
                true,
            );
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn perl_package_name(source: &str, node: &Node) -> Option<String> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "package" {
            return Some(node_text(source, &child).to_string());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

fn perl_scope_chain(node: &Node, source: &str) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = node.parent();

    while let Some(parent) = current {
        if parent.kind() == "package_statement" {
            if let Some(name) = perl_package_name(source, &parent) {
                chain.push(name);
            }
        }
        current = parent.parent();
    }

    if chain.is_empty() && node.kind() != "package_statement" {
        let mut sibling = node.prev_sibling();
        while let Some(prev) = sibling {
            if prev.kind() == "package_statement" {
                if let Some(name) = perl_package_name(source, &prev) {
                    chain.push(name);
                }
                break;
            }
            sibling = prev.prev_sibling();
        }
    }

    chain.reverse();
    chain
}

fn extract_perl_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Perl;
    let capture_names = query.capture_names();
    let mut symbols = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, *root, source.as_bytes());

    while let Some(m) = {
        matches.advance();
        matches.get()
    } {
        let mut package_name_node = None;
        let mut package_def_node = None;
        let mut fn_name_node = None;
        let mut fn_def_node = None;
        let mut var_name_node = None;
        let mut var_def_node = None;
        let mut const_pragma_node = None;
        let mut const_name_node = None;
        let mut const_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "package.name" => package_name_node = Some(cap.node),
                "package.def" => package_def_node = Some(cap.node),
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "var.name" => var_name_node = Some(cap.node),
                "var.def" => var_def_node = Some(cap.node),
                "const.pragma" => const_pragma_node = Some(cap.node),
                "const.name" => const_name_node = Some(cap.node),
                "const.def" => const_def_node = Some(cap.node),
                _ => {}
            }
        }

        // `use constant NAME => ...;` defines a constant. Our grammar parses every
        // `use`/`no` pragma as a generic `use_statement`, so gate on the pragma
        // module text to avoid treating e.g. `use parent -norequire, ...` as a
        // constant definition.
        if let (Some(pragma_node), Some(name_node), Some(def_node)) =
            (const_pragma_node, const_name_node, const_def_node)
        {
            if node_text(source, &pragma_node) == "constant" {
                var_name_node = Some(name_node);
                var_def_node = Some(def_node);
            }
        }

        if let (Some(name_node), Some(def_node)) = (package_name_node, package_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Class,
                vec![],
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let scope_chain = perl_scope_chain(&def_node, source);
            let kind = if scope_chain.is_empty() {
                SymbolKind::Function
            } else {
                SymbolKind::Method
            };
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                kind,
                scope_chain,
                true,
            );
        }
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            push_captured_symbol(
                &mut symbols,
                source,
                lang,
                name_node,
                def_node,
                SymbolKind::Variable,
                perl_scope_chain(&def_node, source),
                true,
            );
        }
    }

    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn extract_vue_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let mut symbols = Vec::new();
    collect_vue_sections(source, root, &mut symbols, true);
    dedup_symbols(&mut symbols);
    Ok(symbols)
}

fn collect_vue_sections(
    source: &str,
    node: &Node,
    symbols: &mut Vec<Symbol>,
    allow_sections: bool,
) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        if let Some(section_name) = vue_section_name(&child) {
            if allow_sections {
                symbols.push(Symbol {
                    name: section_name.to_string(),
                    kind: SymbolKind::Heading,
                    range: node_range(&child),
                    signature: vue_opening_tag_signature(source, &child),
                    scope_chain: vec![],
                    exported: false,
                    parent: None,
                });
            }
        } else {
            collect_vue_sections(
                source,
                &child,
                symbols,
                allow_sections && child.kind() == "document",
            );
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn vue_section_name(node: &Node) -> Option<&'static str> {
    match node.kind() {
        "template_element" => Some("template"),
        "script_element" => Some("script"),
        "style_element" => Some("style"),
        _ => None,
    }
}

fn vue_opening_tag_signature(source: &str, node: &Node) -> Option<String> {
    find_child_by_kind(*node, "start_tag")
        .or_else(|| find_child_by_kind(*node, "script_start_tag"))
        .or_else(|| find_child_by_kind(*node, "style_start_tag"))
        .or_else(|| find_child_by_kind(*node, "template_start_tag"))
        .map(|tag| node_text(source, &tag).trim().to_string())
}

fn source_line_end_col(source: &str, line: u32) -> u32 {
    source
        .lines()
        .nth(line as usize)
        .map(|line| line.len() as u32)
        .unwrap_or(0)
}

fn extract_html_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let mut headings = Vec::new();
    collect_html_headings(source, root, &mut headings);

    let total_lines = source.lines().count() as u32;

    // Extend each heading's end_line to just before the next heading at the
    // same or shallower level (or EOF). This makes aft_zoom return the full
    // section content rather than just the heading element's single line.
    for i in 0..headings.len() {
        let level = headings[i].level;
        let section_end = headings[i + 1..]
            .iter()
            .find(|heading| heading.level <= level)
            .map(|heading| heading.symbol.range.start_line.saturating_sub(1))
            .unwrap_or_else(|| total_lines.saturating_sub(1));
        headings[i].symbol.range.end_line = section_end;
        if section_end != headings[i].symbol.range.start_line {
            headings[i].symbol.range.end_col = source_line_end_col(source, section_end);
        }
    }

    // Build hierarchy: assign scope_chain and parent based on heading level
    let mut scope_stack: Vec<(u8, String)> = Vec::new(); // (level, name)
    for heading in &mut headings {
        // Pop scope entries that are at the same level or deeper
        while scope_stack
            .last()
            .is_some_and(|(level, _)| *level >= heading.level)
        {
            scope_stack.pop();
        }
        heading.symbol.scope_chain = scope_stack.iter().map(|(_, name)| name.clone()).collect();
        heading.symbol.parent = scope_stack.last().map(|(_, name)| name.clone());
        scope_stack.push((heading.level, heading.symbol.name.clone()));
    }

    Ok(headings.into_iter().map(|heading| heading.symbol).collect())
}

/// A parsed HTML heading and its explicit `id` or legacy `name` URL anchor.
struct HtmlHeading {
    level: u8,
    symbol: Symbol,
    anchor_id: Option<String>,
}

/// Recursively collect h1-h6 elements from the HTML tree.
fn collect_html_headings(source: &str, node: &Node, headings: &mut Vec<HtmlHeading>) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "element" {
            // Check if this element's start tag is h1-h6
            if let Some(start_tag) = child
                .child_by_field_name("start_tag")
                .or_else(|| child.child(0).filter(|c| c.kind() == "start_tag"))
            {
                if let Some(tag_name_node) = start_tag
                    .child_by_field_name("tag_name")
                    .or_else(|| start_tag.child(1).filter(|c| c.kind() == "tag_name"))
                {
                    let tag_name = node_text(source, &tag_name_node).to_lowercase();
                    if let Some(level) = match tag_name.as_str() {
                        "h1" => Some(1u8),
                        "h2" => Some(2),
                        "h3" => Some(3),
                        "h4" => Some(4),
                        "h5" => Some(5),
                        "h6" => Some(6),
                        _ => None,
                    } {
                        // Extract text content from the element
                        let text = extract_element_text(source, &child).trim().to_string();
                        if !text.is_empty() {
                            let range = node_range(&child);
                            let signature = format!("<h{}> {}", level, text);
                            headings.push(HtmlHeading {
                                level,
                                anchor_id: html_heading_anchor_id(source, &start_tag),
                                symbol: Symbol {
                                    name: text,
                                    kind: SymbolKind::Heading,
                                    range,
                                    signature: Some(signature),
                                    scope_chain: vec![], // filled later
                                    exported: false,
                                    parent: None, // filled later
                                },
                            });
                        }
                    }
                }
            }
            // Recurse into element children (nested headings)
            collect_html_headings(source, &child, headings);
        } else {
            // Recurse into other node types (document, body, etc.)
            collect_html_headings(source, &child, headings);
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

fn extract_html_heading_anchors(source: &str, root: &Node) -> Vec<crate::language::HeadingAnchor> {
    let mut headings = Vec::new();
    collect_html_headings(source, root, &mut headings);
    headings
        .into_iter()
        .filter_map(|heading| {
            heading.anchor_id.map(|id| crate::language::HeadingAnchor {
                start_line: heading.symbol.range.start_line,
                start_col: heading.symbol.range.start_col,
                id,
            })
        })
        .collect()
}

fn html_heading_anchor_id(source: &str, start_tag: &Node) -> Option<String> {
    for expected_name in ["id", "name"] {
        let mut cursor = start_tag.walk();
        for attribute in start_tag.named_children(&mut cursor) {
            if attribute.kind() != "attribute" {
                continue;
            }
            let Some(name_node) = attribute
                .child_by_field_name("name")
                .or_else(|| find_child_by_kind(attribute, "attribute_name"))
            else {
                continue;
            };
            if !node_text(source, &name_node).eq_ignore_ascii_case(expected_name) {
                continue;
            }
            let Some(value_node) = attribute
                .child_by_field_name("value")
                .or_else(|| find_child_by_kind(attribute, "quoted_attribute_value"))
                .or_else(|| find_child_by_kind(attribute, "attribute_value"))
            else {
                continue;
            };
            let raw_value = node_text(source, &value_node).trim();
            let value = raw_value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .or_else(|| {
                    raw_value
                        .strip_prefix('\'')
                        .and_then(|value| value.strip_suffix('\''))
                })
                .unwrap_or(raw_value)
                .trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Extract text content from an HTML element, stripping tags.
fn extract_element_text(source: &str, node: &Node) -> String {
    let mut text = String::new();
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return text;
    }
    loop {
        let child = cursor.node();
        match child.kind() {
            "text" => {
                text.push_str(node_text(source, &child));
            }
            "element" => {
                // Recurse into nested elements (e.g., <strong>, <em>, <a>)
                text.push_str(&extract_element_text(source, &child));
            }
            _ => {}
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    text
}

struct RawHeading {
    name: String,
    level: u8,
    range: Range,
}

fn parse_atx_heading(source: &str, node: &Node) -> Option<(String, u8)> {
    let mut heading_level = 1;
    let mut heading_name = String::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let kind = child.kind();
            if kind.starts_with("atx_h") && kind.ends_with("_marker") {
                heading_level = kind
                    .strip_prefix("atx_h")
                    .and_then(|s| s.strip_suffix("_marker"))
                    .and_then(|s| s.parse::<u8>().ok())
                    .unwrap_or(1);
            } else if kind == "inline" {
                heading_name = node_text(source, &child).trim().to_string();
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    if heading_name.is_empty() {
        None
    } else {
        Some((heading_name, heading_level))
    }
}

fn parse_setext_heading(source: &str, node: &Node) -> Option<(String, u8)> {
    let mut heading_level = 1;
    let mut heading_name = String::new();
    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let kind = child.kind();
            if kind.starts_with("setext_h") && kind.ends_with("_underline") {
                heading_level = kind
                    .strip_prefix("setext_h")
                    .and_then(|s| s.strip_suffix("_underline"))
                    .and_then(|s| s.parse::<u8>().ok())
                    .unwrap_or(1);
            } else if kind == "paragraph" {
                heading_name = node_text(source, &child).trim().to_string();
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
    if heading_name.is_empty() {
        None
    } else {
        Some((heading_name, heading_level))
    }
}

fn collect_headings(source: &str, node: &Node, headings: &mut Vec<RawHeading>) {
    let kind = node.kind();
    if kind == "atx_heading" {
        if let Some((name, level)) = parse_atx_heading(source, node) {
            headings.push(RawHeading {
                name,
                level,
                range: node_range(node),
            });
        }
    } else if kind == "setext_heading" {
        if let Some((name, level)) = parse_setext_heading(source, node) {
            headings.push(RawHeading {
                name,
                level,
                range: node_range(node),
            });
        }
    } else {
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                collect_headings(source, &cursor.node(), headings);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }
}

/// Extract markdown headings as symbols.
/// Each heading becomes a symbol with kind `Heading`, and its range covers the entire
/// section (from the heading to the next heading at the same or higher level, or EOF).
fn extract_md_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let mut raw_headings = Vec::new();
    collect_headings(source, root, &mut raw_headings);

    let mut symbols = Vec::new();
    let total_lines = source.lines().count() as u32;

    let mut active_headings: Vec<Option<String>> = vec![None; 7];

    for i in 0..raw_headings.len() {
        let h = &raw_headings[i];
        let level = h.level as usize;

        // Build scope chain from active headings of lower levels
        let mut scope_chain = Vec::new();
        for l in 1..level {
            if let Some(ref name) = active_headings[l] {
                scope_chain.push(name.clone());
            }
        }

        // Update active headings
        active_headings[level] = Some(h.name.clone());
        for l in (level + 1)..7 {
            active_headings[l] = None;
        }

        // Determine the section range
        let start_line = h.range.start_line;
        let start_col = h.range.start_col;

        // Find the next heading of the same or higher level (level <= h.level)
        let mut end_line = total_lines.saturating_sub(1);
        let mut end_col = if total_lines > 0 {
            source_line_end_col(source, end_line)
        } else {
            0
        };

        for j in (i + 1)..raw_headings.len() {
            if raw_headings[j].level <= h.level {
                // The section ends before this heading starts
                let next_start_line = raw_headings[j].range.start_line;
                end_line = next_start_line.saturating_sub(1).max(start_line);
                end_col = source_line_end_col(source, end_line);
                break;
            }
        }

        let signature = format!("{} {}", "#".repeat((h.level as usize).min(6)), h.name);

        symbols.push(Symbol {
            name: h.name.clone(),
            kind: SymbolKind::Heading,
            range: Range {
                start_line,
                start_col,
                end_line,
                end_col,
            },
            signature: Some(signature),
            scope_chain: scope_chain.clone(),
            exported: false,
            parent: scope_chain.last().cloned(),
        });
    }

    Ok(symbols)
}

/// Remove duplicate symbols based on (name, kind, start_line).
/// Class declarations can match both "class" and "method" patterns,
/// producing duplicates.
fn dedup_symbols(symbols: &mut Vec<Symbol>) {
    let mut seen = std::collections::HashSet::new();
    symbols.retain(|s| {
        let key = (s.name.clone(), format!("{:?}", s.kind), s.range.start_line);
        seen.insert(key)
    });
}

/// Provider that uses tree-sitter for real symbol extraction.
/// Implements the `LanguageProvider` trait from `language.rs`.
pub struct TreeSitterProvider {
    symbol_cache: SharedSymbolCache,
}

#[derive(Debug, Clone)]
struct ReExportTarget {
    file: PathBuf,
    symbol_name: String,
}

impl TreeSitterProvider {
    /// Create a new `TreeSitterProvider` backed by a fresh shared symbol cache.
    pub fn new() -> Self {
        Self::with_symbol_cache(Arc::new(RwLock::new(SymbolCache::new())))
    }

    /// Create a new `TreeSitterProvider` backed by a shared symbol cache.
    pub fn with_symbol_cache(symbol_cache: SharedSymbolCache) -> Self {
        Self { symbol_cache }
    }

    /// Return shared symbol cache entries for status reporting.
    pub fn symbol_cache_len(&self) -> usize {
        self.symbol_cache
            .read()
            .map(|cache| cache.len())
            .unwrap_or(0)
    }

    /// Shared symbol cache backing this provider.
    pub fn symbol_cache(&self) -> SharedSymbolCache {
        Arc::clone(&self.symbol_cache)
    }

    fn resolve_symbol_inner(
        &self,
        parser: &mut FileParser,
        file: &Path,
        name: &str,
        depth: usize,
        visited: &mut HashSet<(PathBuf, String)>,
    ) -> Result<Vec<SymbolMatch>, AftError> {
        if depth > MAX_REEXPORT_DEPTH {
            return Ok(Vec::new());
        }

        let canonical_file = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        if !visited.insert((canonical_file, name.to_string())) {
            return Ok(Vec::new());
        }

        let symbols = parser.extract_symbols(file)?;
        let local_matches = symbol_matches_in_file(file, &symbols, name);
        if !local_matches.is_empty() {
            return Ok(local_matches);
        }

        if name == "default" {
            let default_matches = self.resolve_local_default_export(parser, file, &symbols)?;
            if !default_matches.is_empty() {
                return Ok(default_matches);
            }
        }

        let reexport_targets = self.collect_reexport_targets(parser, file, name)?;
        let mut matches = Vec::new();
        let mut seen = HashSet::new();
        for target in reexport_targets {
            for resolved in self.resolve_symbol_inner(
                parser,
                &target.file,
                &target.symbol_name,
                depth + 1,
                visited,
            )? {
                let key = format!(
                    "{}:{}:{}:{}:{}:{}",
                    resolved.file,
                    resolved.symbol.name,
                    resolved.symbol.range.start_line,
                    resolved.symbol.range.start_col,
                    resolved.symbol.range.end_line,
                    resolved.symbol.range.end_col
                );
                if seen.insert(key) {
                    matches.push(resolved);
                }
            }
        }

        Ok(matches)
    }

    fn collect_reexport_targets(
        &self,
        parser: &mut FileParser,
        file: &Path,
        requested_name: &str,
    ) -> Result<Vec<ReExportTarget>, AftError> {
        let (source, tree, lang) = self.read_parsed_file(parser, file)?;
        if !matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
            return Ok(Vec::new());
        }

        let mut targets = Vec::new();
        let root = tree.root_node();
        let from_dir = file.parent().unwrap_or_else(|| Path::new("."));

        let mut cursor = root.walk();
        if !cursor.goto_first_child() {
            return Ok(targets);
        }

        loop {
            let node = cursor.node();
            if node.kind() == "export_statement" {
                let Some(source_node) = node.child_by_field_name("source") else {
                    if let Some(export_clause) = find_child_by_kind(node, "export_clause") {
                        if let Some(symbol_name) =
                            resolve_export_clause_name(&source, &export_clause, requested_name)
                        {
                            targets.push(ReExportTarget {
                                file: file.to_path_buf(),
                                symbol_name,
                            });
                        }
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                    continue;
                };

                let Some(module_path) = string_content(&source, &source_node) else {
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                    continue;
                };

                let Some(target_file) = resolve_module_path(from_dir, &module_path) else {
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                    continue;
                };

                if let Some(export_clause) = find_child_by_kind(node, "export_clause") {
                    if let Some(symbol_name) =
                        resolve_export_clause_name(&source, &export_clause, requested_name)
                    {
                        targets.push(ReExportTarget {
                            file: target_file,
                            symbol_name,
                        });
                    }
                } else if export_statement_has_wildcard(&source, &node) {
                    targets.push(ReExportTarget {
                        file: target_file,
                        symbol_name: requested_name.to_string(),
                    });
                }
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }

        Ok(targets)
    }

    fn resolve_local_default_export(
        &self,
        parser: &mut FileParser,
        file: &Path,
        symbols: &[Symbol],
    ) -> Result<Vec<SymbolMatch>, AftError> {
        let (source, tree, lang) = self.read_parsed_file(parser, file)?;
        if !matches!(lang, LangId::TypeScript | LangId::Tsx | LangId::JavaScript) {
            return Ok(Vec::new());
        }

        let root = tree.root_node();
        let mut matches = Vec::new();
        let mut seen = HashSet::new();

        let mut cursor = root.walk();
        if !cursor.goto_first_child() {
            return Ok(matches);
        }

        loop {
            let node = cursor.node();
            if node.kind() == "export_statement"
                && node.child_by_field_name("source").is_none()
                && node_contains_token(&source, &node, "default")
            {
                if let Some(target_name) = default_export_target_name(&source, &node) {
                    for symbol_match in symbol_matches_in_file(file, symbols, &target_name) {
                        let key = format!(
                            "{}:{}:{}:{}:{}:{}",
                            symbol_match.file,
                            symbol_match.symbol.name,
                            symbol_match.symbol.range.start_line,
                            symbol_match.symbol.range.start_col,
                            symbol_match.symbol.range.end_line,
                            symbol_match.symbol.range.end_col
                        );
                        if seen.insert(key) {
                            matches.push(symbol_match);
                        }
                    }
                }
            }

            if !cursor.goto_next_sibling() {
                break;
            }
        }

        Ok(matches)
    }

    fn read_parsed_file(
        &self,
        parser: &mut FileParser,
        file: &Path,
    ) -> Result<(String, Tree, LangId), AftError> {
        let current_mtime = std::fs::metadata(file)
            .and_then(|m| m.modified())
            .map_err(|e| AftError::FileNotFound {
                path: format!("{}: {}", file.display(), e),
            })?;
        let source = std::fs::read_to_string(file).map_err(|e| AftError::FileNotFound {
            path: format!("{}: {}", file.display(), e),
        })?;
        let size = source.len() as u64;
        let content_hash = content_hash_for_source(&source);
        let (tree, lang) =
            parser.parse_with_source(file, &source, current_mtime, size, content_hash)?;
        Ok((source, tree.clone(), lang))
    }
}

fn symbol_matches_in_file(file: &Path, symbols: &[Symbol], name: &str) -> Vec<SymbolMatch> {
    symbols
        .iter()
        .filter(|symbol| symbol_query_matches(symbol, name))
        .cloned()
        .map(|symbol| SymbolMatch {
            file: file.display().to_string(),
            symbol,
        })
        .collect()
}

fn symbol_query_matches(symbol: &Symbol, query: &str) -> bool {
    if symbol.name == query {
        return true;
    }

    if !query.contains('.') && !query.contains("::") {
        return false;
    }

    let mut parts = symbol
        .scope_chain
        .iter()
        .filter(|part| !part.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>();
    if parts.is_empty() {
        return false;
    }
    parts.push(symbol.name.as_str());

    parts.join(".") == query || parts.join("::") == query
}

fn string_content(source: &str, node: &Node) -> Option<String> {
    let text = node_text(source, node);
    if text.len() < 2 {
        return None;
    }

    Some(
        text.trim_start_matches(|c| c == '\'' || c == '"')
            .trim_end_matches(|c| c == '\'' || c == '"')
            .to_string(),
    )
}

fn find_child_by_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let child = cursor.node();
        if child.kind() == kind {
            return Some(child);
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

fn resolve_export_clause_name(
    source: &str,
    export_clause: &Node,
    requested_name: &str,
) -> Option<String> {
    let mut cursor = export_clause.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "export_specifier" {
            let (source_name, exported_name) = export_specifier_names(source, &child)?;
            if exported_name == requested_name {
                return Some(source_name);
            }
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

fn export_specifier_names(source: &str, specifier: &Node) -> Option<(String, String)> {
    let source_name = specifier
        .child_by_field_name("name")
        .map(|node| node_text(source, &node).to_string());
    let alias_name = specifier
        .child_by_field_name("alias")
        .map(|node| node_text(source, &node).to_string());

    if let Some(source_name) = source_name {
        let exported_name = alias_name.unwrap_or_else(|| source_name.clone());
        return Some((source_name, exported_name));
    }

    let mut names = Vec::new();
    let mut cursor = specifier.walk();
    if cursor.goto_first_child() {
        loop {
            let child = cursor.node();
            let child_text = node_text(source, &child).trim();
            if matches!(
                child.kind(),
                "identifier" | "type_identifier" | "property_identifier"
            ) || child_text == "default"
            {
                names.push(child_text.to_string());
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }

    match names.as_slice() {
        [name] => Some((name.clone(), name.clone())),
        [source_name, exported_name, ..] => Some((source_name.clone(), exported_name.clone())),
        _ => None,
    }
}

fn export_statement_has_wildcard(source: &str, node: &Node) -> bool {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return false;
    }

    loop {
        if node_text(source, &cursor.node()).trim() == "*" {
            return true;
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    false
}

fn node_contains_token(source: &str, node: &Node, token: &str) -> bool {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return false;
    }

    loop {
        if node_text(source, &cursor.node()).trim() == token {
            return true;
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    false
}

fn default_export_target_name(source: &str, export_stmt: &Node) -> Option<String> {
    if let Some(value_node) = export_stmt.child_by_field_name("value") {
        if let Some(name) = default_export_node_name(source, &value_node) {
            return Some(name);
        }
    }

    if let Some(declaration_node) = export_stmt.child_by_field_name("declaration") {
        if let Some(name) = default_export_node_name(source, &declaration_node) {
            return Some(name);
        }
    }

    let mut cursor = export_stmt.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let child = cursor.node();
        if let Some(name) = default_export_node_name(source, &child) {
            return Some(name);
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

fn default_export_node_name(source: &str, node: &Node) -> Option<String> {
    match node.kind() {
        "function_declaration"
        | "generator_function_declaration"
        | "function_expression"
        | "generator_function"
        | "class_declaration"
        | "class" => node
            .child_by_field_name("name")
            .map(|name_node| node_text(source, &name_node).to_string())
            .or_else(|| Some("default".to_string())),
        "interface_declaration" | "enum_declaration" | "type_alias_declaration" => node
            .child_by_field_name("name")
            .map(|name_node| node_text(source, &name_node).to_string()),
        "lexical_declaration" => lexical_declaration_name(source, node),
        "identifier" | "type_identifier" => {
            let text = node_text(source, node);
            (text != "export" && text != "default").then(|| text.to_string())
        }
        _ => None,
    }
}

fn lexical_declaration_name(source: &str, node: &Node) -> Option<String> {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let child = cursor.node();
        if child.kind() == "variable_declarator" {
            if let Some(name_node) = child.child_by_field_name("name") {
                return Some(node_text(source, &name_node).to_string());
            }
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

impl crate::language::LanguageProvider for TreeSitterProvider {
    fn resolve_symbol(&self, file: &Path, name: &str) -> Result<Vec<SymbolMatch>, AftError> {
        let mut parser = FileParser::with_symbol_cache(self.symbol_cache());
        let matches = self.resolve_symbol_inner(&mut parser, file, name, 0, &mut HashSet::new())?;

        if matches.is_empty() {
            Err(AftError::SymbolNotFound {
                name: name.to_string(),
                file: file.display().to_string(),
            })
        } else {
            Ok(matches)
        }
    }

    fn list_symbols(&self, file: &Path) -> Result<Vec<Symbol>, AftError> {
        let mut parser = FileParser::with_symbol_cache(self.symbol_cache());
        parser.extract_symbols(file)
    }

    fn heading_anchors(
        &self,
        file: &Path,
    ) -> Result<Vec<crate::language::HeadingAnchor>, AftError> {
        if detect_language(file) != Some(LangId::Html) {
            return Ok(Vec::new());
        }
        let mut parser = FileParser::with_symbol_cache(self.symbol_cache());
        let (source, tree, _) = self.read_parsed_file(&mut parser, file)?;
        Ok(extract_html_heading_anchors(&source, &tree.root_node()))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language::LanguageProvider;
    use crate::symbol_cache_disk;
    use std::path::{Path, PathBuf};

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    /// The export walk must not spend stack per tree depth. This runs it on a
    /// deliberately small thread over a tree nested far deeper than the old
    /// recursive walk survived at that size; a return to recursion overflows
    /// here (the abort is deterministic, not a timing flake) instead of in the
    /// daemon, where a URL that parsed as TypeScript did exactly that.
    #[test]
    fn export_walk_survives_deep_trees_on_a_small_stack() {
        const DEPTH: usize = 3000;
        // The export clause sits after the deep expression: the walk is
        // pre-order over the whole tree, so it must come through the nesting
        // to find it, and finding it proves the traversal completed.
        let mut source = String::from("const deep = ");
        source.push_str(&"(".repeat(DEPTH));
        source.push('1');
        source.push_str(&")".repeat(DEPTH));
        source.push_str(";\nfunction shallow() {}\nexport { deep, shallow };\n");

        let exported = std::thread::Builder::new()
            .stack_size(256 * 1024)
            .spawn(move || {
                let grammar = grammar_for(LangId::TypeScript);
                let mut parser = Parser::new();
                parser.set_language(&grammar).unwrap();
                let tree = parser.parse(&source, None).unwrap();
                collect_exported_symbol_names(&source, &tree.root_node())
            })
            .unwrap()
            .join()
            .expect("export walk must not overflow a 256 KiB stack");

        assert!(exported.contains("deep"), "exports: {exported:?}");
        assert!(exported.contains("shallow"), "exports: {exported:?}");
    }

    #[test]
    #[ignore = "manual single-file parser/query phase benchmark"]
    fn profile_rust_parser_and_query_phases() {
        let source = "pub fn tiny() -> bool {\n    true\n}\n";

        let grammar_started = std::time::Instant::now();
        let grammar = grammar_for(LangId::Rust);
        let grammar_elapsed = grammar_started.elapsed();

        let parser_init_started = std::time::Instant::now();
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let parser_init_elapsed = parser_init_started.elapsed();

        let parse_started = std::time::Instant::now();
        let tree = parser.parse(source, None).unwrap();
        let parse_elapsed = parse_started.elapsed();

        let query_compile_started = std::time::Instant::now();
        let query = Query::new(&grammar, RS_QUERY).unwrap();
        let query_compile_elapsed = query_compile_started.elapsed();

        let query_execute_started = std::time::Instant::now();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());
        let mut query_matches = 0;
        while matches.next().is_some() {
            query_matches += 1;
        }
        let query_execute_elapsed = query_execute_started.elapsed();

        let direct_extract_started = std::time::Instant::now();
        let symbols = extract_rs_symbols(source, &tree.root_node()).unwrap();
        let direct_extract_elapsed = direct_extract_started.elapsed();

        eprintln!(
            "rust parser/query phases: grammar={grammar_elapsed:?} parser_init={parser_init_elapsed:?} parse_execute={parse_elapsed:?} query_compile={query_compile_elapsed:?} query_execute={query_execute_elapsed:?} direct_extract={direct_extract_elapsed:?} symbols={}",
            symbols.len()
        );
        assert!(query_matches > 0);
        assert_eq!(symbols.len(), 1);
    }

    fn extract_rs_symbols_with_query_oracle(source: &str, root: Node<'_>) -> Vec<Symbol> {
        let grammar = grammar_for(LangId::Rust);
        let query = Query::new(&grammar, RS_QUERY).expect("compile Rust parity query");
        let capture_names = query.capture_names();
        let is_pub = |node: &Node<'_>| {
            let mut cursor = node.walk();
            if cursor.goto_first_child() {
                loop {
                    if cursor.node().kind() == "visibility_modifier" {
                        return true;
                    }
                    if !cursor.goto_next_sibling() {
                        break;
                    }
                }
            }
            false
        };

        let mut symbols = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut matches = cursor.matches(&query, root, source.as_bytes());
        while let Some(query_match) = {
            matches.advance();
            matches.get()
        } {
            let mut function_name = None;
            let mut function_definition = None;
            let mut item_name = None;
            let mut item_definition = None;
            let mut item_kind = None;
            let mut impl_node = None;
            for capture in query_match.captures {
                let Some(name) = capture_names.get(capture.index as usize) else {
                    continue;
                };
                match *name {
                    "fn.name" => function_name = Some(capture.node),
                    "fn.def" => function_definition = Some(capture.node),
                    "struct.name" => {
                        item_name = Some(capture.node);
                        item_kind = Some(SymbolKind::Struct);
                    }
                    "struct.def" => item_definition = Some(capture.node),
                    "enum.name" => {
                        item_name = Some(capture.node);
                        item_kind = Some(SymbolKind::Enum);
                    }
                    "enum.def" => item_definition = Some(capture.node),
                    "trait.name" => {
                        item_name = Some(capture.node);
                        item_kind = Some(SymbolKind::Interface);
                    }
                    "trait.def" => item_definition = Some(capture.node),
                    "impl.def" => impl_node = Some(capture.node),
                    _ => {}
                }
            }

            if let (Some(name_node), Some(definition)) = (function_name, function_definition) {
                let owner = rust_function_declaration_list_owner(&definition);
                if owner
                    .as_ref()
                    .is_none_or(|owner| owner.kind() == "mod_item")
                {
                    let scope_chain = rust_mod_scope_chain(&definition, source);
                    symbols.push(Symbol {
                        name: node_text(source, &name_node).to_string(),
                        kind: SymbolKind::Function,
                        range: node_range_with_decorators(&definition, source, LangId::Rust),
                        signature: Some(extract_signature(source, &definition)),
                        scope_chain: scope_chain.clone(),
                        exported: is_pub(&definition),
                        parent: scope_chain.last().cloned(),
                    });
                }
            }

            if let (Some(name_node), Some(definition), Some(kind)) =
                (item_name, item_definition, item_kind)
            {
                symbols.push(Symbol {
                    name: node_text(source, &name_node).to_string(),
                    kind,
                    range: node_range_with_decorators(&definition, source, LangId::Rust),
                    signature: Some(extract_signature(source, &definition)),
                    scope_chain: Vec::new(),
                    exported: is_pub(&definition),
                    parent: None,
                });
            }

            if let Some(impl_node) = impl_node {
                let scope_name = rust_impl_scope_name(&impl_node, source);
                let parent_name = scope_name
                    .rsplit(" for ")
                    .next()
                    .unwrap_or_default()
                    .to_string();
                let mut impl_cursor = impl_node.walk();
                if impl_cursor.goto_first_child() {
                    loop {
                        let child = impl_cursor.node();
                        if child.kind() == "declaration_list" {
                            let mut method_cursor = child.walk();
                            if method_cursor.goto_first_child() {
                                loop {
                                    let method = method_cursor.node();
                                    if method.kind() == "function_item" {
                                        if let Some(name_node) = method.child_by_field_name("name")
                                        {
                                            symbols.push(Symbol {
                                                name: node_text(source, &name_node).to_string(),
                                                kind: SymbolKind::Method,
                                                range: node_range_with_decorators(
                                                    &method,
                                                    source,
                                                    LangId::Rust,
                                                ),
                                                signature: Some(extract_signature(source, &method)),
                                                scope_chain: (!scope_name.is_empty())
                                                    .then(|| vec![scope_name.clone()])
                                                    .unwrap_or_default(),
                                                exported: is_pub(&method),
                                                parent: (!parent_name.is_empty())
                                                    .then(|| parent_name.clone()),
                                            });
                                        }
                                    }
                                    if !method_cursor.goto_next_sibling() {
                                        break;
                                    }
                                }
                            }
                        }
                        if !impl_cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }
            }
        }

        dedup_symbols(&mut symbols);
        symbols
    }

    fn rust_symbol_fingerprint(symbols: &[Symbol]) -> Vec<String> {
        symbols
            .iter()
            .map(|symbol| {
                format!(
                    "{}|{:?}|{:?}|{:?}|{:?}|{}|{:?}",
                    symbol.name,
                    symbol.kind,
                    symbol.range,
                    symbol.signature,
                    symbol.scope_chain,
                    symbol.exported,
                    symbol.parent
                )
            })
            .collect()
    }

    #[test]
    fn rust_walk_matches_query_oracle_for_adversarial_shapes() {
        let source = r#"
#[cfg(any())]
pub(crate) mod outer {
    pub mod inner {
        #[doc = "keeps decorator range"]
        pub(crate) fn nested<T>() {}

        pub(crate) struct Generic<T>(T);
        pub enum State { Ready }
        pub trait Service<T> {
            fn declaration(&self);
        }

        impl<T> Service<T> for Generic<T> {
            #[inline]
            pub(crate) fn attributed_method(&self) {}
            fn private_method(&self) {}
        }
    }
}
"#;
        let grammar = grammar_for(LangId::Rust);
        let mut parser = Parser::new();
        parser.set_language(&grammar).unwrap();
        let tree = parser.parse(source, None).unwrap();

        let query_symbols = extract_rs_symbols_with_query_oracle(source, tree.root_node());
        let walk_symbols = extract_rs_symbols(source, &tree.root_node()).unwrap();
        assert_eq!(
            rust_symbol_fingerprint(&walk_symbols),
            rust_symbol_fingerprint(&query_symbols)
        );
        assert!(walk_symbols.iter().any(|symbol| {
            symbol.name == "nested" && symbol.exported && symbol.scope_chain == ["outer", "inner"]
        }));
        assert!(walk_symbols.iter().any(|symbol| {
            symbol.name == "attributed_method"
                && symbol.kind == SymbolKind::Method
                && symbol.scope_chain == ["Service<T> for Generic<T>"]
                && symbol.exported
        }));
        assert!(
            walk_symbols
                .iter()
                .all(|symbol| symbol.name != "declaration"),
            "trait declarations are not implementation methods"
        );
    }

    #[test]
    fn source_parser_is_reused_on_the_current_worker_thread() {
        let first = "pub fn first() {}\n";
        let second = "pub struct Second;\n";
        let path = Path::new("worker.rs");

        let before = REUSABLE_PARSERS.with(|parsers| parsers.borrow().len());
        let first_tree =
            parse_source_with_cached_parser(path, first, LangId::Rust).expect("parse first source");
        let after_first = REUSABLE_PARSERS.with(|parsers| parsers.borrow().len());
        let second_tree = parse_source_with_cached_parser(path, second, LangId::Rust)
            .expect("parse second source");
        let after_second = REUSABLE_PARSERS.with(|parsers| parsers.borrow().len());

        assert!(!first_tree.root_node().has_error());
        assert!(!second_tree.root_node().has_error());
        assert!(after_first == before || after_first == before + 1);
        assert_eq!(after_second, after_first);
    }

    fn test_symbol(name: &str) -> Symbol {
        Symbol {
            name: name.to_string(),
            kind: SymbolKind::Function,
            range: Range {
                start_line: 0,
                start_col: 0,
                end_line: 0,
                end_col: 10,
            },
            signature: Some(format!("fn {name}()")),
            scope_chain: Vec::new(),
            exported: true,
            parent: None,
        }
    }

    #[test]
    fn symbol_memory_estimate_is_zero_when_empty_and_nonzero_when_populated() {
        let mut cache = SymbolCache::new();
        assert_eq!(cache.estimated_memory().estimated_bytes, Some(0));
        cache.insert(
            PathBuf::from("src/lib.rs"),
            SystemTime::UNIX_EPOCH,
            10,
            cache_freshness::zero_hash(),
            vec![test_symbol("memory_estimate")],
        );
        let estimate = cache.estimated_memory();
        assert!(estimate.estimated_bytes.unwrap() > 0);
        assert_eq!(estimate.counts["entries"], 1);
        assert_eq!(estimate.counts["symbols"], 1);
    }

    #[test]
    fn inc_and_scss_extensions_are_detected() {
        assert_eq!(
            detect_language(Path::new("template.inc")),
            Some(LangId::Php)
        );
        assert_eq!(
            detect_language(Path::new("styles.scss")),
            Some(LangId::Scss)
        );
        assert_eq!(detect_language(Path::new("main.pas")), Some(LangId::Pascal));
        assert_eq!(detect_language(Path::new("main.pp")), Some(LangId::Pascal));
        assert_eq!(detect_language(Path::new("main.dpr")), Some(LangId::Pascal));
        assert_eq!(detect_language(Path::new("main.dpk")), Some(LangId::Pascal));
        assert_eq!(detect_language(Path::new("main.lpr")), Some(LangId::Pascal));
        assert_eq!(detect_language(Path::new("template.tpl")), None);
    }

    #[test]
    fn inc_files_parse_with_php_grammar() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let file = tmp.path().join("partial.inc");
        std::fs::write(&file, "<?php\nfunction render_partial() { return 1; }\n")
            .expect("write inc file");

        let mut parser = FileParser::new();
        let symbols = parser
            .extract_symbols(&file)
            .expect("extract php inc symbols");
        let function = symbols
            .iter()
            .find(|symbol| symbol.name == "render_partial")
            .expect("find PHP function in .inc");
        assert_eq!(function.kind, SymbolKind::Function);
    }

    #[test]
    fn scss_symbols_include_mixin_variable_function_and_rule() {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let file = tmp.path().join("styles.scss");
        std::fs::write(
            &file,
            r#"$brand-color: #336699;

@mixin button-base($padding) {
  padding: $padding;
}

@function double($value) {
  @return $value * 2;
}

.card, .panel {
  color: $brand-color;
}
"#,
        )
        .expect("write scss file");

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).expect("extract scss symbols");
        let get = |name: &str| {
            symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing {name}; got {symbols:?}"))
        };

        assert_eq!(get("button-base").kind, SymbolKind::Function);
        assert_eq!(get("double").kind, SymbolKind::Function);
        assert_eq!(get("$brand-color").kind, SymbolKind::Variable);
        assert_eq!(get(".card, .panel").kind, SymbolKind::Class);
    }

    #[test]
    fn symbol_cache_load_from_disk_round_trips_synthetic_entry() {
        let project = tempfile::tempdir().expect("create project dir");
        let storage = tempfile::tempdir().expect("create storage dir");
        let source = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source, "pub fn cached() {}\n").expect("write source");
        let mtime = std::fs::metadata(&source)
            .expect("stat source")
            .modified()
            .expect("source mtime");
        let content = std::fs::read(&source).expect("read source");
        let size = content.len() as u64;
        let hash = crate::cache_freshness::hash_bytes(&content);

        let mut cache = SymbolCache::new();
        cache.set_project_root(project.path().to_path_buf());
        cache.insert(
            source.clone(),
            mtime,
            size,
            hash,
            vec![test_symbol("cached")],
        );
        symbol_cache_disk::write_to_disk(&cache, storage.path(), "unit-project")
            .expect("write symbol cache");

        let mut restored = SymbolCache::new();
        let outcome =
            restored.load_from_disk_with_outcome(storage.path(), "unit-project", project.path());
        let symbols = restored.get(&source, mtime).expect("restored symbols");

        assert_eq!(outcome.loaded, 1);
        assert!(!outcome.needs_persistence);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "cached");
    }

    #[test]
    fn symbol_cache_load_from_disk_marks_mtime_refresh_for_persistence() {
        let project = tempfile::tempdir().expect("create project dir");
        let storage = tempfile::tempdir().expect("create storage dir");
        let source = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source, "pub fn cached() {}\n").expect("write source");
        let metadata = std::fs::metadata(&source).expect("stat source");
        let mtime = metadata.modified().expect("source mtime");
        let content = std::fs::read(&source).expect("read source");

        let mut cache = SymbolCache::new();
        cache.set_project_root(project.path().to_path_buf());
        cache.insert(
            source.clone(),
            mtime,
            content.len() as u64,
            crate::cache_freshness::hash_bytes(&content),
            vec![test_symbol("cached")],
        );
        symbol_cache_disk::write_to_disk(&cache, storage.path(), "mtime-unit-project")
            .expect("write symbol cache");

        let advanced_mtime = mtime
            .checked_add(std::time::Duration::from_secs(2))
            .expect("advance source mtime");
        filetime::set_file_mtime(
            &source,
            filetime::FileTime::from_system_time(advanced_mtime),
        )
        .expect("touch source");

        let mut restored = SymbolCache::new();
        let outcome = restored.load_from_disk_with_outcome(
            storage.path(),
            "mtime-unit-project",
            project.path(),
        );

        assert_eq!(outcome.loaded, 1);
        assert!(outcome.needs_persistence);
        assert!(restored.get(&source, advanced_mtime).is_some());
    }

    #[test]
    fn symbol_cache_load_from_disk_drops_stale_synthetic_entry() {
        let project = tempfile::tempdir().expect("create project dir");
        let storage = tempfile::tempdir().expect("create storage dir");
        let source = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source, "pub fn cached() {}\n").expect("write source");
        let mtime = std::fs::metadata(&source)
            .expect("stat source")
            .modified()
            .expect("source mtime");
        let content = std::fs::read(&source).expect("read source");
        let size = content.len() as u64;
        let hash = crate::cache_freshness::hash_bytes(&content);

        let mut cache = SymbolCache::new();
        cache.set_project_root(project.path().to_path_buf());
        cache.insert(
            source.clone(),
            mtime,
            size,
            hash,
            vec![test_symbol("cached")],
        );
        symbol_cache_disk::write_to_disk(&cache, storage.path(), "stale-unit-project")
            .expect("write symbol cache");

        std::fs::write(&source, "pub fn cached() {}\npub fn fresh() {}\n").expect("change source");

        let mut restored = SymbolCache::new();
        let outcome = restored.load_from_disk_with_outcome(
            storage.path(),
            "stale-unit-project",
            project.path(),
        );

        assert_eq!(outcome.loaded, 0);
        assert!(outcome.needs_persistence);
        assert_eq!(restored.len(), 0);
    }

    #[test]
    fn stale_prewarm_generation_cannot_repopulate_symbol_cache_after_reset() {
        let project = tempfile::tempdir().expect("create project dir");
        let storage = tempfile::tempdir().expect("create storage dir");
        let source = project.path().join("src/lib.rs");
        std::fs::create_dir_all(source.parent().expect("source parent"))
            .expect("create source dir");
        std::fs::write(&source, "pub fn cached() {}\n").expect("write source");
        let mtime = std::fs::metadata(&source)
            .expect("stat source")
            .modified()
            .expect("source mtime");
        let content = std::fs::read(&source).expect("read source");
        let size = content.len() as u64;
        let hash = crate::cache_freshness::hash_bytes(&content);

        let mut disk_cache = SymbolCache::new();
        disk_cache.set_project_root(project.path().to_path_buf());
        disk_cache.insert(
            source.clone(),
            mtime,
            size,
            hash,
            vec![test_symbol("cached")],
        );
        symbol_cache_disk::write_to_disk(&disk_cache, storage.path(), "prewarm-reset")
            .expect("write symbol cache");

        let shared = Arc::new(RwLock::new(SymbolCache::new()));
        let stale_generation = shared.write().unwrap().reset();
        let active_generation = shared.write().unwrap().reset();
        assert_ne!(stale_generation, active_generation);

        {
            let mut cache = shared.write().unwrap();
            assert!(!cache
                .set_project_root_for_generation(stale_generation, project.path().to_path_buf()));
            assert_eq!(
                cache.load_from_disk_for_generation(
                    stale_generation,
                    storage.path(),
                    "prewarm-reset",
                    project.path()
                ),
                0
            );
        }

        let mut stale_parser =
            FileParser::with_symbol_cache_generation(Arc::clone(&shared), Some(stale_generation));
        stale_parser
            .extract_symbols(&source)
            .expect("stale prewarm parses source but must not write cache");

        let cache = shared.read().unwrap();
        assert_eq!(cache.generation(), active_generation);
        assert_eq!(cache.len(), 0);
        assert!(cache.project_root().is_none());
        assert!(!cache.contains_key(&source));
    }

    // --- Language detection ---

    #[test]
    fn detect_ts() {
        assert_eq!(
            detect_language(Path::new("foo.ts")),
            Some(LangId::TypeScript)
        );
    }

    #[test]
    fn detect_tsx() {
        assert_eq!(detect_language(Path::new("foo.tsx")), Some(LangId::Tsx));
    }

    #[test]
    fn detect_js() {
        assert_eq!(
            detect_language(Path::new("foo.js")),
            Some(LangId::JavaScript)
        );
    }

    #[test]
    fn detect_jsx() {
        assert_eq!(
            detect_language(Path::new("foo.jsx")),
            Some(LangId::JavaScript)
        );
    }

    #[test]
    fn detect_py() {
        assert_eq!(detect_language(Path::new("foo.py")), Some(LangId::Python));
    }

    #[test]
    fn detect_rs() {
        assert_eq!(detect_language(Path::new("foo.rs")), Some(LangId::Rust));
    }

    #[test]
    fn detect_go() {
        assert_eq!(detect_language(Path::new("foo.go")), Some(LangId::Go));
    }

    #[test]
    fn detect_c() {
        assert_eq!(detect_language(Path::new("foo.c")), Some(LangId::C));
    }

    #[test]
    fn detect_h() {
        assert_eq!(detect_language(Path::new("foo.h")), Some(LangId::C));
    }

    #[test]
    fn detect_cc() {
        assert_eq!(detect_language(Path::new("foo.cc")), Some(LangId::Cpp));
    }

    #[test]
    fn detect_cpp() {
        assert_eq!(detect_language(Path::new("foo.cpp")), Some(LangId::Cpp));
    }

    #[test]
    fn detect_cxx() {
        assert_eq!(detect_language(Path::new("foo.cxx")), Some(LangId::Cpp));
    }

    #[test]
    fn detect_hpp() {
        assert_eq!(detect_language(Path::new("foo.hpp")), Some(LangId::Cpp));
    }

    #[test]
    fn detect_hh() {
        assert_eq!(detect_language(Path::new("foo.hh")), Some(LangId::Cpp));
    }

    #[test]
    fn detect_zig() {
        assert_eq!(detect_language(Path::new("foo.zig")), Some(LangId::Zig));
    }

    #[test]
    fn detect_cs() {
        assert_eq!(detect_language(Path::new("foo.cs")), Some(LangId::CSharp));
    }

    #[test]
    fn detect_unknown_returns_none() {
        assert_eq!(detect_language(Path::new("foo.txt")), None);
    }

    #[test]
    fn groovy_language_constant_loads_under_workspace_tree_sitter() {
        let mut parser = Parser::new();
        let language: Language = dekobon_tree_sitter_groovy::LANGUAGE.into();
        parser
            .set_language(&language)
            .expect("load Groovy grammar under workspace tree-sitter");
        let tree = parser
            .parse("class Greeter { def greet() { 'hi' } }", None)
            .expect("parse Groovy sample");
        assert!(
            !tree.root_node().has_error(),
            "Groovy sample should parse cleanly"
        );
    }

    #[test]
    fn detect_groovy_extensions_and_jenkinsfile() {
        assert_eq!(
            detect_language(Path::new("script.groovy")),
            Some(LangId::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("script.gvy")),
            Some(LangId::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("script.gy")),
            Some(LangId::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("shell.gsh")),
            Some(LangId::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("build.gradle")),
            Some(LangId::Groovy)
        );
        assert_eq!(
            detect_language(Path::new("build.gradle.kts")),
            Some(LangId::Kotlin)
        );
        assert_eq!(
            detect_language(Path::new("Jenkinsfile")),
            Some(LangId::Groovy)
        );
    }

    // --- Unsupported extension error ---

    #[test]
    fn unsupported_extension_returns_invalid_request() {
        // Use a file that exists but has an unsupported extension
        let path = fixture_path("sample.ts");
        let bad_path = path.with_extension("txt");
        // Create a dummy file so the error comes from language detection, not I/O
        std::fs::write(&bad_path, "hello").unwrap();
        let provider = TreeSitterProvider::new();
        let result = provider.list_symbols(&bad_path);
        std::fs::remove_file(&bad_path).ok();
        match result {
            Err(AftError::InvalidRequest { message }) => {
                assert!(
                    message.contains("unsupported file extension"),
                    "msg: {}",
                    message
                );
                assert!(message.contains("txt"), "msg: {}", message);
            }
            other => panic!("expected InvalidRequest, got {:?}", other),
        }
    }

    // --- TypeScript extraction ---

    #[test]
    fn ts_extracts_all_symbol_kinds() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"greet"),
            "missing function greet: {:?}",
            names
        );
        assert!(names.contains(&"add"), "missing arrow fn add: {:?}", names);
        assert!(
            names.contains(&"UserService"),
            "missing class UserService: {:?}",
            names
        );
        assert!(
            names.contains(&"Config"),
            "missing interface Config: {:?}",
            names
        );
        assert!(
            names.contains(&"Status"),
            "missing enum Status: {:?}",
            names
        );
        assert!(
            names.contains(&"UserId"),
            "missing type alias UserId: {:?}",
            names
        );
        assert!(
            names.contains(&"internalHelper"),
            "missing non-exported fn: {:?}",
            names
        );

        // At least 6 unique symbols as required
        assert!(
            symbols.len() >= 6,
            "expected ≥6 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn ts_symbol_kinds_correct() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert_eq!(find("greet").kind, SymbolKind::Function);
        assert_eq!(find("add").kind, SymbolKind::Function); // arrow fn → Function
        assert_eq!(find("UserService").kind, SymbolKind::Class);
        assert_eq!(find("Config").kind, SymbolKind::Interface);
        assert_eq!(find("Status").kind, SymbolKind::Enum);
        assert_eq!(find("UserId").kind, SymbolKind::TypeAlias);
    }

    #[test]
    fn ts_export_detection() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert!(find("greet").exported, "greet should be exported");
        assert!(find("add").exported, "add should be exported");
        assert!(
            find("UserService").exported,
            "UserService should be exported"
        );
        assert!(find("Config").exported, "Config should be exported");
        assert!(find("Status").exported, "Status should be exported");
        assert!(
            !find("internalHelper").exported,
            "internalHelper should not be exported"
        );
    }

    #[test]
    fn ts_method_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        let methods: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();
        assert!(!methods.is_empty(), "should have at least one method");

        for method in &methods {
            assert_eq!(
                method.scope_chain,
                vec!["UserService"],
                "method {} should have UserService in scope chain",
                method.name
            );
            assert_eq!(method.parent.as_deref(), Some("UserService"));
        }
    }

    #[test]
    fn ts_signatures_present() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        let greet_sig = find("greet").signature.as_ref().unwrap();
        assert!(
            greet_sig.contains("greet"),
            "signature should contain function name: {}",
            greet_sig
        );
    }

    #[test]
    fn ts_ranges_valid() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.ts")).unwrap();

        for s in &symbols {
            assert!(
                s.range.end_line >= s.range.start_line,
                "symbol {} has invalid range: {:?}",
                s.name,
                s.range
            );
        }
    }

    // --- JavaScript extraction ---

    #[test]
    fn js_extracts_core_symbols() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.js")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"multiply"),
            "missing function multiply: {:?}",
            names
        );
        assert!(
            names.contains(&"divide"),
            "missing arrow fn divide: {:?}",
            names
        );
        assert!(
            names.contains(&"EventEmitter"),
            "missing class EventEmitter: {:?}",
            names
        );
        assert!(
            names.contains(&"main"),
            "missing default export fn main: {:?}",
            names
        );

        assert!(
            symbols.len() >= 4,
            "expected ≥4 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn js_arrow_fn_correctly_named() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.js")).unwrap();

        let divide = symbols.iter().find(|s| s.name == "divide").unwrap();
        assert_eq!(divide.kind, SymbolKind::Function);
        assert!(divide.exported, "divide should be exported");

        let internal = symbols.iter().find(|s| s.name == "internalUtil").unwrap();
        assert_eq!(internal.kind, SymbolKind::Function);
        assert!(!internal.exported, "internalUtil should not be exported");
    }

    #[test]
    fn js_method_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.js")).unwrap();

        let methods: Vec<&Symbol> = symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Method)
            .collect();

        for method in &methods {
            assert_eq!(
                method.scope_chain,
                vec!["EventEmitter"],
                "method {} should have EventEmitter in scope chain",
                method.name
            );
        }
    }

    // --- TSX extraction ---

    #[test]
    fn tsx_extracts_react_component() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.tsx")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"Button"),
            "missing React component Button: {:?}",
            names
        );
        assert!(
            names.contains(&"Counter"),
            "missing class Counter: {:?}",
            names
        );
        assert!(
            names.contains(&"formatLabel"),
            "missing function formatLabel: {:?}",
            names
        );

        assert!(
            symbols.len() >= 2,
            "expected ≥2 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn tsx_jsx_doesnt_break_parser() {
        // Main assertion: TSX grammar handles JSX without errors
        let provider = TreeSitterProvider::new();
        let result = provider.list_symbols(&fixture_path("sample.tsx"));
        assert!(
            result.is_ok(),
            "TSX parsing should succeed: {:?}",
            result.err()
        );
    }

    // --- resolve_symbol ---

    #[test]
    fn resolve_symbol_finds_match() {
        let provider = TreeSitterProvider::new();
        let matches = provider
            .resolve_symbol(&fixture_path("sample.ts"), "greet")
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].symbol.name, "greet");
        assert_eq!(matches[0].symbol.kind, SymbolKind::Function);
    }

    #[test]
    fn resolve_symbol_not_found() {
        let provider = TreeSitterProvider::new();
        let result = provider.resolve_symbol(&fixture_path("sample.ts"), "nonexistent");
        assert!(matches!(result, Err(AftError::SymbolNotFound { .. })));
    }

    #[test]
    fn resolve_symbol_follows_reexport_chains() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.ts");
        let barrel1 = dir.path().join("barrel1.ts");
        let barrel2 = dir.path().join("barrel2.ts");
        let barrel3 = dir.path().join("barrel3.ts");
        let index = dir.path().join("index.ts");

        std::fs::write(
            &config,
            "export class Config {}\nexport default class DefaultConfig {}\n",
        )
        .unwrap();
        std::fs::write(
            &barrel1,
            "export { Config } from './config';\nexport { default as NamedDefault } from './config';\n",
        )
        .unwrap();
        std::fs::write(
            &barrel2,
            "export { Config as RenamedConfig } from './barrel1';\n",
        )
        .unwrap();
        std::fs::write(
            &barrel3,
            "export * from './barrel2';\nexport * from './barrel1';\n",
        )
        .unwrap();
        std::fs::write(
            &index,
            "export class Config {}\nexport { RenamedConfig as FinalConfig } from './barrel3';\nexport * from './barrel3';\n",
        )
        .unwrap();

        let provider = TreeSitterProvider::new();
        let config_canon = std::fs::canonicalize(&config).unwrap();

        let direct = provider.resolve_symbol(&barrel1, "Config").unwrap();
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].symbol.name, "Config");
        assert_eq!(direct[0].file, config_canon.display().to_string());

        let renamed = provider.resolve_symbol(&barrel2, "RenamedConfig").unwrap();
        assert_eq!(renamed.len(), 1);
        assert_eq!(renamed[0].symbol.name, "Config");
        assert_eq!(renamed[0].file, config_canon.display().to_string());

        let wildcard_chain = provider.resolve_symbol(&index, "FinalConfig").unwrap();
        assert_eq!(wildcard_chain.len(), 1);
        assert_eq!(wildcard_chain[0].symbol.name, "Config");
        assert_eq!(wildcard_chain[0].file, config_canon.display().to_string());

        let wildcard_default = provider.resolve_symbol(&index, "NamedDefault").unwrap();
        assert_eq!(wildcard_default.len(), 1);
        assert_eq!(wildcard_default[0].symbol.name, "DefaultConfig");
        assert_eq!(wildcard_default[0].file, config_canon.display().to_string());

        let local = provider.resolve_symbol(&index, "Config").unwrap();
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].symbol.name, "Config");
        assert_eq!(local[0].file, index.display().to_string());
    }

    // --- Parse tree caching ---

    #[test]
    fn symbol_range_includes_rust_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_attrs.rs");
        std::fs::write(
            &path,
            "/// This is a doc comment\n#[test]\n#[cfg(test)]\nfn my_test_fn() {\n    assert!(true);\n}\n",
        )
        .unwrap();

        let provider = TreeSitterProvider::new();
        let matches = provider.resolve_symbol(&path, "my_test_fn").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].symbol.range.start_line, 0,
            "symbol range should include preceding /// doc comment, got start_line={}",
            matches[0].symbol.range.start_line
        );
    }

    #[test]
    fn symbol_range_includes_go_doc_comment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_doc.go");
        std::fs::write(
            &path,
            "package main\n\n// MyFunc does something useful.\n// It has a multi-line doc.\nfunc MyFunc() {\n}\n",
        )
        .unwrap();

        let provider = TreeSitterProvider::new();
        let matches = provider.resolve_symbol(&path, "MyFunc").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].symbol.range.start_line, 2,
            "symbol range should include preceding doc comments, got start_line={}",
            matches[0].symbol.range.start_line
        );
    }

    #[test]
    fn symbol_range_skips_unrelated_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_gap.go");
        std::fs::write(
            &path,
            "package main\n\n// This is a standalone comment\n\nfunc Standalone() {\n}\n",
        )
        .unwrap();

        let provider = TreeSitterProvider::new();
        let matches = provider.resolve_symbol(&path, "Standalone").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(
            matches[0].symbol.range.start_line, 4,
            "symbol range should NOT include comment separated by blank line, got start_line={}",
            matches[0].symbol.range.start_line
        );
    }

    #[test]
    fn parse_cache_returns_same_tree() {
        let mut parser = FileParser::new();
        let path = fixture_path("sample.ts");

        let (tree1, _) = parser.parse(&path).unwrap();
        let tree1_root = tree1.root_node().byte_range();

        let (tree2, _) = parser.parse(&path).unwrap();
        let tree2_root = tree2.root_node().byte_range();

        // Same tree (cache hit) should return identical root node range
        assert_eq!(tree1_root, tree2_root);
    }

    #[test]
    fn extract_symbols_from_tree_matches_list_symbols() {
        let path = fixture_path("sample.rs");
        let source = std::fs::read_to_string(&path).unwrap();

        let provider = TreeSitterProvider::new();
        let listed = provider.list_symbols(&path).unwrap();

        let mut parser = FileParser::new();
        let (tree, lang) = parser.parse(&path).unwrap();
        let extracted = extract_symbols_from_tree(&source, tree, lang).unwrap();

        assert_eq!(symbols_as_debug(&extracted), symbols_as_debug(&listed));
    }

    fn symbols_as_debug(symbols: &[Symbol]) -> Vec<String> {
        symbols
            .iter()
            .map(|symbol| {
                format!(
                    "{}|{:?}|{}:{}-{}:{}|{:?}|{:?}|{}|{:?}",
                    symbol.name,
                    symbol.kind,
                    symbol.range.start_line,
                    symbol.range.start_col,
                    symbol.range.end_line,
                    symbol.range.end_col,
                    symbol.signature,
                    symbol.scope_chain,
                    symbol.exported,
                    symbol.parent,
                )
            })
            .collect()
    }

    // --- Python extraction ---

    #[test]
    fn py_extracts_all_symbols() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.py")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"top_level_function"),
            "missing top_level_function: {:?}",
            names
        );
        assert!(names.contains(&"MyClass"), "missing MyClass: {:?}", names);
        assert!(
            names.contains(&"instance_method"),
            "missing method instance_method: {:?}",
            names
        );
        assert!(
            names.contains(&"decorated_function"),
            "missing decorated_function: {:?}",
            names
        );

        // Plan requires ≥4 symbols
        assert!(
            symbols.len() >= 4,
            "expected ≥4 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn py_symbol_kinds_correct() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.py")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert_eq!(find("top_level_function").kind, SymbolKind::Function);
        assert_eq!(find("MyClass").kind, SymbolKind::Class);
        assert_eq!(find("instance_method").kind, SymbolKind::Method);
        assert_eq!(find("decorated_function").kind, SymbolKind::Function);
        assert_eq!(find("OuterClass").kind, SymbolKind::Class);
        assert_eq!(find("InnerClass").kind, SymbolKind::Class);
        assert_eq!(find("inner_method").kind, SymbolKind::Method);
        assert_eq!(find("outer_method").kind, SymbolKind::Method);
    }

    #[test]
    fn py_method_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.py")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        // Method inside MyClass
        assert_eq!(
            find("instance_method").scope_chain,
            vec!["MyClass"],
            "instance_method should have MyClass in scope chain"
        );
        assert_eq!(find("instance_method").parent.as_deref(), Some("MyClass"));

        // Method inside OuterClass > InnerClass
        assert_eq!(
            find("inner_method").scope_chain,
            vec!["OuterClass", "InnerClass"],
            "inner_method should have nested scope chain"
        );

        // InnerClass itself should have OuterClass in scope
        assert_eq!(
            find("InnerClass").scope_chain,
            vec!["OuterClass"],
            "InnerClass should have OuterClass in scope"
        );

        // Top-level function has empty scope
        assert!(
            find("top_level_function").scope_chain.is_empty(),
            "top-level function should have empty scope chain"
        );
    }

    #[test]
    fn py_decorated_function_signature() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.py")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        let sig = find("decorated_function").signature.as_ref().unwrap();
        assert!(
            sig.contains("@staticmethod"),
            "decorated function signature should include decorator: {}",
            sig
        );
        assert!(
            sig.contains("def decorated_function"),
            "signature should include function def: {}",
            sig
        );
    }

    #[test]
    fn py_ranges_valid() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.py")).unwrap();

        for s in &symbols {
            assert!(
                s.range.end_line >= s.range.start_line,
                "symbol {} has invalid range: {:?}",
                s.name,
                s.range
            );
        }
    }

    // --- Rust extraction ---

    #[test]
    fn rs_extracts_all_symbols() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"public_function"),
            "missing public_function: {:?}",
            names
        );
        assert!(
            names.contains(&"private_function"),
            "missing private_function: {:?}",
            names
        );
        assert!(names.contains(&"MyStruct"), "missing MyStruct: {:?}", names);
        assert!(names.contains(&"Color"), "missing enum Color: {:?}", names);
        assert!(
            names.contains(&"Drawable"),
            "missing trait Drawable: {:?}",
            names
        );
        // impl methods
        assert!(
            names.contains(&"new"),
            "missing impl method new: {:?}",
            names
        );

        // Plan requires ≥6 symbols
        assert!(
            symbols.len() >= 6,
            "expected ≥6 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn rs_symbol_kinds_correct() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert_eq!(find("public_function").kind, SymbolKind::Function);
        assert_eq!(find("private_function").kind, SymbolKind::Function);
        assert_eq!(find("MyStruct").kind, SymbolKind::Struct);
        assert_eq!(find("Color").kind, SymbolKind::Enum);
        assert_eq!(find("Drawable").kind, SymbolKind::Interface); // trait → Interface
        assert_eq!(find("new").kind, SymbolKind::Method);
    }

    #[test]
    fn rs_pub_export_detection() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert!(
            find("public_function").exported,
            "pub fn should be exported"
        );
        assert!(
            !find("private_function").exported,
            "non-pub fn should not be exported"
        );
        assert!(find("MyStruct").exported, "pub struct should be exported");
        assert!(find("Color").exported, "pub enum should be exported");
        assert!(find("Drawable").exported, "pub trait should be exported");
        assert!(
            find("new").exported,
            "pub fn inside impl should be exported"
        );
        assert!(
            !find("helper").exported,
            "non-pub fn inside impl should not be exported"
        );
    }

    #[test]
    fn rs_impl_method_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        // `impl MyStruct { fn new() }` → scope chain = ["MyStruct"]
        assert_eq!(
            find("new").scope_chain,
            vec!["MyStruct"],
            "impl method should have type in scope chain"
        );
        assert_eq!(find("new").parent.as_deref(), Some("MyStruct"));

        // Free function has empty scope chain
        assert!(
            find("public_function").scope_chain.is_empty(),
            "free function should have empty scope chain"
        );
    }

    #[test]
    fn rs_trait_impl_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        // `impl Drawable for MyStruct { fn draw() }` → scope = ["Drawable for MyStruct"]
        let draw = symbols.iter().find(|s| s.name == "draw").unwrap();
        assert_eq!(
            draw.scope_chain,
            vec!["Drawable for MyStruct"],
            "trait impl method should have 'Trait for Type' scope"
        );
        assert_eq!(draw.parent.as_deref(), Some("MyStruct"));
    }

    #[test]
    fn rs_ranges_valid() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.rs")).unwrap();

        for s in &symbols {
            assert!(
                s.range.end_line >= s.range.start_line,
                "symbol {} has invalid range: {:?}",
                s.name,
                s.range
            );
        }
    }

    // --- Go extraction ---

    #[test]
    fn go_extracts_all_symbols() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.go")).unwrap();

        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"ExportedFunction"),
            "missing ExportedFunction: {:?}",
            names
        );
        assert!(
            names.contains(&"unexportedFunction"),
            "missing unexportedFunction: {:?}",
            names
        );
        assert!(
            names.contains(&"MyStruct"),
            "missing struct MyStruct: {:?}",
            names
        );
        assert!(
            names.contains(&"Reader"),
            "missing interface Reader: {:?}",
            names
        );
        // receiver method
        assert!(
            names.contains(&"String"),
            "missing receiver method String: {:?}",
            names
        );

        // Plan requires ≥4 symbols
        assert!(
            symbols.len() >= 4,
            "expected ≥4 symbols, got {}: {:?}",
            symbols.len(),
            names
        );
    }

    #[test]
    fn go_symbol_kinds_correct() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.go")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert_eq!(find("ExportedFunction").kind, SymbolKind::Function);
        assert_eq!(find("unexportedFunction").kind, SymbolKind::Function);
        assert_eq!(find("MyStruct").kind, SymbolKind::Struct);
        assert_eq!(find("Reader").kind, SymbolKind::Interface);
        assert_eq!(find("String").kind, SymbolKind::Method);
        assert_eq!(find("helper").kind, SymbolKind::Method);
    }

    #[test]
    fn go_uppercase_export_detection() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.go")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        assert!(
            find("ExportedFunction").exported,
            "ExportedFunction (uppercase) should be exported"
        );
        assert!(
            !find("unexportedFunction").exported,
            "unexportedFunction (lowercase) should not be exported"
        );
        assert!(
            find("MyStruct").exported,
            "MyStruct (uppercase) should be exported"
        );
        assert!(
            find("Reader").exported,
            "Reader (uppercase) should be exported"
        );
        assert!(
            find("String").exported,
            "String method (uppercase) should be exported"
        );
        assert!(
            !find("helper").exported,
            "helper method (lowercase) should not be exported"
        );
    }

    #[test]
    fn go_receiver_method_scope_chain() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.go")).unwrap();

        let find = |name: &str| symbols.iter().find(|s| s.name == name).unwrap();

        // `func (m *MyStruct) String()` → scope chain = ["MyStruct"]
        assert_eq!(
            find("String").scope_chain,
            vec!["MyStruct"],
            "receiver method should have type in scope chain"
        );
        assert_eq!(find("String").parent.as_deref(), Some("MyStruct"));

        // Regular function has empty scope chain
        assert!(
            find("ExportedFunction").scope_chain.is_empty(),
            "regular function should have empty scope chain"
        );
    }

    #[test]
    fn go_ranges_valid() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("sample.go")).unwrap();

        for s in &symbols {
            assert!(
                s.range.end_line >= s.range.start_line,
                "symbol {} has invalid range: {:?}",
                s.name,
                s.range
            );
        }
    }

    // --- Cross-language ---

    #[test]
    fn cross_language_all_six_produce_symbols() {
        let provider = TreeSitterProvider::new();

        let fixtures = [
            ("sample.ts", "TypeScript"),
            ("sample.tsx", "TSX"),
            ("sample.js", "JavaScript"),
            ("sample.py", "Python"),
            ("sample.rs", "Rust"),
            ("sample.go", "Go"),
        ];

        for (fixture, lang) in &fixtures {
            let symbols = provider
                .list_symbols(&fixture_path(fixture))
                .unwrap_or_else(|e| panic!("{} ({}) failed: {:?}", lang, fixture, e));
            assert!(
                symbols.len() >= 2,
                "{} should produce ≥2 symbols, got {}: {:?}",
                lang,
                symbols.len(),
                symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
            );
        }
    }

    // --- Symbol cache tests ---

    fn cached_freshness_fixture(
        content: &str,
    ) -> (tempfile::TempDir, PathBuf, SystemTime, u64, blake3::Hash) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, content).unwrap();
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(1, 0)).unwrap();
        let metadata = std::fs::metadata(&file).unwrap();
        let mtime = metadata.modified().unwrap();
        let size = metadata.len();
        let hash = cache_freshness::hash_bytes(content.as_bytes());
        (dir, file, mtime, size, hash)
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cached_freshness_unchanged_metadata_skips_hashing() {
        let (_dir, file, mtime, size, hash) = cached_freshness_fixture("pub fn hello() {}\n");

        cache_freshness::reset_hash_file_if_small_count_for_debug();
        assert!(cached_file_is_fresh(&file, mtime, size, hash, mtime));
        assert_eq!(cache_freshness::hash_file_if_small_count_for_debug(), 0);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cached_freshness_changed_mtime_with_identical_content_hashes_fresh() {
        let (_dir, file, mtime, size, hash) = cached_freshness_fixture("pub fn hello() {}\n");
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(2, 0)).unwrap();

        cache_freshness::reset_hash_file_if_small_count_for_debug();
        assert!(cached_file_is_fresh(&file, mtime, size, hash, mtime));
        assert_eq!(cache_freshness::hash_file_if_small_count_for_debug(), 1);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cached_freshness_changed_mtime_and_same_size_content_hashes_stale() {
        let (_dir, file, mtime, size, hash) = cached_freshness_fixture("pub fn one() {}\n");
        std::fs::write(&file, "pub fn two() {}\n").unwrap();
        filetime::set_file_mtime(&file, filetime::FileTime::from_unix_time(2, 0)).unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().len(), size);

        cache_freshness::reset_hash_file_if_small_count_for_debug();
        assert!(!cached_file_is_fresh(&file, mtime, size, hash, mtime));
        assert_eq!(cache_freshness::hash_file_if_small_count_for_debug(), 1);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cached_freshness_changed_size_skips_hashing() {
        let (_dir, file, mtime, size, hash) = cached_freshness_fixture("pub fn hello() {}\n");
        std::fs::write(&file, "pub fn hello() {}\n// longer\n").unwrap();

        cache_freshness::reset_hash_file_if_small_count_for_debug();
        assert!(!cached_file_is_fresh(&file, mtime, size, hash, mtime));
        assert_eq!(cache_freshness::hash_file_if_small_count_for_debug(), 0);
    }

    #[test]
    fn symbol_cache_returns_cached_results_on_second_call() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "pub fn hello() {}\npub fn world() {}").unwrap();

        let mut parser = FileParser::new();

        let symbols1 = parser.extract_symbols(&file).unwrap();
        assert_eq!(symbols1.len(), 2);

        // Second call should return cached result
        let symbols2 = parser.extract_symbols(&file).unwrap();
        assert_eq!(symbols2.len(), 2);
        assert_eq!(symbols1[0].name, symbols2[0].name);

        // Verify cache is populated
        assert!(parser.symbol_cache.read().unwrap().contains_key(&file));
    }

    #[test]
    fn symbol_cache_invalidates_on_file_change() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "pub fn hello() {}").unwrap();

        let mut parser = FileParser::new();

        let symbols1 = parser.extract_symbols(&file).unwrap();
        assert_eq!(symbols1.len(), 1);
        assert_eq!(symbols1[0].name, "hello");

        // Wait to ensure mtime changes (filesystem resolution can be 1s on some OS)
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Modify file — add a second function
        std::fs::write(&file, "pub fn hello() {}\npub fn goodbye() {}").unwrap();

        // Should detect mtime change and re-extract
        let symbols2 = parser.extract_symbols(&file).unwrap();
        assert_eq!(symbols2.len(), 2);
        assert!(symbols2.iter().any(|s| s.name == "goodbye"));
    }

    #[test]
    fn symbol_cache_invalidate_method_clears_entry() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.rs");
        std::fs::write(&file, "pub fn hello() {}").unwrap();

        let mut parser = FileParser::new();
        parser.extract_symbols(&file).unwrap();
        assert!(parser.symbol_cache.read().unwrap().contains_key(&file));

        parser.invalidate_symbols(&file);
        assert!(!parser.symbol_cache.read().unwrap().contains_key(&file));
        // Parse tree cache should also be cleared
        assert!(!parser.cache.contains_key(&file));
    }

    #[test]
    fn symbol_cache_works_for_multiple_languages() {
        let dir = tempfile::tempdir().unwrap();
        let rs_file = dir.path().join("lib.rs");
        let ts_file = dir.path().join("app.ts");
        let py_file = dir.path().join("main.py");

        std::fs::write(&rs_file, "pub fn rust_fn() {}").unwrap();
        std::fs::write(&ts_file, "export function tsFn() {}").unwrap();
        std::fs::write(&py_file, "def py_fn():\n    pass").unwrap();

        let mut parser = FileParser::new();

        let rs_syms = parser.extract_symbols(&rs_file).unwrap();
        let ts_syms = parser.extract_symbols(&ts_file).unwrap();
        let py_syms = parser.extract_symbols(&py_file).unwrap();

        assert!(rs_syms.iter().any(|s| s.name == "rust_fn"));
        assert!(ts_syms.iter().any(|s| s.name == "tsFn"));
        assert!(py_syms.iter().any(|s| s.name == "py_fn"));

        // All should be cached now
        assert_eq!(parser.symbol_cache.read().unwrap().len(), 3);

        // Re-extract should return same results from cache
        let rs_syms2 = parser.extract_symbols(&rs_file).unwrap();
        assert_eq!(rs_syms.len(), rs_syms2.len());
    }

    #[test]
    fn extract_json_symbols_top_level_keys() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("package.json");
        std::fs::write(&file, r#"{"name": "x", "version": "1"}"#).unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 2);
        assert!(symbols
            .iter()
            .any(|s| s.name == "name" && s.kind == SymbolKind::Variable));
        assert!(symbols
            .iter()
            .any(|s| s.name == "version" && s.kind == SymbolKind::Variable));
    }

    #[test]
    fn extract_json_symbols_root_array() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("array.json");
        std::fs::write(&file, "[1,2,3]").unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 0);
    }

    #[test]
    fn extract_json_symbols_no_recursion_into_nested() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested.json");
        std::fs::write(&file, r#"{"scripts": {"build": "tsc"}}"#).unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "scripts");
        assert!(!symbols.iter().any(|s| s.name == "build"));
    }

    #[test]
    fn extract_scala_symbols_object_and_method() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Greeter.scala");
        std::fs::write(
            &file,
            "object Greeter {\n  def hello(name: String): String = s\"hi $name\"\n}",
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert!(symbols
            .iter()
            .any(|s| s.name == "Greeter" && s.kind == SymbolKind::Class));
        assert!(symbols.iter().any(|s| s.name == "hello"
            && s.kind == SymbolKind::Method
            && s.scope_chain == vec!["Greeter".to_string()]));
    }

    #[test]
    fn extract_scala_symbols_class_and_trait() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("Types.scala");
        std::fs::write(&file, "class Foo\ntrait Bar").unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert!(symbols
            .iter()
            .any(|s| s.name == "Foo" && s.kind == SymbolKind::Class));
        assert!(symbols
            .iter()
            .any(|s| s.name == "Bar" && s.kind == SymbolKind::Interface));
    }

    #[test]
    fn extract_yaml_symbols_k8s_resource() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deployment.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: nginx
  namespace: web
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for K8s Deployment");
        let sym = &symbols[0];
        assert_eq!(sym.name, "web/Deployment/nginx");
        assert_eq!(sym.kind, SymbolKind::Class);
        assert!(sym.exported);
    }

    #[test]
    fn extract_yaml_symbols_k8s_no_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("service.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: v1
kind: Service
metadata:
  name: nginx-svc
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for K8s Service");
        let sym = &symbols[0];
        assert_eq!(sym.name, "Service/nginx-svc");
        assert_eq!(sym.kind, SymbolKind::Class);
    }

    #[test]
    fn extract_yaml_symbols_multidoc() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("multidoc.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: a
---
apiVersion: v1
kind: Service
metadata:
  name: b
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 2, "Expected 2 symbols for multi-doc YAML");
        assert!(symbols.iter().any(|s| s.name == "Deployment/a"));
        assert!(symbols.iter().any(|s| s.name == "Service/b"));
    }

    #[test]
    fn extract_yaml_symbols_generic_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("compose.yaml");
        std::fs::write(
            &file,
            r#"version: "3"
services:
  web: {}
volumes:
  data: {}
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        // Should have top-level keys: version, services, volumes
        assert_eq!(symbols.len(), 3, "Expected 3 symbols for generic YAML");
        assert!(symbols
            .iter()
            .any(|s| s.name == "version" && s.kind == SymbolKind::Variable));
        assert!(symbols
            .iter()
            .any(|s| s.name == "services" && s.kind == SymbolKind::Variable));
        assert!(symbols
            .iter()
            .any(|s| s.name == "volumes" && s.kind == SymbolKind::Variable));
    }

    #[test]
    fn extract_yaml_symbols_empty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("empty.yaml");
        std::fs::write(&file, "").unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 0, "Expected 0 symbols for empty YAML");
    }

    #[test]
    fn extract_yaml_symbols_resource_limits() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deployment.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: app
spec:
  template:
    spec:
      containers:
      - name: main
        image: myapp:1.0
        resources:
          limits:
            cpu: "2"
            memory: 1Gi
          requests:
            cpu: "1"
            memory: 512Mi
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for Deployment");
        let sym = &symbols[0];
        assert_eq!(sym.name, "Deployment/app");
        let sig = sym.signature.as_ref().unwrap();
        assert!(sig.contains("cpu="), "Signature should contain cpu= field");
        assert!(
            sig.contains("memory="),
            "Signature should contain memory= field"
        );
    }

    #[test]
    fn extract_yaml_symbols_env_names() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deployment.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: app
spec:
  template:
    spec:
      containers:
      - name: main
        image: myapp:1.0
        env:
        - name: FOO
          value: "bar"
        - name: BAR
          value: "baz"
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for Deployment");
        let sym = &symbols[0];
        let sig = sym.signature.as_ref().unwrap();
        assert!(
            sig.contains("env=FOO,BAR"),
            "Signature should contain env=FOO,BAR"
        );
    }

    #[test]
    fn extract_yaml_symbols_rbac_rules() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("role.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata:
  name: reader
rules:
- apiGroups: [""]
  resources: [pods, services]
  verbs: [get, list, watch]
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for Role");
        let sym = &symbols[0];
        let sig = sym.signature.as_ref().unwrap();
        assert!(
            sig.contains("verbs=get,list,watch"),
            "Signature should contain verbs=get,list,watch"
        );
        assert!(
            sig.contains("resources=pods,services"),
            "Signature should contain resources=pods,services"
        );
    }

    #[test]
    fn extract_yaml_symbols_argo_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("workflow.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: argoproj.io/v1alpha1
kind: Workflow
metadata:
  name: hello-world
spec:
  entrypoint: main
  templates:
  - name: main
    container:
      image: alpine:3.18
      command: [echo]
      args: ["hello"]
  - name: print
    container:
      image: alpine:3.18
      command: [echo]
      args: ["world"]
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for Workflow");
        let sym = &symbols[0];
        assert!(
            sym.name.contains("Workflow"),
            "Symbol name should contain Workflow"
        );
        let sig = sym.signature.as_ref().unwrap();
        assert!(
            sig.contains("entrypoint=main"),
            "Signature should contain entrypoint=main"
        );
        assert!(
            sig.contains("templates=main,print"),
            "Signature should contain templates=main,print"
        );
        assert!(
            sig.contains("image=alpine:3.18"),
            "Signature should contain image=alpine:3.18"
        );
        assert!(
            sig.contains("command=echo"),
            "Signature should contain command=echo"
        );
    }

    #[test]
    fn extract_yaml_symbols_generatename_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("workflow.yaml");
        std::fs::write(
            &file,
            r#"apiVersion: argoproj.io/v1alpha1
kind: Workflow
metadata:
  generateName: hello-
spec:
  entrypoint: main
  templates:
  - name: main
    container:
      image: alpine:3.18
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        assert_eq!(symbols.len(), 1, "Expected 1 symbol for Workflow");
        let sym = &symbols[0];
        assert!(
            sym.name.contains("hello-"),
            "Symbol name should contain generateName fallback 'hello-'"
        );
    }

    #[test]
    fn detect_r_extensions_are_case_sensitive() {
        assert_eq!(detect_language(Path::new("analysis.R")), Some(LangId::R));
        assert_eq!(detect_language(Path::new("script.r")), Some(LangId::R));
    }

    #[test]
    fn extract_r_symbols_test() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("analysis.R");
        std::fs::write(
            &file,
            r#"
# summary function
summarise <- function(data, column) {
  total <- sum(data[[column]])
  total
}

normalise = function(x) {
  x / max(x)
}

function(y) {
  y + 1
} -> transform_values

threshold <- 10
"done" -> status

outer <- function(values) {
  local_value <- 1
  local_value
}
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        let get = |name: &str| {
            symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing {name}; got {symbols:?}"))
        };

        assert_eq!(get("summarise").kind, SymbolKind::Function);
        assert_eq!(get("normalise").kind, SymbolKind::Function);
        assert_eq!(get("transform_values").kind, SymbolKind::Function);
        assert!(
            symbols
                .iter()
                .all(|symbol| !symbol.name.starts_with("function(")),
            "rightward function assignments should use the RHS identifier: {symbols:?}"
        );
        assert_eq!(get("outer").kind, SymbolKind::Function);
        assert_eq!(get("threshold").kind, SymbolKind::Variable);
        assert_eq!(get("status").kind, SymbolKind::Variable);
        assert!(
            symbols.iter().all(|symbol| symbol.name != "local_value"),
            "nested assignments should not surface as top-level variables: {symbols:?}"
        );
    }

    #[test]
    fn extract_pascal_symbols_test() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("MyUnit.pas");
        std::fs::write(
            &file,
            r#"
unit MyUnit;

interface

uses SysUtils;

type
  TMyClass = class
  private
    FValue: Integer;
  public
    constructor Create;
    procedure DoSomething; virtual;
  end;

  TMyRecord = record
    X, Y: Integer;
  end;

  IMyInterface = interface
    procedure DoSomethingElse;
  end;

  TMyEnum = (Red, Green, Blue);

const
  UNIT_CONST = 42;

var
  UnitVar: string;

implementation

constructor TMyClass.Create;
begin
  FValue := 0;
end;

procedure TMyClass.DoSomething;
begin
end;

procedure StandaloneProc;
begin
end;

end.
"#,
        )
        .unwrap();

        let mut parser = FileParser::new();
        let symbols = parser.extract_symbols(&file).unwrap();

        let get = |name: &str| {
            symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing {name}; got {symbols:?}"))
        };

        assert_eq!(get("MyUnit").kind, SymbolKind::Class);
        assert_eq!(get("TMyClass").kind, SymbolKind::Class);
        assert_eq!(get("TMyRecord").kind, SymbolKind::Struct);
        assert_eq!(get("IMyInterface").kind, SymbolKind::Interface);
        assert_eq!(get("TMyEnum").kind, SymbolKind::Enum);
        assert_eq!(get("UNIT_CONST").kind, SymbolKind::Variable);
        assert_eq!(get("UnitVar").kind, SymbolKind::Variable);
        assert_eq!(get("StandaloneProc").kind, SymbolKind::Function);

        // Methods inside TMyClass
        let create_methods: Vec<&Symbol> = symbols.iter().filter(|s| s.name == "Create").collect();
        assert_eq!(create_methods.len(), 2); // one in interface, one in implementation
        for m in create_methods {
            assert_eq!(m.kind, SymbolKind::Method);
            assert_eq!(m.scope_chain, vec!["TMyClass".to_string()]);
        }

        let do_something_methods: Vec<&Symbol> =
            symbols.iter().filter(|s| s.name == "DoSomething").collect();
        assert_eq!(do_something_methods.len(), 2); // one in interface, one in implementation
        for m in do_something_methods {
            assert_eq!(m.kind, SymbolKind::Method);
            assert_eq!(m.scope_chain, vec!["TMyClass".to_string()]);
        }
    }

    #[test]
    fn groovy_fixture_extracts_types_methods_and_properties() {
        let provider = TreeSitterProvider::new();
        let symbols = provider
            .list_symbols(&fixture_path("sample.groovy"))
            .unwrap();

        let get = |name: &str| {
            symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing {name}; got {symbols:?}"))
        };

        assert_eq!(get("GreeterSupport").kind, SymbolKind::Interface);
        assert_eq!(get("Named").kind, SymbolKind::Interface);
        assert_eq!(get("BuildStatus").kind, SymbolKind::Enum);
        assert_eq!(get("BuildLogic").kind, SymbolKind::Class);
        assert_eq!(get("action").kind, SymbolKind::Variable);
        assert_eq!(get("action").scope_chain, vec!["BuildLogic".to_string()]);
        assert_eq!(get("greet").kind, SymbolKind::Method);
        assert_eq!(get("greet").scope_chain, vec!["BuildLogic".to_string()]);
        assert_eq!(get("topLevelHelper").kind, SymbolKind::Function);
        assert!(get("topLevelHelper").scope_chain.is_empty());

        assert_eq!(get("BuildLogic").range.start_line, 12);
        assert_eq!(get("BuildLogic").range.end_line, 23);
        assert_eq!(get("action").range.start_line, 14);
        assert_eq!(get("action").range.end_line, 14);
        assert_eq!(get("greet").range.start_line, 16);
        assert_eq!(get("greet").range.end_line, 18);
        assert_eq!(get("topLevelHelper").range.start_line, 25);
        assert_eq!(get("topLevelHelper").range.end_line, 27);
    }

    #[test]
    fn groovy_gradle_fixture_extracts_task_declarations() {
        let provider = TreeSitterProvider::new();
        let symbols = provider
            .list_symbols(&fixture_path("build.gradle"))
            .unwrap();

        let get = |name: &str| {
            symbols
                .iter()
                .find(|symbol| symbol.name == name)
                .unwrap_or_else(|| panic!("missing {name}; got {symbols:?}"))
        };

        assert_eq!(get("smokeTest").kind, SymbolKind::Function);
        assert_eq!(get("smokeTest").range.start_line, 8);
        assert_eq!(get("smokeTest").range.end_line, 12);
        assert_eq!(get("ciCheck").kind, SymbolKind::Function);
        assert_eq!(get("ciCheck").range.start_line, 14);
        assert_eq!(get("ciCheck").range.end_line, 18);
    }

    #[test]
    fn groovy_jenkinsfile_fixture_extracts_pipeline_block() {
        let provider = TreeSitterProvider::new();
        let symbols = provider.list_symbols(&fixture_path("Jenkinsfile")).unwrap();

        let pipeline = symbols
            .iter()
            .find(|symbol| symbol.name == "pipeline")
            .unwrap_or_else(|| panic!("missing pipeline; got {symbols:?}"));
        assert_eq!(pipeline.kind, SymbolKind::Function);
        assert_eq!(pipeline.range.start_line, 0);
        assert_eq!(pipeline.range.end_line, 9);
    }
}
