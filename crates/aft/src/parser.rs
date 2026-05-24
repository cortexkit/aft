use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
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

;; arrow functions assigned to const/let/var
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow.name
    value: (arrow_function) @arrow.body)) @arrow.def

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
    name: (identifier) @var.name)) @var.def

;; export statement wrappers (top-level only)
(export_statement) @export.stmt
"#;

const JS_QUERY: &str = r#"
;; function declarations
(function_declaration
  name: (identifier) @fn.name) @fn.def

;; arrow functions assigned to const/let/var
(lexical_declaration
  (variable_declarator
    name: (identifier) @arrow.name
    value: (arrow_function) @arrow.body)) @arrow.def

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
    name: (identifier) @var.name)) @var.def

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
"#;

const PERL_QUERY: &str = r#"
;; packages and subroutines
(package_statement
  (package_name) @package.name) @package.def
(function_definition
  name: (identifier) @fn.name) @fn.def
(function_definition_without_sub
  name: (identifier) @fn.name) @fn.def

;; constants / lexical variables
(use_constant_statement
  constant: (identifier) @var.name) @var.def
(variable_declaration
  variable_name: (_) @var.name) @var.def
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
}

/// Maps file extension to language identifier.
pub fn detect_language(path: &Path) -> Option<LangId> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "ts" => Some(LangId::TypeScript),
        "tsx" => Some(LangId::Tsx),
        "js" | "jsx" => Some(LangId::JavaScript),
        "py" => Some(LangId::Python),
        "rs" => Some(LangId::Rust),
        "go" => Some(LangId::Go),
        "c" | "h" => Some(LangId::C),
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => Some(LangId::Cpp),
        "zig" => Some(LangId::Zig),
        "cs" => Some(LangId::CSharp),
        "sh" | "bash" | "zsh" => Some(LangId::Bash),
        "html" | "htm" => Some(LangId::Html),
        "md" | "markdown" | "mdx" => Some(LangId::Markdown),
        "sol" => Some(LangId::Solidity),
        "vue" => Some(LangId::Vue),
        "json" | "jsonc" => Some(LangId::Json),
        "scala" | "sc" => Some(LangId::Scala),
        "java" => Some(LangId::Java),
        "rb" => Some(LangId::Ruby),
        "kt" | "kts" => Some(LangId::Kotlin),
        "swift" => Some(LangId::Swift),
        "php" => Some(LangId::Php),
        "lua" => Some(LangId::Lua),
        "pl" | "pm" | "t" => Some(LangId::Perl),
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
    }
}

/// Returns the query pattern string for a given LangId, if implemented.
fn query_for(lang: LangId) -> Option<&'static str> {
    match lang {
        LangId::TypeScript | LangId::Tsx => Some(TS_QUERY),
        LangId::JavaScript => Some(JS_QUERY),
        LangId::Python => Some(PY_QUERY),
        LangId::Rust => Some(RS_QUERY),
        LangId::Go => Some(GO_QUERY),
        LangId::C => Some(C_QUERY),
        LangId::Cpp => Some(CPP_QUERY),
        LangId::Zig => Some(ZIG_QUERY),
        LangId::CSharp => Some(CSHARP_QUERY),
        LangId::Bash => Some(BASH_QUERY),
        LangId::Html => None, // HTML uses direct tree walking like Markdown
        LangId::Markdown => None,
        LangId::Solidity => Some(SOL_QUERY),
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
static RS_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Rust));
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
        LangId::Rust => Some(&*RS_QUERY_CACHE),
        LangId::Go => Some(&*GO_QUERY_CACHE),
        LangId::C => Some(&*C_QUERY_CACHE),
        LangId::Cpp => Some(&*CPP_QUERY_CACHE),
        LangId::Zig => Some(&*ZIG_QUERY_CACHE),
        LangId::CSharp => Some(&*CSHARP_QUERY_CACHE),
        LangId::Bash => Some(&*BASH_QUERY_CACHE),
        LangId::Solidity => Some(&*SOL_QUERY_CACHE),
        LangId::Scala => Some(&*SCALA_QUERY_CACHE),
        LangId::Java => Some(&*JAVA_QUERY_CACHE),
        LangId::Ruby => Some(&*RUBY_QUERY_CACHE),
        LangId::Kotlin => Some(&*KOTLIN_QUERY_CACHE),
        LangId::Swift => Some(&*SWIFT_QUERY_CACHE),
        LangId::Php => Some(&*PHP_QUERY_CACHE),
        LangId::Lua => Some(&*LUA_QUERY_CACHE),
        LangId::Perl => Some(&*PERL_QUERY_CACHE),
        LangId::Html | LangId::Markdown | LangId::Vue | LangId::Json => None,
    };

    query
        .map(|result| {
            result.as_ref().map_err(|message| AftError::ParseError {
                message: message.clone(),
            })
        })
        .transpose()
}

/// Cached parse result: mtime at parse time + the tree.
struct CachedTree {
    mtime: SystemTime,
    size: u64,
    content_hash: blake3::Hash,
    tree: Tree,
}

/// Cached symbol extraction result: mtime at extraction time + symbols.
#[derive(Clone)]
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
    if current_size > cache_freshness::CONTENT_HASH_SIZE_CAP {
        return current_mtime == cached_mtime;
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
    project_root: Option<PathBuf>,
}

pub type SharedSymbolCache = Arc<RwLock<SymbolCache>>;

impl SymbolCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            generation: 0,
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
    ) {
        self.entries.insert(
            path,
            CachedSymbols {
                mtime,
                size,
                content_hash,
                symbols,
            },
        );
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
        self.insert(path, mtime, size, content_hash, symbols);
        true
    }

    /// Return cached symbols when the source file is still fresh.
    pub fn get(&self, path: &Path, mtime: SystemTime) -> Option<Vec<Symbol>> {
        self.entries.get(path).and_then(|cached| {
            cached_file_is_fresh(path, cached.mtime, cached.size, cached.content_hash, mtime)
                .then(|| cached.symbols.clone())
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
        debug_assert!(current_root.is_absolute());
        let Some(cache) = symbol_cache_disk::read_from_disk(storage_dir, project_key) else {
            return 0;
        };

        self.project_root = Some(current_root.to_path_buf());
        self.entries.clear();
        let mut loaded = 0usize;

        for entry in cache.entries {
            let Some(path) =
                crate::search_index::cached_path_under_root(current_root, &entry.relative_path)
            else {
                continue;
            };
            let cached_freshness = FileFreshness {
                mtime: entry.mtime,
                size: entry.size,
                content_hash: entry.content_hash,
            };
            let mtime = match cache_freshness::verify_file(&path, &cached_freshness) {
                FreshnessVerdict::HotFresh => entry.mtime,
                FreshnessVerdict::ContentFresh { new_mtime, .. } => new_mtime,
                FreshnessVerdict::Stale | FreshnessVerdict::Deleted => continue,
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
            loaded += 1;
        }

        loaded
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
        if self.generation != generation {
            return 0;
        }
        self.load_from_disk(storage_dir, project_key, current_root)
    }

    /// Invalidate cached symbols for a specific file.
    pub fn invalidate(&mut self, path: &Path) {
        self.entries.remove(path);
    }

    /// Clear all entries and advance the generation to ignore stale background writers.
    pub fn reset(&mut self) -> u64 {
        self.entries.clear();
        self.project_root = None;
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }

    /// Current generation token.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the cache has an entry for a file.
    pub fn contains_key(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn project_root(&self) -> Option<PathBuf> {
        self.project_root.clone()
    }

    pub(crate) fn disk_entries(
        &self,
    ) -> Vec<(&PathBuf, SystemTime, u64, blake3::Hash, &Vec<Symbol>)> {
        self.entries
            .iter()
            .filter_map(|(path, cached)| {
                if cached.symbols.is_empty() {
                    return None;
                }
                Some((
                    path,
                    cached.mtime,
                    cached.size,
                    cached.content_hash,
                    &cached.symbols,
                ))
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

    /// Extract symbols from a file using language-specific query patterns.
    /// Results are cached by `(path, mtime)` — subsequent calls for unchanged
    /// files return the cached symbol table without re-parsing.
    pub fn extract_symbols(&mut self, path: &Path) -> Result<Vec<Symbol>, AftError> {
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
            return Ok(symbols);
        }

        let source = std::fs::read_to_string(path).map_err(|e| AftError::FileNotFound {
            path: format!("{}: {}", path.display(), e),
        })?;
        let size = source.len() as u64;
        let content_hash = content_hash_for_source(&source);

        let symbols = {
            let (tree, lang) = self.parse(path)?;
            extract_symbols_from_tree(&source, tree, lang)?
        };

        // Cache the result
        let mut symbol_cache = self
            .symbol_cache
            .write()
            .map_err(|_| AftError::ParseError {
                message: "symbol cache lock poisoned".to_string(),
            })?;
        if let Some(generation) = self.symbol_cache_generation {
            symbol_cache.insert_for_generation(
                generation,
                canon,
                current_mtime,
                size,
                content_hash,
                symbols.clone(),
            );
        } else {
            symbol_cache.insert(canon, current_mtime, size, content_hash, symbols.clone());
        }

        Ok(symbols)
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

    let query = cached_query_for(lang)?.ok_or_else(|| AftError::InvalidRequest {
        message: format!("no query patterns implemented for {:?} yet", lang),
    })?;

    match lang {
        LangId::TypeScript | LangId::Tsx => extract_ts_symbols(source, &root, query),
        LangId::JavaScript => extract_js_symbols(source, &root, query),
        LangId::Python => extract_py_symbols(source, &root, query),
        LangId::Rust => extract_rs_symbols(source, &root, query),
        LangId::Go => extract_go_symbols(source, &root, query),
        LangId::C => extract_c_symbols(source, &root, query),
        LangId::Cpp => extract_cpp_symbols(source, &root, query),
        LangId::Zig => extract_zig_symbols(source, &root, query),
        LangId::CSharp => extract_csharp_symbols(source, &root, query),
        LangId::Bash => extract_bash_symbols(source, &root, query),
        LangId::Solidity => extract_solidity_symbols(source, &root, query),
        LangId::Scala => extract_scala_symbols(source, &root, query),
        LangId::Java => extract_java_symbols(source, &root, query),
        LangId::Ruby => extract_ruby_symbols(source, &root, query),
        LangId::Kotlin => extract_kotlin_symbols(source, &root, query),
        LangId::Swift => extract_swift_symbols(source, &root, query),
        LangId::Php => extract_php_symbols(source, &root, query),
        LangId::Lua => extract_lua_symbols(source, &root, query),
        LangId::Perl => extract_perl_symbols(source, &root, query),
        LangId::Html | LangId::Markdown | LangId::Vue | LangId::Json => {
            unreachable!("handled before query lookup")
        }
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
                    // Include /** JSDoc */ comments
                    || (kind == "comment"
                        && node_text(source, &prev).starts_with("/**"))
            }
            LangId::Go | LangId::C | LangId::Cpp | LangId::Zig | LangId::CSharp | LangId::Bash => {
                // Include doc comments only if immediately above (no blank line gap)
                kind == "comment" && is_adjacent_line(&prev, &current, source)
            }
            LangId::Solidity
            | LangId::Scala
            | LangId::Java
            | LangId::Kotlin
            | LangId::Swift
            | LangId::Php => {
                // Include `///` doc comments and `/** */` doc blocks if immediately above
                let text = node_text(source, &prev);
                (kind == "comment" || kind == "line_comment" || kind == "block_comment")
                    && (text.starts_with("///") || text.starts_with("/**"))
                    && is_adjacent_line(&prev, &current, source)
            }
            LangId::Ruby | LangId::Lua => {
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
            LangId::Html | LangId::Markdown | LangId::Vue | LangId::Json => false,
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

fn collect_exported_symbol_names(source: &str, root: &Node) -> HashSet<String> {
    let mut exported = HashSet::new();
    collect_exported_symbol_names_inner(source, root, &mut exported);
    exported
}

fn collect_exported_symbol_names_inner(source: &str, node: &Node, exported: &mut HashSet<String>) {
    if node.kind() == "export_statement" {
        collect_names_from_export_statement(source, node, exported);
    }

    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        collect_exported_symbol_names_inner(source, &child, exported);
        if !cursor.goto_next_sibling() {
            break;
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

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "arrow.name" => arrow_name_node = Some(cap.node),
                "arrow.def" => arrow_def_node = Some(cap.node),
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
                // var.value/var.decl removed — not needed
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

        // Arrow function
        if let (Some(name_node), Some(def_node)) = (arrow_name_node, arrow_def_node) {
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
            let is_function_like = lexical_declaration_has_function_value(&def_node);
            let name = node_text(source, &name_node).to_string();
            let already_captured = symbols.iter().any(|s| s.name == name);
            if is_top_level && !is_function_like && !already_captured {
                symbols.push(Symbol {
                    name,
                    kind: SymbolKind::Variable,
                    range: node_range_with_decorators(&def_node, source, lang),
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
        let mut class_name_node = None;
        let mut class_def_node = None;
        let mut method_class_name_node = None;
        let mut method_name_node = None;
        let mut method_def_node = None;

        for cap in m.captures {
            let Some(&name) = capture_names.get(cap.index as usize) else {
                continue;
            };
            match name {
                "fn.name" => fn_name_node = Some(cap.node),
                "fn.def" => fn_def_node = Some(cap.node),
                "arrow.name" => arrow_name_node = Some(cap.node),
                "arrow.def" => arrow_def_node = Some(cap.node),
                "class.name" => class_name_node = Some(cap.node),
                "class.def" => class_def_node = Some(cap.node),
                "method.class_name" => method_class_name_node = Some(cap.node),
                "method.name" => method_name_node = Some(cap.node),
                "method.def" => method_def_node = Some(cap.node),
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

/// Extract symbols from Rust source.
/// Handles: free functions, struct, enum, trait (as Interface), impl methods with scope chains.
fn extract_rs_symbols(source: &str, root: &Node, query: &Query) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::Rust;
    let capture_names = query.capture_names();

    // Collect all visibility_modifier byte ranges first
    let mut vis_ranges: Vec<std::ops::Range<usize>> = Vec::new();
    {
        let vis_idx = capture_names.iter().position(|n| *n == "vis.mod");
        if let Some(idx) = vis_idx {
            let idx = idx as u32;
            let mut cursor = QueryCursor::new();
            let mut matches = cursor.matches(query, *root, source.as_bytes());
            while let Some(m) = {
                matches.advance();
                matches.get()
            } {
                for cap in m.captures {
                    if cap.index == idx {
                        vis_ranges.push(cap.node.byte_range());
                    }
                }
            }
        }
    }

    let is_pub = |node: &Node| -> bool {
        // Check if the node has a visibility_modifier as a direct child
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
        let mut trait_name_node = None;
        let mut trait_def_node = None;
        let mut impl_def_node = None;

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
                "trait.name" => trait_name_node = Some(cap.node),
                "trait.def" => trait_def_node = Some(cap.node),
                "impl.def" => impl_def_node = Some(cap.node),
                _ => {}
            }
        }

        // Free function (not inside impl block — check parent)
        if let (Some(name_node), Some(def_node)) = (fn_name_node, fn_def_node) {
            let parent = def_node.parent();
            let in_impl = parent
                .map(|p| p.kind() == "declaration_list")
                .unwrap_or(false);
            if !in_impl {
                symbols.push(Symbol {
                    name: node_text(source, &name_node).to_string(),
                    kind: SymbolKind::Function,
                    range: node_range_with_decorators(&def_node, source, lang),
                    signature: Some(extract_signature(source, &def_node)),
                    scope_chain: vec![],
                    exported: is_pub(&def_node),
                    parent: None,
                });
            }
        }

        // Struct
        if let (Some(name_node), Some(def_node)) = (struct_name_node, struct_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Struct,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_pub(&def_node),
                parent: None,
            });
        }

        // Enum
        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Enum,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_pub(&def_node),
                parent: None,
            });
        }

        // Trait (mapped to Interface kind)
        if let (Some(name_node), Some(def_node)) = (trait_name_node, trait_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Interface,
                range: node_range_with_decorators(&def_node, source, lang),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_pub(&def_node),
                parent: None,
            });
        }

        // Impl block — extract methods from inside
        if let Some(impl_node) = impl_def_node {
            // Find the type name(s) from the impl
            // `impl TypeName { ... }` → scope = ["TypeName"]
            // `impl Trait for TypeName { ... }` → scope = ["Trait for TypeName"]
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

            let scope_name = if type_names.len() >= 2 {
                // impl Trait for Type
                format!("{} for {}", type_names[0], type_names[1])
            } else if type_names.len() == 1 {
                type_names[0].clone()
            } else {
                String::new()
            };

            let parent_name = type_names.last().cloned().unwrap_or_default();

            // Find declaration_list and extract function_items
            let mut child_cursor = impl_node.walk();
            if child_cursor.goto_first_child() {
                loop {
                    let child = child_cursor.node();
                    if child.kind() == "declaration_list" {
                        let mut fn_cursor = child.walk();
                        if fn_cursor.goto_first_child() {
                            loop {
                                let fn_node = fn_cursor.node();
                                if fn_node.kind() == "function_item" {
                                    if let Some(name_node) = fn_node.child_by_field_name("name") {
                                        symbols.push(Symbol {
                                            name: node_text(source, &name_node).to_string(),
                                            kind: SymbolKind::Method,
                                            range: node_range_with_decorators(
                                                &fn_node, source, lang,
                                            ),
                                            signature: Some(extract_signature(source, &fn_node)),
                                            scope_chain: if scope_name.is_empty() {
                                                vec![]
                                            } else {
                                                vec![scope_name.clone()]
                                            },
                                            exported: is_pub(&fn_node),
                                            parent: if parent_name.is_empty() {
                                                None
                                            } else {
                                                Some(parent_name.clone())
                                            },
                                        });
                                    }
                                }
                                if !fn_cursor.goto_next_sibling() {
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

fn extract_json_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let Some(value) = root.named_child(0) else {
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
                kind: SymbolKind::Class,
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
    let signature = extract_signature(source, node);
    if signature.starts_with("struct ") {
        SymbolKind::Struct
    } else if signature.starts_with("enum ") {
        SymbolKind::Enum
    } else {
        SymbolKind::Class
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
        if child.kind() == "package_name" {
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
                _ => {}
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

fn extract_html_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let mut headings: Vec<(u8, Symbol)> = Vec::new();
    collect_html_headings(source, root, &mut headings);

    let total_lines = source.lines().count() as u32;

    // Extend each heading's end_line to just before the next heading at the
    // same or shallower level (or EOF). This makes aft_zoom return the full
    // section content rather than just the heading element's single line.
    for i in 0..headings.len() {
        let level = headings[i].0;
        let section_end = headings[i + 1..]
            .iter()
            .find(|(l, _)| *l <= level)
            .map(|(_, s)| s.range.start_line.saturating_sub(1))
            .unwrap_or_else(|| total_lines.saturating_sub(1));
        headings[i].1.range.end_line = section_end;
        if section_end != headings[i].1.range.start_line {
            headings[i].1.range.end_col = 0;
        }
    }

    // Build hierarchy: assign scope_chain and parent based on heading level
    let mut scope_stack: Vec<(u8, String)> = Vec::new(); // (level, name)
    for (level, symbol) in headings.iter_mut() {
        // Pop scope entries that are at the same level or deeper
        while scope_stack.last().is_some_and(|(l, _)| *l >= *level) {
            scope_stack.pop();
        }
        symbol.scope_chain = scope_stack.iter().map(|(_, name)| name.clone()).collect();
        symbol.parent = scope_stack.last().map(|(_, name)| name.clone());
        scope_stack.push((*level, symbol.name.clone()));
    }

    Ok(headings.into_iter().map(|(_, s)| s).collect())
}

/// Recursively collect h1-h6 elements from the HTML tree.
fn collect_html_headings(source: &str, node: &Node, headings: &mut Vec<(u8, Symbol)>) {
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
                            headings.push((
                                level,
                                Symbol {
                                    name: text,
                                    kind: SymbolKind::Heading,
                                    range,
                                    signature: Some(signature),
                                    scope_chain: vec![], // filled later
                                    exported: false,
                                    parent: None, // filled later
                                },
                            ));
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

/// Extract markdown headings as symbols.
/// Each heading becomes a symbol with kind `Heading`, and its range covers the entire
/// section (from the heading to the next heading at the same or higher level, or EOF).
fn extract_md_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let mut symbols = Vec::new();
    extract_md_sections(source, root, &mut symbols, &[]);
    Ok(symbols)
}

/// Recursively walk `section` nodes to build the heading hierarchy.
fn extract_md_sections(
    source: &str,
    node: &Node,
    symbols: &mut Vec<Symbol>,
    scope_chain: &[String],
) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }

    loop {
        let child = cursor.node();
        match child.kind() {
            "section" => {
                // A section contains an atx_heading as its first child,
                // followed by content and possibly nested sections.
                let mut section_cursor = child.walk();
                let mut heading_name = String::new();
                let mut heading_level: u8 = 0;

                if section_cursor.goto_first_child() {
                    loop {
                        let section_child = section_cursor.node();
                        if section_child.kind() == "atx_heading" {
                            // Extract heading level from marker type
                            let mut h_cursor = section_child.walk();
                            if h_cursor.goto_first_child() {
                                loop {
                                    let h_child = h_cursor.node();
                                    let kind = h_child.kind();
                                    if kind.starts_with("atx_h") && kind.ends_with("_marker") {
                                        // "atx_h1_marker" → level 1, "atx_h2_marker" → level 2, etc.
                                        heading_level = kind
                                            .strip_prefix("atx_h")
                                            .and_then(|s| s.strip_suffix("_marker"))
                                            .and_then(|s| s.parse::<u8>().ok())
                                            .unwrap_or(1);
                                    } else if h_child.kind() == "inline" {
                                        heading_name =
                                            node_text(source, &h_child).trim().to_string();
                                    }
                                    if !h_cursor.goto_next_sibling() {
                                        break;
                                    }
                                }
                            }
                        }
                        if !section_cursor.goto_next_sibling() {
                            break;
                        }
                    }
                }

                if !heading_name.is_empty() {
                    let range = node_range(&child);
                    let signature = format!(
                        "{} {}",
                        "#".repeat((heading_level as usize).min(6)),
                        heading_name
                    );

                    symbols.push(Symbol {
                        name: heading_name.clone(),
                        kind: SymbolKind::Heading,
                        range,
                        signature: Some(signature),
                        scope_chain: scope_chain.to_vec(),
                        exported: false,
                        parent: scope_chain.last().cloned(),
                    });

                    // Recurse into the section for nested headings
                    let mut new_scope = scope_chain.to_vec();
                    new_scope.push(heading_name);
                    extract_md_sections(source, &child, symbols, &new_scope);
                }
            }
            _ => {}
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }
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
    parser: RefCell<FileParser>,
}

#[derive(Debug, Clone)]
struct ReExportTarget {
    file: PathBuf,
    symbol_name: String,
}

impl TreeSitterProvider {
    /// Create a new `TreeSitterProvider` backed by a fresh `FileParser`.
    pub fn new() -> Self {
        Self::with_symbol_cache(Arc::new(RwLock::new(SymbolCache::new())))
    }

    /// Create a new `TreeSitterProvider` backed by a shared symbol cache.
    pub fn with_symbol_cache(symbol_cache: SharedSymbolCache) -> Self {
        Self {
            parser: RefCell::new(FileParser::with_symbol_cache(symbol_cache)),
        }
    }

    /// Return shared symbol cache entries for status reporting.
    pub fn symbol_cache_len(&self) -> usize {
        let parser = self.parser.borrow();
        parser.symbol_cache_len()
    }

    /// Shared symbol cache backing this provider.
    pub fn symbol_cache(&self) -> SharedSymbolCache {
        let parser = self.parser.borrow();
        parser.symbol_cache()
    }

    fn resolve_symbol_inner(
        &self,
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

        let symbols = self.parser.borrow_mut().extract_symbols(file)?;
        let local_matches = symbol_matches_in_file(file, &symbols, name);
        if !local_matches.is_empty() {
            return Ok(local_matches);
        }

        if name == "default" {
            let default_matches = self.resolve_local_default_export(file, &symbols)?;
            if !default_matches.is_empty() {
                return Ok(default_matches);
            }
        }

        let reexport_targets = self.collect_reexport_targets(file, name)?;
        let mut matches = Vec::new();
        let mut seen = HashSet::new();
        for target in reexport_targets {
            for resolved in
                self.resolve_symbol_inner(&target.file, &target.symbol_name, depth + 1, visited)?
            {
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
        file: &Path,
        requested_name: &str,
    ) -> Result<Vec<ReExportTarget>, AftError> {
        let (source, tree, lang) = self.read_parsed_file(file)?;
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
        file: &Path,
        symbols: &[Symbol],
    ) -> Result<Vec<SymbolMatch>, AftError> {
        let (source, tree, lang) = self.read_parsed_file(file)?;
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

    fn read_parsed_file(&self, file: &Path) -> Result<(String, Tree, LangId), AftError> {
        let source = std::fs::read_to_string(file).map_err(|e| AftError::FileNotFound {
            path: format!("{}: {}", file.display(), e),
        })?;
        let (tree, lang) = {
            let mut parser = self.parser.borrow_mut();
            parser.parse_cloned(file)?
        };
        Ok((source, tree, lang))
    }
}

fn symbol_matches_in_file(file: &Path, symbols: &[Symbol], name: &str) -> Vec<SymbolMatch> {
    symbols
        .iter()
        .filter(|symbol| symbol.name == name)
        .cloned()
        .map(|symbol| SymbolMatch {
            file: file.display().to_string(),
            symbol,
        })
        .collect()
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
    let mut cursor = export_stmt.walk();
    if !cursor.goto_first_child() {
        return None;
    }

    loop {
        let child = cursor.node();
        match child.kind() {
            "function_declaration"
            | "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "type_alias_declaration"
            | "lexical_declaration" => {
                if let Some(name_node) = child.child_by_field_name("name") {
                    return Some(node_text(source, &name_node).to_string());
                }

                if child.kind() == "lexical_declaration" {
                    let mut child_cursor = child.walk();
                    if child_cursor.goto_first_child() {
                        loop {
                            let nested = child_cursor.node();
                            if nested.kind() == "variable_declarator" {
                                if let Some(name_node) = nested.child_by_field_name("name") {
                                    return Some(node_text(source, &name_node).to_string());
                                }
                            }
                            if !child_cursor.goto_next_sibling() {
                                break;
                            }
                        }
                    }
                }
            }
            "identifier" | "type_identifier" => {
                let text = node_text(source, &child);
                if text != "export" && text != "default" {
                    return Some(text.to_string());
                }
            }
            _ => {}
        }

        if !cursor.goto_next_sibling() {
            break;
        }
    }

    None
}

impl crate::language::LanguageProvider for TreeSitterProvider {
    fn resolve_symbol(&self, file: &Path, name: &str) -> Result<Vec<SymbolMatch>, AftError> {
        let matches = self.resolve_symbol_inner(file, name, 0, &mut HashSet::new())?;

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
        self.parser.borrow_mut().extract_symbols(file)
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
    use std::path::PathBuf;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
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
        let loaded = restored.load_from_disk(storage.path(), "unit-project", project.path());
        let symbols = restored.get(&source, mtime).expect("restored symbols");

        assert_eq!(loaded, 1);
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "cached");
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
        let loaded = restored.load_from_disk(storage.path(), "stale-unit-project", project.path());

        assert_eq!(loaded, 0);
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
}
