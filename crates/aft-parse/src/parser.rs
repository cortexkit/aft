use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, RwLock};
use std::time::SystemTime;

#[cfg(any(test, feature = "test-support"))]
use crate::callgraph_store::join::Parser;
use streaming_iterator::StreamingIterator;
#[cfg(not(any(test, feature = "test-support")))]
use tree_sitter::Parser;
use tree_sitter::{Node, Query, QueryCursor, Tree};

use crate::cache_freshness::{self};
use crate::callgraph::resolve_module_path;
use crate::callgraph_store::join::syntax_index::{Direct, Nav};
use crate::error::AftError;
use crate::symbols::{Range, Symbol, SymbolKind, SymbolMatch};

mod file_language;
pub use file_language::dynamic_language_for_extension;
mod grammar_selection;
pub use grammar_selection::grammar_for;
pub mod dead_code_facts;
pub mod outline_facts;
pub mod todo_markers;

const MAX_REEXPORT_DEPTH: usize = 10;

/// Upper bound on the bytes kept for one symbol's signature.
///
/// A signature is the first source line of the definition. In minified or
/// generated code that line can be the rest of the file, and every nested
/// symbol repeats it, so one file could serialize to hundreds of megabytes of
/// symbol data. Real signatures are far shorter than this bound.
pub const MAX_SIGNATURE_BYTES: usize = 1024;

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

;; callable fields inside classes
(class_declaration
  name: (type_identifier) @class_property.class_name
  body: (class_body
    (public_field_definition
      name: (_) @class_property.name
      value: (arrow_function) @class_property.value) @class_property.def))
(class_declaration
  name: (type_identifier) @class_property.class_name
  body: (class_body
    (public_field_definition
      name: (_) @class_property.name
      value: (function_expression) @class_property.value) @class_property.def))

;; interface declarations and callable members
(interface_declaration
  name: (type_identifier) @interface.name) @interface.def
(interface_declaration
  name: (type_identifier) @interface_member.interface_name
  body: (interface_body
    (method_signature
      name: (_) @interface_member.name) @interface_member.def))
(interface_declaration
  name: (type_identifier) @interface_property.interface_name
  body: (interface_body
    (property_signature
      name: (_) @interface_property.name) @interface_property.def))

;; callable object-literal properties
(lexical_declaration
  (variable_declarator
    name: (identifier) @object_property.object_name
    value: (object
      (pair
        key: (_) @object_property.name
        value: (arrow_function) @object_property.value) @object_property.def)))
(lexical_declaration
  (variable_declarator
    name: (identifier) @object_property.object_name
    value: (object
      (pair
        key: (_) @object_property.name
        value: (function_expression) @object_property.value) @object_property.def)))
(lexical_declaration
  (variable_declarator
    name: (identifier) @object_property.object_name
    value: (object
      (pair
        key: (_) @object_property.name
        value: (generator_function) @object_property.value) @object_property.def)))
(lexical_declaration
  (variable_declarator
    name: (identifier) @object_property.object_name
    value: (object
      (method_definition
        name: (_) @object_property.name) @object_property.def)))

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

#[cfg(any(test, feature = "test-support"))]
pub const RS_QUERY: &str = r#"
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
[(type_spec
    name: (type_identifier) @type.name
    type: (_) @type.body)
 (type_alias
    name: (type_identifier) @type.name
    type: (_) @type.body)] @type.def
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

pub use crate::lang_id::LangId;

/// Detect a file's language, resolving ambiguous headers from their contents.
pub fn detect_language(path: &Path) -> Option<LangId> {
    if is_h_header(path) {
        return read_source(path)
            .ok()
            .and_then(|source| detect_language_from_source(path, &source))
            .or(Some(LangId::C));
    }
    language_for_extension(path)
}

/// Dynamic extensions are opt-in per handling context. Detection never reads
/// stored grammars and never starts a download.
pub fn detect_language_any(path: &Path, dynamic_languages: bool) -> Option<LangId> {
    detect_language(path).or_else(|| dynamic_language_for_extension(path, dynamic_languages))
}

/// Detect against the caller's source snapshot rather than reopening the file.
pub fn detect_language_from_source(path: &Path, source: &str) -> Option<LangId> {
    let lang = language_for_extension(path)?;
    if is_h_header(path) {
        return parse_resolved_source(path, source, lang)
            .ok()
            .map(|(_, lang)| lang);
    }
    Some(lang)
}

fn is_h_header(path: &Path) -> bool {
    path.extension().and_then(|ext| ext.to_str()) == Some("h")
}

pub use crate::lang_id::language_by_extension as language_for_extension;

/// The most container blocks (block quotes and list items) a Markdown file
/// may hold open at once and still be parsed. The Markdown grammar's block
/// scanner saves its open blocks into tree-sitter's fixed 1024-byte state
/// buffer without a bounds check, 4 bytes per block after a 5-byte header,
/// so 255 open blocks overrun it and tree-sitter aborts the whole process
/// (`tree-sitter-md` 0.5.3 under tree-sitter 0.26). Half that leaves room
/// for the overestimate [`markdown_too_deep_to_parse`] makes.
pub const MARKDOWN_MAX_OPEN_BLOCKS: usize = 128;

/// Why a Markdown file over [`MARKDOWN_MAX_OPEN_BLOCKS`] is not parsed. Tools
/// report it after the file's path, as `<path>: <reason>`.
pub const MARKDOWN_TOO_DEEP: &str =
    "markdown is nested too deeply to parse safely (more than 128 open blocks); not parsed";

/// Whether `source`, to be parsed as `lang`, is Markdown that could hold
/// more than [`MARKDOWN_MAX_OPEN_BLOCKS`] container blocks open at once, and
/// so must not reach the Markdown grammar. Every place that parses
/// Markdown checks this first.
///
/// A linear scan over each line's container prefix (the run of `>`, list
/// markers and whitespace before the line's text) bounds the blocks open on
/// that line from above: each block a line opens or continues takes a `>`
/// or a list marker on it, or, for a list item continued by indentation, at
/// least two columns of whitespace. A block cannot open on a line without
/// such a mark, so the most blocks ever open is at most the largest line's
/// count, plus one for a code block or other block of the scanner's own.
pub fn markdown_too_deep_to_parse(lang: LangId, source: &str) -> bool {
    lang == LangId::Markdown
        && source
            .split('\n')
            .any(|line| markdown_line_block_bound(line.as_bytes()) > MARKDOWN_MAX_OPEN_BLOCKS)
}

/// An upper bound on the container blocks open on one Markdown line: its
/// markers, half its other whitespace columns, and one more (see
/// [`markdown_too_deep_to_parse`]).
fn markdown_line_block_bound(line: &[u8]) -> usize {
    let space_at = |index: usize| matches!(line.get(index), Some(b' ' | b'\t'));
    let ends_marker = |index: usize| index >= line.len() || space_at(index);
    let (mut markers, mut columns, mut index) = (0, 0, 0);
    while let Some(&byte) = line.get(index) {
        index = match byte {
            b' ' => {
                columns += 1;
                index + 1
            }
            b'\t' => {
                columns += 4;
                index + 1
            }
            // A block quote marker, with the one space that belongs to it.
            b'>' => {
                markers += 1;
                index + 1 + usize::from(space_at(index + 1))
            }
            b'-' | b'+' | b'*' if ends_marker(index + 1) => {
                markers += 1;
                index + 1 + usize::from(space_at(index + 1))
            }
            b'0'..=b'9' => {
                let digits = line[index..]
                    .iter()
                    .take_while(|byte| byte.is_ascii_digit())
                    .count();
                let delimiter = index + digits;
                if !matches!(line.get(delimiter), Some(b'.' | b')')) || !ends_marker(delimiter + 1)
                {
                    break;
                }
                markers += 1;
                delimiter + 1 + usize::from(space_at(delimiter + 1))
            }
            _ => break,
        };
    }
    markers + columns / 2 + 1
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
        LangId::Cpp | LangId::Cuda | LangId::Metal => Some(CPP_QUERY),
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
        LangId::Toml => None,
        LangId::Luau => None,
        LangId::Nix => None,
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
static CUDA_QUERY_CACHE: LazyLock<Result<Query, String>> =
    LazyLock::new(|| compile_query(LangId::Cuda));
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
    let grammar = grammar_for(lang).map_err(|error| error.to_string())?;
    Query::new(&grammar, query_src)
        .map_err(|error| format!("query compile error for {lang:?}: {error}"))
}

test_support_visibility! {
fn cached_query_for(lang: LangId) -> Result<Option<&'static Query>, AftError> {
    if lang == LangId::Luau {
        grammar_for(lang)?;
        return Ok(None);
    }
    let query = match lang {
        LangId::TypeScript => Some(&*TS_QUERY_CACHE),
        LangId::Tsx => Some(&*TSX_QUERY_CACHE),
        LangId::JavaScript => Some(&*JS_QUERY_CACHE),
        LangId::Python => Some(&*PY_QUERY_CACHE),
        LangId::Go => Some(&*GO_QUERY_CACHE),
        LangId::C => Some(&*C_QUERY_CACHE),
        LangId::Cpp => Some(&*CPP_QUERY_CACHE),
        LangId::Cuda => Some(&*CUDA_QUERY_CACHE),
        LangId::Metal => Some(&*CPP_QUERY_CACHE),
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
        | LangId::Yaml
        | LangId::Toml
        | LangId::Luau
        | LangId::Nix => None,
    };

    query
        .map(|result| {
            result.as_ref().map_err(|message| AftError::ParseError {
                message: message.clone(),
            })
        })
        .transpose()
}
}

thread_local! {
    // A Parser is mutable and not Sync, so each semantic/rayon worker retains its
    // own language parsers without a global lock or cross-thread sharing.
    #[cfg(any(test, feature = "test-support"))]
    pub static REUSABLE_PARSERS: RefCell<HashMap<LangId, Parser>> = RefCell::new(HashMap::new());
    #[cfg(not(any(test, feature = "test-support")))]
    static REUSABLE_PARSERS: RefCell<HashMap<LangId, Parser>> = RefCell::new(HashMap::new());
}

pub fn read_source(path: &Path) -> std::io::Result<String> {
    #[cfg(any(test, feature = "test-support"))]
    work_counters::SOURCE_READS.with(|count| count.set(count.get() + 1));
    if !crate::bounded_io::skip_disclosure_active() {
        return crate::bounded_io::read_to_string(path);
    }
    let mut reader = crate::bounded_io::reader(path, None)?;
    crate::large_read_counts::allow_whole(
        crate::large_read_counts::Site::ParseSource,
        path,
        reader.size_at_open(),
    )?;
    let bytes = reader.append(Vec::new(), reader.size_at_open() as usize)?;
    let source = String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    crate::large_read_counts::record_whole(
        crate::large_read_counts::Site::ParseSource,
        source.len(),
    );
    Ok(source)
}

test_support_visibility! {
/// Keep the completed read's allocation; an Arc<str> conversion copies its bytes.
fn captured_source(source: String) -> Arc<String> {
    Arc::new(source)
}
}

/// Parse source with a parser retained by the current worker thread.
pub fn parse_source_with_cached_parser(
    path: &Path,
    source: &str,
    lang: LangId,
) -> Result<Tree, AftError> {
    parse_resolved_source(path, source, lang).map(|(tree, _)| tree)
}

#[derive(Default)]
struct HeaderParseCache {
    entries: HashMap<blake3::Hash, (Tree, LangId)>,
    order: VecDeque<blake3::Hash>,
}

// FileParser instances are request-local. Share header trees by content so a
// language-only consumer and a later symbol/graph consumer use the same verdict
// without repeating the fallback parses. Eviction affects reuse, not results.
static HEADER_PARSE_CACHE: LazyLock<Mutex<HeaderParseCache>> =
    LazyLock::new(|| Mutex::new(HeaderParseCache::default()));

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static HEADER_PARSE_ON_MISS: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
}

/// Reuse a resolved header tree in path-free extraction without changing the
/// caller's already-resolved language.
pub fn cached_header_tree(source: &str, lang: LangId) -> Option<Tree> {
    if !matches!(lang, LangId::C | LangId::Cpp | LangId::ObjC) {
        return None;
    }
    HEADER_PARSE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .get(&cache_freshness::hash_bytes(source.as_bytes()))
        .filter(|(_, resolved)| *resolved == lang)
        .map(|(tree, _)| tree.clone())
}

pub fn parse_resolved_source(
    path: &Path,
    source: &str,
    lang: LangId,
) -> Result<(Tree, LangId), AftError> {
    if !is_h_header(path) {
        return parse_source_with_parser(path, source, lang).map(|tree| (tree, lang));
    }
    let hash = cache_freshness::hash_bytes(source.as_bytes());
    if let Some(parsed) = HEADER_PARSE_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entries
        .get(&hash)
    {
        return Ok(parsed.clone());
    }

    #[cfg(any(test, feature = "test-support"))]
    HEADER_PARSE_ON_MISS.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });

    let c_tree = parse_source_with_parser(path, source, LangId::C)?;
    let mut resolved = (c_tree.clone(), LangId::C);
    if c_tree.root_node().has_error() {
        for candidate in [LangId::Cpp, LangId::ObjC] {
            let Some(tree) = parse_source_with_parser_deadline(
                path,
                source,
                candidate,
                Some(HEADER_FALLBACK_PARSE_DEADLINE),
            )?
            else {
                continue;
            };
            // has_error includes both ERROR nodes and inserted MISSING nodes.
            if !tree.root_node().has_error() {
                resolved = (tree, candidate);
                break;
            }
        }
    }
    // Parsing must not serialize unrelated headers behind this cache lock.
    // Racing misses may both parse; return the first inserted verdict to both
    // callers, including when one of their fallback attempts was cancelled.
    // Wall deadlines are load-sensitive: a near-budget clean alternative could
    // finish in only one racing thread. The 1,900 ms budget leaves over 10x Zig's
    // measured 189.362 ms 99.9th-percentile Objective-C header parse time as a
    // margin (see the deadline constant below). Returning the stored verdict
    // keeps callers consistent at that boundary.
    let mut cache = HEADER_PARSE_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(parsed) = cache.entries.get(&hash) {
        return Ok(parsed.clone());
    }
    // Bound retained trees independently of the number of FileParser instances.
    if cache.entries.len() == 128 {
        if let Some(oldest) = cache.order.pop_front() {
            cache.entries.remove(&oldest);
        }
    }
    cache.order.push_back(hash);
    cache.entries.insert(hash, resolved.clone());
    Ok(resolved)
}

test_support_visibility! {
fn parse_source_with_parser(path: &Path, source: &str, lang: LangId) -> Result<Tree, AftError> {
    parse_source_with_parser_deadline(path, source, lang, None)?.ok_or_else(|| {
        AftError::ParseError {
            message: format!("tree-sitter parse returned None for {}", path.display()),
        }
    })
}
}

test_support_visibility! {
// Optimized native parses on 8 Linux CPUs: 11,492 Zig and 10,733 Linux headers.
// C++/Objective-C 99.9th-percentile parse times were 163.527/189.362 ms (Zig),
// 13.268/16.352 ms (Linux). Take 10x the largest, rounded up to 50 ms, with a 250 ms
// floor: 1,900 ms. Three errorful attempts on two Zig headers were cut; no
// selected trees changed. This is cooperative cancellation, not a memory cap
// or a bound on one native lexer/scanner call between progress callbacks.
const HEADER_FALLBACK_PARSE_DEADLINE: std::time::Duration = std::time::Duration::from_millis(1900);
}

test_support_visibility! {
fn parse_source_with_parser_deadline(
    path: &Path,
    source: &str,
    lang: LangId,
    deadline: Option<std::time::Duration>,
) -> Result<Option<Tree>, AftError> {
    if markdown_too_deep_to_parse(lang, source) {
        return Err(AftError::ParseError {
            message: format!("{}: {MARKDOWN_TOO_DEEP}", path.display()),
        });
    }
    #[cfg(feature = "test-hooks")]
    SEARCH_PARSE_COUNT.with(|count| count.set(count.get() + 1));
    #[cfg(any(test, feature = "test-support"))]
    work_counters::CACHED_PARSER_PARSES.with(|count| count.set(count.get() + 1));
    REUSABLE_PARSERS.with(|parsers| {
        let mut parsers = parsers.borrow_mut();
        let mut parser = match parsers.remove(&lang) {
            Some(parser) => parser,
            None => {
                #[cfg(any(test, feature = "test-support"))]
                work_counters::GRAMMAR_LOADS.with(|count| count.set(count.get() + 1));
                let mut parser = Parser::new();
                initialize_parser(&mut parser, lang)?;
                parser
            }
        };
        #[cfg(any(test, feature = "test-support"))]
        parser.note_parse();
        let result = if let Some(deadline) = deadline {
            let start = std::time::Instant::now();
            let mut expired = false;
            let mut progress = |_: &tree_sitter::ParseState| {
                if start.elapsed() >= deadline {
                    expired = true;
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            };
            let bytes = source.as_bytes();
            let tree = parser.parse_with_options(
                &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
                None,
                Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
            );
            // A cancelled parser retains partial native state. Discard it rather
            // than resuming that state on the next unrelated header.
            if expired {
                Ok(None)
            } else {
                tree.map(Some).ok_or_else(|| AftError::ParseError {
                    message: format!("tree-sitter parse returned None for {}", path.display()),
                })
            }
        } else {
            parse_with_parser(&mut parser, path, source, lang, None).map(Some)
        };
        if matches!(result, Ok(Some(_))) {
            parsers.insert(lang, parser);
        }
        result
    })
}
}

// The five largest react-lua runtime modules took 3.4-7.3 s each on an
// unoptimized build using Wasmtime's Pulley bytecode interpreter. Release
// builds are faster; 30 s leaves about fourfold margin over the slowest
// measured file while bounding host parsing.
const LUAU_PARSE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

#[doc(hidden)]
pub fn parse_luau_source(path: &Path, source: &str) -> Result<Tree, AftError> {
    parse_source_with_cached_parser(path, source, LangId::Luau)
}

#[cfg(feature = "test-hooks")]
thread_local! {
    static LUAU_TEST_DEADLINE: std::cell::Cell<Option<std::time::Duration>> = const { std::cell::Cell::new(None) };
}

#[cfg(feature = "test-hooks")]
#[doc(hidden)]
pub fn with_luau_parse_deadline_for_test<T>(
    deadline: std::time::Duration,
    run: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<std::time::Duration>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LUAU_TEST_DEADLINE.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(LUAU_TEST_DEADLINE.with(|slot| slot.replace(Some(deadline))));
    run()
}

#[cfg(all(
    any(test, feature = "test-support"),
    feature = "test-hooks",
    feature = "wasm-grammars"
))]
pub fn install_luau_parser_for_test(parser: Parser) {
    REUSABLE_PARSERS.with(|parsers| {
        parsers.borrow_mut().insert(LangId::Luau, parser);
    });
}

#[cfg(all(
    any(test, feature = "test-support"),
    feature = "test-hooks",
    feature = "wasm-grammars"
))]
pub fn retains_luau_parser_for_test() -> bool {
    REUSABLE_PARSERS.with(|parsers| parsers.borrow().contains_key(&LangId::Luau))
}

/// The process's Luau budget, also used to describe persisted timeout gaps.
pub fn luau_parse_deadline() -> std::time::Duration {
    #[cfg(feature = "test-hooks")]
    let deadline = {
        static FROM_ENV: std::sync::LazyLock<Option<std::time::Duration>> =
            std::sync::LazyLock::new(|| {
                std::env::var("AFT_TEST_GRAMMAR_PARSE_DEADLINE_MS")
                    .ok()
                    .map(|value| {
                        std::time::Duration::from_millis(
                            value.parse().expect("test parse deadline milliseconds"),
                        )
                    })
            });
        LUAU_TEST_DEADLINE
            .with(|slot| slot.get())
            .or(*FROM_ENV)
            .unwrap_or(LUAU_PARSE_DEADLINE)
    };
    #[cfg(not(feature = "test-hooks"))]
    let deadline = LUAU_PARSE_DEADLINE;
    deadline
}

/// One parsing boundary for fresh and reused dynamic-language parsers.
pub fn parse_with_parser(
    parser: &mut tree_sitter::Parser,
    path: &Path,
    source: &str,
    lang: LangId,
    old_tree: Option<&Tree>,
) -> Result<Tree, AftError> {
    let no_tree = || AftError::ParseError {
        message: format!("tree-sitter parse returned None for {}", path.display()),
    };
    let Some(language) = crate::grammar::extractor_name(lang) else {
        return parser.parse(source, old_tree).ok_or_else(no_tree);
    };
    let deadline =
        crate::grammar::parse_deadline(language).expect("extractor has a parse deadline");
    let start = std::time::Instant::now();
    let mut expired = false;
    // This bounds host parsing, but cannot interrupt one lex or scanner call
    // that never returns: Tree-sitter exposes no Wasmtime store-budget hook.
    // Built-in native grammars have the same single-call interruption gap.
    let mut progress = |_: &tree_sitter::ParseState| {
        if start.elapsed() >= deadline {
            expired = true;
            std::ops::ControlFlow::Break(())
        } else {
            std::ops::ControlFlow::Continue(())
        }
    };
    let bytes = source.as_bytes();
    let tree = parser.parse_with_options(
        &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
        old_tree,
        Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
    );
    if expired || start.elapsed() >= deadline {
        return Err(AftError::GrammarParseTimeout {
            path: path.display().to_string(),
            language: language.into(),
            timeout_ms: deadline.as_millis().min(u64::MAX as u128) as u64,
        });
    }
    tree.ok_or_else(no_tree)
}

/// A Wasm language must have a store attached before set_language can run it.
pub fn initialize_parser(parser: &mut tree_sitter::Parser, lang: LangId) -> Result<(), AftError> {
    let grammar = grammar_for(lang)?;
    if let Some(language) = crate::grammar::extractor_name(lang) {
        crate::grammar::prepare_parser(parser, language)?;
    }
    parser
        .set_language(&grammar)
        .map_err(|error| AftError::ParseError {
            message: format!("grammar init failed for {lang:?}: {error}"),
        })
}

#[cfg(feature = "test-hooks")]
thread_local! {
    static SEARCH_PARSE_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Actual tree-sitter parses on this thread, for search workload measurements.
#[cfg(feature = "test-hooks")]
pub fn search_parse_count_for_test() -> u64 {
    SEARCH_PARSE_COUNT.with(std::cell::Cell::get)
}

test_support_visibility! {
/// Cached parse result: mtime at parse time + the tree.
struct CachedTree {
    mtime: SystemTime,
    size: u64,
    content_hash: blake3::Hash,
    tree: Tree,
    lang: LangId,
    #[cfg(any(test, feature = "test-support"))]
    pub source: Option<Arc<String>>,
    #[cfg(not(any(test, feature = "test-support")))]
    source: Option<Arc<String>>,
}
}

/// Outline-only module containers; these never enter the extracted symbol table.
#[derive(Clone, PartialEq, Eq)]
pub struct RustModule {
    pub name: String,
    pub range: Range,
    pub exported: bool,
    pub children: Vec<RustModule>,
}

pub fn rust_modules(root: Node<'_>, source: &str) -> Vec<RustModule> {
    #[cfg(any(test, feature = "test-support"))]
    work_counters::RUST_MODULE_SCANS.with(|count| count.set(count.get() + 1));
    fn immediate_modules<'a>(node: Node<'a>, modules: &mut Vec<Node<'a>>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "mod_item" {
                modules.push(child);
            } else {
                immediate_modules(child, modules);
            }
        }
    }
    let mut modules = Vec::new();
    immediate_modules(root, &mut modules);
    modules
        .into_iter()
        .map(|module| {
            let mut cursor = module.walk();
            let exported = module
                .named_children(&mut cursor)
                .any(|child| child.kind() == "visibility_modifier");
            RustModule {
                name: module
                    .child_by_field_name("name")
                    .map(|name| node_text(source, &name))
                    .unwrap_or("module")
                    .to_string(),
                range: node_range_with_decorators(&module, source, LangId::Rust),
                exported,
                children: module
                    .child_by_field_name("body")
                    .map(|body| rust_modules(body, source))
                    .unwrap_or_default(),
            }
        })
        .collect()
}

/// A request-local source and tree captured together, for read-only consumers.
pub struct ParsedFile {
    pub source: Arc<String>,
    pub tree: Tree,
    pub lang: LangId,
    pub modules: Option<Arc<Vec<RustModule>>>,
    mtime: SystemTime,
    size: u64,
    content_hash: blake3::Hash,
}

test_support_visibility! {
/// Cached symbol extraction result: mtime at extraction time + symbols.
#[derive(Clone)]
struct CachedSymbols {
    #[cfg(any(test, feature = "test-support"))]
    pub mtime: SystemTime,
    #[cfg(not(any(test, feature = "test-support")))]
    mtime: SystemTime,
    size: u64,
    content_hash: blake3::Hash,
    /// Shared so a lookup can snapshot an entry under the read lock with a
    /// reference-count bump, and copy the symbols only after the lock is
    /// released and only if the entry turns out to be fresh.
    symbols: Arc<Vec<Symbol>>,
    #[cfg(any(test, feature = "test-support"))]
    pub modules: Option<Arc<Vec<RustModule>>>,
    #[cfg(not(any(test, feature = "test-support")))]
    modules: Option<Arc<Vec<RustModule>>>,
    /// Whether the tree these symbols were extracted from had syntax errors.
    /// `None` when the symbols arrived without a tree (a disk load or a
    /// prewarm insert). Outline uses it to skip broken files without parsing.
    has_error: Option<bool>,
    /// Structure-map facts (test regions, trait method previews) that the
    /// multi-file outline derived from the same tree. They share this entry's
    /// freshness key and are dropped with it when the file's content changes.
    outline_facts: Option<Arc<outline_facts::OutlineParseFacts>>,
    /// The symbols are a table shared with other checkouts' caches, whose
    /// memory the process-wide table cache counts once (see
    /// [`SymbolCache::insert_shared_table`]); this cache leaves them out of
    /// its own estimate.
    shared_table: bool,
}
}

/// A fresh cache entry with everything a multi-file outline needs to render
/// a file without reading or parsing it again.
pub struct CachedOutline {
    pub symbols: Arc<Vec<Symbol>>,
    pub modules: Option<Arc<Vec<RustModule>>>,
    pub has_error: bool,
    pub facts: Arc<outline_facts::OutlineParseFacts>,
}

/// The content hash a symbol-cache entry records for `source`: none (zero)
/// past the hashing size cap.
pub fn content_hash_for_source(source: &str) -> blake3::Hash {
    if source.len() as u64 > cache_freshness::CONTENT_HASH_SIZE_CAP {
        cache_freshness::zero_hash()
    } else {
        cache_freshness::hash_bytes(source.as_bytes())
    }
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static FRESHNESS_TEST_BARRIERS: std::cell::RefCell<Option<(std::sync::Arc<std::sync::Barrier>, std::sync::Arc<std::sync::Barrier>)>> = const { std::cell::RefCell::new(None) };
    pub static CACHE_REFRESH_TEST_MUTATOR: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
    pub static CACHE_REFRESH_TEST_BARRIERS: std::cell::RefCell<Option<(std::sync::Arc<std::sync::Barrier>, std::sync::Arc<std::sync::Barrier>)>> = const { std::cell::RefCell::new(None) };
}

test_support_visibility! {
fn cached_file_is_fresh(
    path: &Path,
    cached_mtime: SystemTime,
    cached_size: u64,
    cached_content_hash: blake3::Hash,
    fallback_mtime: SystemTime,
) -> bool {
    #[cfg(any(test, feature = "test-support"))]
    FRESHNESS_TEST_BARRIERS.with(|slot| {
        if let Some((arrived, released)) = slot.borrow_mut().take() {
            arrived.wait();
            released.wait();
        }
    });
    let Ok(metadata) = crate::bounded_io::metadata(path) else {
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
}

/// Shared symbol cache that can be pre-warmed in a background thread
/// and read by all parser instances.
#[derive(Clone, Default)]
pub struct SymbolCache {
    #[cfg(any(test, feature = "test-support"))]
    pub entries: HashMap<PathBuf, CachedSymbols>,
    #[cfg(not(any(test, feature = "test-support")))]
    entries: HashMap<PathBuf, CachedSymbols>,
    // Payload totals exclude the root, which is counted only for a nonempty map.
    entry_bytes: u64,
    symbol_count: usize,
    generation: u64,
    // Revisions distinguish real entry changes from successful parses that reuse cached data.
    mutation_revision: u64,
    persisted_revision: u64,
    project_root: Option<PathBuf>,
}

pub type SharedSymbolCache = Arc<RwLock<SymbolCache>>;

pub fn symbols_estimated_bytes(symbols: &[Symbol]) -> u64 {
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

fn module_bytes(modules: &[RustModule]) -> u64 {
    modules
        .iter()
        .map(|module| {
            std::mem::size_of::<RustModule>() as u64
                + module.name.len() as u64
                + module_bytes(&module.children)
        })
        .sum()
}

/// The bytes `cached` holds for this cache alone: a shared symbol table is
/// counted where it is shared, once for the process.
fn cached_symbols_estimated_bytes(path: &Path, cached: &CachedSymbols) -> u64 {
    (std::mem::size_of::<PathBuf>() as u64)
        .saturating_add(crate::memory::path_bytes(path))
        .saturating_add(std::mem::size_of::<CachedSymbols>() as u64)
        .saturating_add(if cached.shared_table {
            0
        } else {
            symbols_estimated_bytes(&cached.symbols)
        })
        .saturating_add(
            cached
                .modules
                .as_deref()
                .map_or(0, |modules| module_bytes(modules)),
        )
        .saturating_add(
            cached
                .outline_facts
                .as_deref()
                .map_or(0, |facts| facts.estimated_bytes()),
        )
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Parks a disk-cache decode before its next entry, so a test can act on
    /// the shared cache while the decode is in flight.
    static SYMBOL_CACHE_DECODE_TEST_BARRIERS: std::cell::RefCell<Option<(std::sync::Arc<std::sync::Barrier>, std::sync::Arc<std::sync::Barrier>)>> = const { std::cell::RefCell::new(None) };
}

impl SymbolCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            entry_bytes: 0,
            symbol_count: 0,
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
        self.insert_shared(
            path,
            mtime,
            size,
            content_hash,
            Arc::new(symbols),
            None,
            None,
        )
    }

    /// Insert pre-warmed symbols for a file as a table other checkouts'
    /// caches may hold too. The caller keeps the table where the process
    /// counts its memory once, for as long as any cache holds it, so this
    /// cache's estimate leaves the table out.
    pub fn insert_shared_table(
        &mut self,
        path: PathBuf,
        mtime: SystemTime,
        size: u64,
        content_hash: blake3::Hash,
        symbols: Arc<Vec<Symbol>>,
    ) -> bool {
        self.insert_entry(path, mtime, size, content_hash, symbols, None, None, true)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_shared(
        &mut self,
        path: PathBuf,
        mtime: SystemTime,
        size: u64,
        content_hash: blake3::Hash,
        symbols: Arc<Vec<Symbol>>,
        modules: Option<Arc<Vec<RustModule>>>,
        has_error: Option<bool>,
    ) -> bool {
        self.insert_entry(
            path,
            mtime,
            size,
            content_hash,
            symbols,
            modules,
            has_error,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_entry(
        &mut self,
        path: PathBuf,
        mtime: SystemTime,
        size: u64,
        content_hash: blake3::Hash,
        symbols: Arc<Vec<Symbol>>,
        modules: Option<Arc<Vec<RustModule>>>,
        has_error: Option<bool>,
        shared_table: bool,
    ) -> bool {
        if let Some(cached) = self.entries.get_mut(&path) {
            if cached.mtime == mtime
                && cached.size == size
                && cached.content_hash == content_hash
                && cached.symbols == symbols
            {
                // Same content: keep what was already derived from it. A
                // caller without a tree passes `None` and must not erase the
                // module layout, syntax verdict or outline facts on record.
                if modules.is_some() {
                    self.entry_bytes = self
                        .entry_bytes
                        .saturating_sub(
                            cached
                                .modules
                                .as_deref()
                                .map_or(0, |modules| module_bytes(modules)),
                        )
                        .saturating_add(
                            modules
                                .as_deref()
                                .map_or(0, |modules| module_bytes(modules)),
                        );
                    cached.modules = modules;
                }
                if has_error.is_some() {
                    cached.has_error = has_error;
                }
                return false;
            }
        }
        let entry = CachedSymbols {
            mtime,
            size,
            content_hash,
            symbols,
            modules,
            has_error,
            outline_facts: None,
            shared_table,
        };
        let next_bytes = if let Some((stored_path, previous)) = self.entries.get_key_value(&path) {
            self.entry_bytes = self
                .entry_bytes
                .saturating_sub(cached_symbols_estimated_bytes(stored_path, previous));
            self.symbol_count = self.symbol_count.saturating_sub(previous.symbols.len());
            cached_symbols_estimated_bytes(stored_path, &entry)
        } else {
            cached_symbols_estimated_bytes(&path, &entry)
        };
        self.entry_bytes = self.entry_bytes.saturating_add(next_bytes);
        self.symbol_count = self.symbol_count.saturating_add(entry.symbols.len());
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
            cached_file_is_fresh(path, cached.mtime, cached.size, cached.content_hash, mtime).then(
                || {
                    #[cfg(any(test, feature = "test-support"))]
                    work_counters::OWNED_SYMBOL_COPIES.with(|count| count.set(count.get() + 1));
                    cached.symbols.as_ref().clone()
                },
            )
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

    /// Invalidate cached symbols for a specific file.
    pub fn invalidate(&mut self, path: &Path) {
        if let Some((stored_path, previous)) = self.entries.remove_entry(path) {
            self.entry_bytes = self
                .entry_bytes
                .saturating_sub(cached_symbols_estimated_bytes(&stored_path, &previous));
            self.symbol_count = self.symbol_count.saturating_sub(previous.symbols.len());
            self.mutation_revision = self.mutation_revision.wrapping_add(1);
        }
    }

    /// Clear all entries and advance the generation to ignore stale background writers.
    pub fn reset(&mut self) -> u64 {
        self.entries.clear();
        self.entry_bytes = 0;
        self.symbol_count = 0;
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

    /// Whether the cache has an entry for a file.
    pub fn contains_key(&self, path: &Path) -> bool {
        self.entries.contains_key(path)
    }

    /// Number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Cheap counters for reusing a health estimate, including derived outline
    /// payloads that change memory without changing the persistence revision.
    pub fn memory_shape(&self) -> [u64; 5] {
        [
            self.generation,
            self.mutation_revision,
            self.entries.len() as u64,
            self.entry_bytes,
            self.project_root
                .as_deref()
                .map_or(0, crate::memory::path_bytes),
        ]
    }

    /// Estimate the resident hot symbol map from cached paths and the strings
    /// owned by each symbol. Tree-sitter parse trees are request-local and are
    /// therefore not part of this long-lived cache estimate.
    pub fn estimated_memory(&self) -> crate::memory::MemoryEstimate {
        let bytes = if self.entries.is_empty() {
            0
        } else {
            self.entry_bytes.saturating_add(
                self.project_root
                    .as_deref()
                    .map_or(0, crate::memory::path_bytes),
            )
        };
        crate::memory::MemoryEstimate::estimated(bytes)
            .count("entries", self.entries.len())
            .count("symbols", self.symbol_count)
    }

    /// The original full walk, kept independent of the running totals as a
    /// compatibility oracle and an offline performance baseline.
    #[cfg(any(test, feature = "test-support"))]
    pub fn recounted_memory_for_test(&self) -> crate::memory::MemoryEstimate {
        if self.entries.is_empty() {
            return crate::memory::MemoryEstimate::estimated(0)
                .count("entries", 0)
                .count("symbols", 0);
        }
        let entry_bytes = self.entries.iter().fold(0u64, |bytes, (path, cached)| {
            fn module_bytes(modules: &[RustModule]) -> u64 {
                modules
                    .iter()
                    .map(|module| {
                        std::mem::size_of::<RustModule>() as u64
                            + module.name.len() as u64
                            + module_bytes(&module.children)
                    })
                    .sum()
            }
            bytes
                .saturating_add(std::mem::size_of::<PathBuf>() as u64)
                .saturating_add(crate::memory::path_bytes(path))
                .saturating_add(std::mem::size_of::<CachedSymbols>() as u64)
                .saturating_add(if cached.shared_table {
                    0
                } else {
                    symbols_estimated_bytes(&cached.symbols)
                })
                .saturating_add(
                    cached
                        .modules
                        .as_deref()
                        .map_or(0, |modules| module_bytes(modules)),
                )
                .saturating_add(
                    cached
                        .outline_facts
                        .as_deref()
                        .map_or(0, |facts| facts.estimated_bytes()),
                )
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
}

pub fn cached_symbol_count_shared(
    cache: &SharedSymbolCache,
    path: &Path,
    mtime: SystemTime,
    size: u64,
) -> Option<usize> {
    let cached = cache.read().ok()?.entries.get(path)?.clone();
    if cached.mtime == mtime && cached.size == size {
        return Some(cached.symbols.len());
    }
    if !cached_file_is_fresh(path, cached.mtime, cached.size, cached.content_hash, mtime) {
        return None;
    }
    // Hashing runs without a guard. A replacement during that I/O must not
    // turn the old table into a fresh count for the newly installed entry.
    let current = cache.read().ok()?;
    current
        .entries
        .get(path)
        .filter(|entry| Arc::ptr_eq(&entry.symbols, &cached.symbols))
        .map(|_| cached.symbols.len())
}

/// Core parsing engine. Handles language detection, parse tree caching,
/// symbol table caching, and query pattern execution via tree-sitter.
pub struct FileParser {
    #[cfg(any(test, feature = "test-support"))]
    pub cache: HashMap<PathBuf, CachedTree>,
    #[cfg(not(any(test, feature = "test-support")))]
    cache: HashMap<PathBuf, CachedTree>,
    #[cfg(any(test, feature = "test-support"))]
    pub symbol_cache: SharedSymbolCache,
    #[cfg(not(any(test, feature = "test-support")))]
    symbol_cache: SharedSymbolCache,
    symbol_cache_generation: Option<u64>,
    capture_source: bool,
    dynamic_languages: bool,
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
            symbol_cache,
            symbol_cache_generation,
            capture_source: false,
            dynamic_languages: false,
        }
    }

    /// Outline batches retain source only until the matching tree is evicted;
    /// other parser consumers do not need a second resident copy of each file.
    pub fn with_source_capture(mut self) -> Self {
        self.capture_source = true;
        self
    }

    pub fn with_dynamic_languages(mut self, dynamic_languages: bool) -> Self {
        self.dynamic_languages = dynamic_languages;
        self
    }

    /// Parses `source` with this thread's retained parser for `lang` and
    /// counts the parse for tests. Every `FileParser` used to own its own
    /// per-language `Parser`, and most callers build a fresh `FileParser` per
    /// request (the language provider does it per call), so each call paid a
    /// parser allocation and grammar load before parsing.
    pub fn parse_source(path: &Path, source: &str, lang: LangId) -> Result<Tree, AftError> {
        #[cfg(any(test, feature = "test-support"))]
        work_counters::TREE_PARSES.with(|count| count.set(count.get() + 1));
        parse_source_with_cached_parser(path, source, lang).inspect_err(|_| {
            crate::slog_error!("parse failed for {}", path.display());
        })
    }

    /// Number of entries in the shared symbol cache.
    pub fn symbol_cache_len(&self) -> usize {
        self.symbol_cache
            .read()
            .map(|cache| cache.len())
            .unwrap_or(0)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn parse_tree_cache_len(&self) -> usize {
        self.cache.len()
    }

    /// Shared symbol cache backing this parser.
    pub fn symbol_cache(&self) -> SharedSymbolCache {
        Arc::clone(&self.symbol_cache)
    }

    /// Parse a file, returning the tree and detected language. Uses cache if
    /// the file hasn't been modified since last parse.
    pub fn parse(&mut self, path: &Path) -> Result<(&Tree, LangId), AftError> {
        let lang = language_for_extension(path)
            .or_else(|| {
                self.dynamic_languages
                    .then(|| detect_language_any(path, true))
                    .flatten()
            })
            .ok_or_else(|| AftError::InvalidRequest {
                message: format!(
                    "unsupported file extension: {}",
                    path.extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("<none>")
                ),
            })?;

        let canon = path.to_path_buf();
        let current_mtime = crate::bounded_io::metadata(path)
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
            #[cfg(any(test, feature = "test-support"))]
            work_counters::FILE_PARSE_READS.with(|count| count.set(count.get() + 1));
            #[cfg(any(test, feature = "test-support"))]
            work_counters::TREE_SOURCE_READS.with(|count| count.set(count.get() + 1));
            let source = read_source(path).map_err(|e| AftError::FileNotFound {
                path: format!("{}: {}", path.display(), e),
            })?;

            let (tree, lang) = parse_resolved_source(path, &source, lang)?;
            #[cfg(any(test, feature = "test-support"))]
            work_counters::TREE_PARSES.with(|count| count.set(count.get() + 1));

            self.cache.insert(
                canon.clone(),
                CachedTree {
                    mtime: current_mtime,
                    size: source.len() as u64,
                    content_hash: content_hash_for_source(&source),
                    tree,
                    lang,
                    source: self.capture_source.then(|| captured_source(source)),
                },
            );
        }

        let cached = self.cache.get(&canon).ok_or_else(|| AftError::ParseError {
            message: format!("parser cache missing entry for {}", path.display()),
        })?;
        Ok((&cached.tree, cached.lang))
    }

    /// Parse `source`, the text a caller already read from `path`, without
    /// reading the file again. The tree cache is shared with
    /// [`FileParser::parse`]: a cached tree is reused only when it was parsed
    /// from the same text (same size and content hash), so the tree always
    /// describes `source`.
    pub fn parse_source_text(
        &mut self,
        path: &Path,
        source: &str,
    ) -> Result<(&Tree, LangId), AftError> {
        let lang = language_for_extension(path).ok_or_else(|| AftError::InvalidRequest {
            message: format!(
                "unsupported file extension: {}",
                path.extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("<none>")
            ),
        })?;
        let size = source.len() as u64;
        let content_hash = content_hash_for_source(source);
        let reusable = size <= cache_freshness::CONTENT_HASH_SIZE_CAP
            && self
                .cache
                .get(path)
                .is_some_and(|cached| cached.size == size && cached.content_hash == content_hash);
        if !reusable {
            let (tree, lang) = parse_resolved_source(path, source, lang)?;
            #[cfg(any(test, feature = "test-support"))]
            work_counters::TREE_PARSES.with(|count| count.set(count.get() + 1));
            self.cache.insert(
                path.to_path_buf(),
                CachedTree {
                    // No stat was taken for this text. The epoch never equals
                    // a real mtime, so a later `parse` of the path checks the
                    // content hash before reusing this tree.
                    mtime: SystemTime::UNIX_EPOCH,
                    size,
                    content_hash,
                    tree,
                    lang,
                    source: self
                        .capture_source
                        .then(|| captured_source(source.to_string())),
                },
            );
        }
        let cached = self.cache.get(path).ok_or_else(|| AftError::ParseError {
            message: format!("parser cache missing entry for {}", path.display()),
        })?;
        Ok((&cached.tree, cached.lang))
    }

    /// Like [`FileParser::parse`] but returns an owned `Tree` clone.
    ///
    /// Useful when the caller needs to hold the tree while also calling
    /// other mutable methods on this parser.
    pub fn parse_cloned(&mut self, path: &Path) -> Result<(Tree, LangId), AftError> {
        let (tree, lang) = self.parse(path)?;
        Ok((tree.clone(), lang))
    }

    pub fn parse_snapshot(&mut self, path: &Path) -> Result<ParsedFile, AftError> {
        self.capture_source = true;
        if self
            .cache
            .get(path)
            .is_some_and(|cached| cached.source.is_none())
        {
            self.evict_parse_tree(path);
        }
        let (_, lang) = self.parse(path)?;
        let cached = &self.cache[path];
        // Cache insertion and outline rendering need the same module layout.
        // Derive it with the source snapshot instead of walking its tree twice.
        let modules = (lang == LangId::Rust).then(|| {
            Arc::new(rust_modules(
                cached.tree.root_node(),
                cached.source.as_deref().expect("source capture enabled"),
            ))
        });
        Ok(ParsedFile {
            source: Arc::clone(cached.source.as_ref().expect("source capture enabled")),
            tree: cached.tree.clone(),
            lang,
            modules,
            mtime: cached.mtime,
            size: cached.size,
            content_hash: cached.content_hash,
        })
    }

    /// Reuse a validated request snapshot rather than reopening its source.
    pub fn extract_symbols_from_snapshot(
        &mut self,
        path: &Path,
        parsed: &ParsedFile,
    ) -> Result<Arc<Vec<Symbol>>, AftError> {
        let cached = self
            .symbol_cache
            .read()
            .map_err(|_| AftError::ParseError {
                message: "symbol cache lock poisoned".into(),
            })?
            .entries
            .get(path)
            .cloned();
        let symbols = match cached {
            Some(cached)
                if cached.size == parsed.size
                    && cached.content_hash == parsed.content_hash
                    && (cached.mtime == parsed.mtime
                        || parsed.content_hash != cache_freshness::zero_hash()) =>
            {
                cached.symbols
            }
            _ => Arc::new(extract_symbols_from_tree(
                &parsed.source,
                &parsed.tree,
                parsed.lang,
            )?),
        };
        let modules = parsed.modules.clone();
        let mut cache = self
            .symbol_cache
            .write()
            .map_err(|_| AftError::ParseError {
                message: "symbol cache lock poisoned".into(),
            })?;
        if self
            .symbol_cache_generation
            .is_none_or(|generation| cache.generation == generation)
        {
            cache.insert_shared(
                path.to_path_buf(),
                parsed.mtime,
                parsed.size,
                parsed.content_hash,
                Arc::clone(&symbols),
                modules,
                Some(parsed.tree.root_node().has_error()),
            );
        }
        Ok(symbols)
    }

    /// The syntax verdict recorded with a still-fresh cache entry for `path`:
    /// `Some(true)` when the file had syntax errors. `None` when there is no
    /// entry, the entry has no verdict, or the file changed since.
    pub fn cached_has_error(&self, path: &Path) -> Option<bool> {
        let current_mtime = crate::bounded_io::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()?;
        let cached = self.symbol_cache.read().ok()?.entries.get(path)?.clone();
        let has_error = cached.has_error?;
        cached_file_is_fresh(
            path,
            cached.mtime,
            cached.size,
            cached.content_hash,
            current_mtime,
        )
        .then_some(has_error)
    }

    /// Everything a multi-file outline stored for `path`, when the entry is
    /// still fresh and carries outline facts. A miss means the caller parses.
    pub fn cached_outline(&self, path: &Path) -> Option<CachedOutline> {
        let current_mtime = crate::bounded_io::metadata(path)
            .and_then(|metadata| metadata.modified())
            .ok()?;
        let cached = self.symbol_cache.read().ok()?.entries.get(path)?.clone();
        let (Some(has_error), Some(facts)) = (cached.has_error, cached.outline_facts) else {
            return None;
        };
        cached_file_is_fresh(
            path,
            cached.mtime,
            cached.size,
            cached.content_hash,
            current_mtime,
        )
        .then_some(CachedOutline {
            symbols: cached.symbols,
            modules: cached.modules,
            has_error,
            facts,
        })
    }

    /// Attach outline facts derived from `parsed` to the cache entry for
    /// `path`, only if that entry still describes the same content. The facts
    /// are not persisted to disk, so attaching them does not mark the cache
    /// as needing a write.
    pub fn store_outline_facts(
        &self,
        path: &Path,
        parsed: &ParsedFile,
        facts: Arc<outline_facts::OutlineParseFacts>,
    ) {
        let Ok(mut cache) = self.symbol_cache.write() else {
            return;
        };
        if self
            .symbol_cache_generation
            .is_some_and(|generation| cache.generation != generation)
        {
            return;
        }
        if let Some(entry) = cache.entries.get_mut(path) {
            if entry.mtime == parsed.mtime
                && entry.size == parsed.size
                && entry.content_hash == parsed.content_hash
            {
                let previous_bytes = entry
                    .outline_facts
                    .as_deref()
                    .map_or(0, |facts| facts.estimated_bytes());
                let next_bytes = facts.estimated_bytes();
                entry.outline_facts = Some(facts);
                cache.entry_bytes = cache
                    .entry_bytes
                    .saturating_sub(previous_bytes)
                    .saturating_add(next_bytes);
            }
        }
    }

    /// Module layout shares the symbol entry's freshness and invalidation lifetime.
    /// Disk-loaded or manually inserted tables have no module layout: their first
    /// outline reads and parses once, then later fresh hits do neither.
    pub fn extract_rust_outline(
        &mut self,
        path: &Path,
    ) -> Result<(Arc<Vec<Symbol>>, Arc<Vec<RustModule>>), AftError> {
        let symbols = self.extract_symbols_shared(path)?;
        let modules = self
            .symbol_cache
            .read()
            .map_err(|_| AftError::ParseError {
                message: "symbol cache lock poisoned".into(),
            })?
            .entries
            .get(path)
            .filter(|entry| Arc::ptr_eq(&entry.symbols, &symbols))
            .and_then(|entry| entry.modules.clone());
        if let Some(modules) = modules {
            return Ok((symbols, modules));
        }
        let parsed = self.parse_snapshot(path)?;
        let symbols = self.extract_symbols_from_snapshot(path, &parsed)?;
        Ok((
            symbols,
            parsed.modules.expect("Rust outline requires Rust source"),
        ))
    }

    test_support_visibility! {
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
            let lang = language_for_extension(path)
                .or_else(|| {
                    self.dynamic_languages
                        .then(|| detect_language_any(path, true))
                        .flatten()
                })
                .ok_or_else(|| AftError::InvalidRequest {
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
                let (tree, lang) = parse_resolved_source(path, source, lang)?;
                #[cfg(any(test, feature = "test-support"))]
                work_counters::TREE_PARSES.with(|count| count.set(count.get() + 1));

                self.cache.insert(
                    canon.clone(),
                    CachedTree {
                        mtime: current_mtime,
                        size,
                        content_hash,
                        tree,
                        lang,
                        source: self
                            .capture_source
                            .then(|| captured_source(source.to_owned())),
                    },
                );
            }

            let cached = self.cache.get(&canon).ok_or_else(|| AftError::ParseError {
                message: format!("parser cache missing entry for {}", path.display()),
            })?;
            Ok((&cached.tree, cached.lang))
        }
    }

    /// Extract symbols from a file using language-specific query patterns.
    /// Results are cached by `(path, mtime)` — subsequent calls for unchanged
    /// files return the cached symbol table without re-parsing.
    pub fn extract_symbols(&mut self, path: &Path) -> Result<Vec<Symbol>, AftError> {
        self.extract_symbols_with_cache_status(path)
            .map(|(symbols, _)| symbols)
    }

    pub fn extract_symbols_with_cache_status(
        &mut self,
        path: &Path,
    ) -> Result<(Vec<Symbol>, bool), AftError> {
        let (symbols, changed, hit) = self.extract_symbols_shared_inner(path)?;
        #[cfg(any(test, feature = "test-support"))]
        if hit {
            work_counters::CACHED_SYMBOL_COPIES.with(|count| count.set(count.get() + 1));
        }
        let _ = hit;
        #[cfg(any(test, feature = "test-support"))]
        if Arc::strong_count(&symbols) > 1 {
            work_counters::OWNED_SYMBOL_COPIES.with(|count| count.set(count.get() + 1));
        }
        Ok((Arc::unwrap_or_clone(symbols), changed))
    }

    /// Borrow the immutable cached table without copying every symbol string.
    pub fn extract_symbols_shared(&mut self, path: &Path) -> Result<Arc<Vec<Symbol>>, AftError> {
        self.extract_symbols_shared_inner(path)
            .map(|(symbols, _, _)| symbols)
    }

    fn extract_symbols_shared_inner(
        &mut self,
        path: &Path,
    ) -> Result<(Arc<Vec<Symbol>>, bool, bool), AftError> {
        // Bound contention retries without growing the stack. The last attempt
        // bypasses the symbol-cache snapshot and parses one coherent source read.
        for attempt in 0..=3 {
            let canon = path.to_path_buf();
            let current_mtime = crate::bounded_io::metadata(path)
                .and_then(|m| m.modified())
                .map_err(|e| AftError::FileNotFound {
                    path: format!("{}: {}", path.display(), e),
                })?;
            // A shared symbol cache cannot opt a built-in-only context into a
            // dynamic language after another context has populated that cache.
            if !self.dynamic_languages && language_for_extension(path).is_none() {
                return Err(AftError::InvalidRequest {
                    message: format!(
                        "unsupported file extension: {}",
                        path.extension()
                            .and_then(|ext| ext.to_str())
                            .unwrap_or("<none>")
                    ),
                });
            }

            // Snapshot the entry before checking the file: hashing a touched file must
            // not block cache writers. Refresh metadata only if the entry is unchanged.
            let cached = if attempt < 3 {
                self.symbol_cache
                    .read()
                    .map_err(|_| AftError::ParseError {
                        message: "symbol cache lock poisoned".to_string(),
                    })?
                    .entries
                    .get(&canon)
                    .cloned()
            } else {
                None
            };
            if let Some(cached) = cached {
                if cached_file_is_fresh(
                    path,
                    cached.mtime,
                    cached.size,
                    cached.content_hash,
                    current_mtime,
                ) {
                    if cached.mtime != current_mtime {
                        #[cfg(any(test, feature = "test-support"))]
                        CACHE_REFRESH_TEST_BARRIERS.with(|slot| {
                            if let Some((arrived, released)) = slot.borrow_mut().take() {
                                arrived.wait();
                                released.wait();
                            }
                        });
                        #[cfg(any(test, feature = "test-support"))]
                        CACHE_REFRESH_TEST_MUTATOR.with(|slot| {
                            if let Some(mutator) = slot.borrow_mut().as_mut() {
                                mutator();
                            }
                        });
                        let mut cache =
                            self.symbol_cache
                                .write()
                                .map_err(|_| AftError::ParseError {
                                    message: "symbol cache lock poisoned".to_string(),
                                })?;
                        if let Some(entry) = cache.entries.get_mut(&canon) {
                            if entry.mtime == cached.mtime
                                && entry.size == cached.size
                                && entry.content_hash == cached.content_hash
                            {
                                entry.mtime = current_mtime;
                            } else {
                                // A concurrent writer replaced the snapshot while we hashed.
                                // Retry rather than returning superseded symbols.
                                drop(cache);
                                continue;
                            }
                        } else {
                            drop(cache);
                            continue;
                        }
                    }
                    return Ok((cached.symbols, false, true));
                }
            }

            let source = read_source(path).map_err(|e| AftError::FileNotFound {
                path: format!("{}: {}", path.display(), e),
            })?;
            let size = source.len() as u64;
            let content_hash = content_hash_for_source(&source);

            let (symbols, modules, has_error) = {
                // Reuse the source we just read instead of letting parse() read the
                // same file a second time.
                let (tree, lang) =
                    self.parse_with_source(path, &source, current_mtime, size, content_hash)?;
                let mut symbols = extract_symbols_from_tree(&source, tree, lang)?;
                if lang == LangId::Rust {
                    complete_rust_trait_members(
                        &mut symbols,
                        crate::callgraph::rust_trait_members(&source, tree.root_node(), Direct),
                    );
                }
                (
                    Arc::new(symbols),
                    (lang == LangId::Rust)
                        .then(|| Arc::new(rust_modules(tree.root_node(), &source))),
                    Some(tree.root_node().has_error()),
                )
            };

            // The syntax verdict comes from the same read and tree as the
            // symbols, on every attempt including the last one that skips the
            // cache snapshot. insert_shared keeps cached outline facts only
            // when the entry already describes this same content.
            let mut symbol_cache = self
                .symbol_cache
                .write()
                .map_err(|_| AftError::ParseError {
                    message: "symbol cache lock poisoned".to_string(),
                })?;
            let cache_changed = if let Some(generation) = self.symbol_cache_generation {
                symbol_cache.generation == generation
                    && symbol_cache.insert_shared(
                        canon,
                        current_mtime,
                        size,
                        content_hash,
                        Arc::clone(&symbols),
                        modules,
                        has_error,
                    )
            } else {
                symbol_cache.insert_shared(
                    canon,
                    current_mtime,
                    size,
                    content_hash,
                    Arc::clone(&symbols),
                    modules,
                    has_error,
                )
            };

            return Ok((symbols, cache_changed, false));
        }
        unreachable!("the uncached final attempt returns")
    }

    /// Drop one local parse tree while preserving its shared extracted symbols.
    ///
    /// Whole-corpus prewarming visits each file once, so retaining finished trees
    /// only increases its peak memory. Other parsing operations keep the default
    /// tree-cache reuse behavior unless they explicitly call this method.
    pub fn evict_parse_tree(&mut self, path: &Path) {
        self.cache.remove(path);
    }

    /// Invalidate cached symbols for a specific file (e.g., after an edit).
    pub fn invalidate_symbols(&mut self, path: &Path) {
        if let Ok(mut symbol_cache) = self.symbol_cache.write() {
            symbol_cache.invalidate(path);
        }
        self.evict_parse_tree(path);
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
    extract_symbols_from_tree_with(source, tree, lang, Direct)
}

/// [`extract_symbols_from_tree`], with `nav` walking a Rust tree and
/// answering its parent and sibling lookups. Other languages run tree-sitter
/// queries, which walk the tree themselves.
pub fn extract_symbols_from_tree_with<'tree>(
    source: &str,
    tree: &'tree Tree,
    lang: LangId,
    nav: impl Nav<'tree>,
) -> Result<Vec<Symbol>, AftError> {
    let mut symbols = extract_symbols_from_tree_unbounded(source, tree, lang, nav)?;
    for symbol in &mut symbols {
        if let Some(signature) = symbol.signature.as_mut() {
            truncate_signature(signature);
        }
    }
    Ok(symbols)
}

/// Cut a signature to [`MAX_SIGNATURE_BYTES`] on a character boundary and mark
/// the cut with an ellipsis so readers can tell it was shortened.
pub fn truncate_signature(signature: &mut String) {
    if signature.len() <= MAX_SIGNATURE_BYTES {
        return;
    }
    let mut end = MAX_SIGNATURE_BYTES;
    while !signature.is_char_boundary(end) {
        end -= 1;
    }
    signature.truncate(end);
    signature.push('…');
}

fn extract_symbols_from_tree_unbounded<'tree>(
    source: &str,
    tree: &'tree Tree,
    lang: LangId,
    nav: impl Nav<'tree>,
) -> Result<Vec<Symbol>, AftError> {
    let root = tree.root_node();

    if lang == LangId::Rust {
        return extract_rs_symbols(source, &root, nav);
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
    if lang == LangId::Toml {
        return extract_toml_symbols(source, &root);
    }
    if lang == LangId::Nix {
        return extract_nix_symbols(source, &root);
    }
    if lang == LangId::Luau {
        return Ok(crate::luau::declarations(source, root)
            .into_iter()
            .map(|declaration| luau_symbol(source, declaration))
            .collect());
    }

    let query = cached_query_for(lang)?.ok_or_else(|| AftError::InvalidRequest {
        message: format!("no query patterns implemented for {:?} yet", lang),
    })?;

    match lang {
        LangId::TypeScript | LangId::Tsx => extract_ts_symbols(source, &root, query, nav),
        LangId::JavaScript => extract_js_symbols(source, &root, query, nav),
        LangId::Python => extract_py_symbols(source, &root, query, nav),
        LangId::Go => extract_go_symbols(source, &root, query),
        LangId::C => extract_c_symbols(source, &root, query),
        LangId::Cpp | LangId::Cuda | LangId::Metal => {
            extract_cpp_symbols(source, &root, query, lang)
        }
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
        LangId::Luau => Err(AftError::InvalidRequest {
            message: format!("no query patterns implemented for {:?} yet", lang),
        }),
        LangId::Rust
        | LangId::Html
        | LangId::Markdown
        | LangId::Vue
        | LangId::Json
        | LangId::Yaml
        | LangId::Toml
        | LangId::Nix => unreachable!("handled before query lookup"),
    }
}

/// Build a Range from a tree-sitter Node.
pub fn node_range(node: &Node) -> Range {
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
pub fn node_range_with_decorators(node: &Node, source: &str, lang: LangId) -> Range {
    node_range_with_decorators_with(node, source, lang, Direct)
}

/// [`node_range_with_decorators`], looking up parents and siblings with
/// `nav`.
pub fn node_range_with_decorators_with<'tree>(
    node: &Node<'tree>,
    source: &str,
    lang: LangId,
    nav: impl Nav<'tree>,
) -> Range {
    if matches!(lang, LangId::Python) {
        if let Some(parent) = nav.parent(*node) {
            if nav.kind(parent) == "decorated_definition" {
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
        if let Some(parent) = nav.parent(*node) {
            if nav.kind(parent) == "export_statement" {
                return node_range_with_decorators_inner(&parent, source, lang, nav);
            }
        }
    }

    node_range_with_decorators_inner(node, source, lang, nav)
}

/// Inner walk that handles preceding-sibling expansion (decorators, doc comments).
/// Split from `node_range_with_decorators` so the export-statement parent path can
/// recurse without re-checking the export wrapper.
fn node_range_with_decorators_inner<'tree>(
    node: &Node<'tree>,
    source: &str,
    lang: LangId,
    nav: impl Nav<'tree>,
) -> Range {
    let mut range = node_range(node);

    let mut current = *node;
    while let Some(prev) = nav.prev_sibling(current) {
        let kind = nav.kind(prev);
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
            | LangId::Cuda
            | LangId::Metal
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
            LangId::Perl | LangId::Luau => false,
            LangId::Python => {
                // Decorators are handled by decorated_definition capture
                false
            }
            LangId::Html
            | LangId::Markdown
            | LangId::Vue
            | LangId::Json
            | LangId::Yaml
            | LangId::Toml
            | LangId::Nix => false,
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

    // Check that there's no blank line between them. Only the bytes between
    // the two nodes are scanned: splitting the whole file into lines here made
    // every documented symbol cost a pass over the entire file. The gap text
    // starts with the rest of `upper`'s last row and ends with the start of
    // `lower`'s first row, so the pieces strictly between the first and last
    // newline are exactly the rows in between.
    let gap_start = upper.end_byte().min(source.len());
    let gap_end = lower.start_byte().clamp(gap_start, source.len());
    let gap = &source[gap_start..gap_end];
    #[cfg(any(test, feature = "test-support"))]
    ADJACENT_LINE_BYTES_SCANNED.with(|bytes| bytes.set(bytes.get() + gap.len()));
    let mut rows = gap.split('\n');
    rows.next();
    let mut rows = rows.peekable();
    while let Some(row) = rows.next() {
        if rows.peek().is_some() && row.trim().is_empty() {
            return false;
        }
    }
    true
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Source bytes `is_adjacent_line` examined on this thread, so a test can
    /// prove the check stays proportional to the gap rather than the file.
    pub static ADJACENT_LINE_BYTES_SCANNED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Extract the text of a node from source.
pub fn node_text<'a>(source: &'a str, node: &Node) -> &'a str {
    source.get(node.byte_range()).unwrap_or("")
}

/// Whether a function expression occurs anywhere below `node`. The search
/// keeps its pending nodes on the heap rather than recursing, so a deeply
/// nested declaration cannot overflow the stack, which would abort the
/// process.
fn lexical_declaration_has_function_value(node: &Node) -> bool {
    let mut pending: Vec<Node> = node.children(&mut node.walk()).collect();
    while let Some(child) = pending.pop() {
        if matches!(
            child.kind(),
            "arrow_function" | "function_expression" | "generator_function"
        ) {
            return true;
        }
        pending.extend(child.children(&mut child.walk()));
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

test_support_visibility! {
/// Byte ranges of every `export_statement` in a TS/JS tree, reduced to the
/// outermost ones and sorted, plus the names those statements export.
///
/// Both come from one walk of the tree. They used to cost a separate full
/// pass of the language's whole symbol query (to find the export statements)
/// on top of this walk, and `is_exported` then compared every symbol with every
/// export statement.
struct ExportIndex {
    #[cfg(any(test, feature = "test-support"))]
    pub ranges: Vec<std::ops::Range<usize>>,
    #[cfg(not(any(test, feature = "test-support")))]
    ranges: Vec<std::ops::Range<usize>>,
    #[cfg(any(test, feature = "test-support"))]
    pub names: HashSet<String>,
    #[cfg(not(any(test, feature = "test-support")))]
    names: HashSet<String>,
}
}

test_support_visibility! {
fn collect_exports<'tree>(source: &str, root: &Node<'tree>, nav: impl Nav<'tree>) -> ExportIndex {
    let mut ranges = Vec::new();
    let names = walk_export_statements(source, root, nav, |node| ranges.push(node.byte_range()));
    // Keep only ranges no other range contains. A node is inside some export
    // statement exactly when it is inside one of the outermost ones, and once
    // contained ranges are gone, both starts and ends increase along the list,
    // which `is_exported` relies on for its binary search.
    ranges.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut outermost: Vec<std::ops::Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        if outermost
            .last()
            .is_some_and(|last| last.start <= range.start && range.end <= last.end)
        {
            continue;
        }
        outermost.push(range);
    }
    ExportIndex {
        ranges: outermost,
        names,
    }
}
}

test_support_visibility! {
/// Check if a node's byte range is contained within any export statement.
///
/// `export_ranges` comes from [`collect_exports`]: sorted, with no range
/// containing another, so starts and ends both increase. Of the ranges that
/// start at or before the node, the last one reaches furthest, so it is the
/// only one that needs checking.
fn is_exported(node: &Node, export_ranges: &[std::ops::Range<usize>]) -> bool {
    let r = node.byte_range();
    let candidates = export_ranges.partition_point(|er| er.start <= r.start);
    #[cfg(any(test, feature = "test-support"))]
    EXPORT_RANGE_COMPARISONS.with(|count| count.set(count.get() + 1));
    candidates
        .checked_sub(1)
        .is_some_and(|index| r.end <= export_ranges[index].end)
}
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    /// Containment checks `is_exported` made on this thread.
    pub static EXPORT_RANGE_COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Names exported by the tree's export statements. Symbol extraction gets
/// them from [`collect_exports`]; tests use this to check that walk.
#[cfg(any(test, feature = "test-support"))]
pub fn collect_exported_symbol_names(source: &str, root: &Node) -> HashSet<String> {
    walk_export_statements(source, root, Direct, |_| {})
}

/// Pre-order walk of the whole tree: calls `on_export` for every
/// `export_statement` and returns the names they export.
///
/// Iterative with one cursor on purpose: the recursive form spent a stack
/// frame per tree depth, and a large non-TypeScript document parsed by the
/// TypeScript grammar (an HTML page whose URL ended in `.d.ts`) produced an
/// error-recovery tree deep enough to overflow the 2 MiB executor worker
/// stack and abort the daemon - twice, 55 s apart, on the same request.
fn walk_export_statements<'tree>(
    source: &str,
    root: &Node<'tree>,
    nav: impl Nav<'tree>,
    mut on_export: impl FnMut(&Node),
) -> HashSet<String> {
    let mut exported = HashSet::new();
    nav.walk(*root, |node, kind| {
        if kind == "export_statement" {
            on_export(&node);
            collect_names_from_export_statement(source, &node, &mut exported);
        }
        true
    });
    exported
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

test_support_visibility! {
/// Extract the first line of a node as its signature.
fn extract_signature(source: &str, node: &Node) -> String {
    let text = node_text(source, node);
    let first_line = text.lines().next().unwrap_or(text);
    // Trim trailing opening brace if present
    let trimmed = first_line.trim_end();
    let trimmed = trimmed.strip_suffix('{').unwrap_or(trimmed).trim_end();
    trimmed.to_string()
}
}

fn push_default_export_symbol<'tree>(
    symbols: &mut Vec<Symbol>,
    source: &str,
    lang: LangId,
    body_node: Node<'tree>,
    def_node: Node<'tree>,
    nav: impl Nav<'tree>,
) {
    let kind = if body_node.kind() == "class" {
        SymbolKind::Class
    } else {
        SymbolKind::Function
    };

    symbols.push(Symbol {
        name: "default".to_string(),
        kind,
        range: node_range_with_decorators_with(&def_node, source, lang, nav),
        signature: Some(extract_signature(source, &def_node)),
        scope_chain: vec![],
        exported: true,
        parent: None,
    });
}

fn node_contains_descendant_kind(node: &Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    loop {
        if cursor.node().kind() == kind {
            return true;
        }
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return false;
            }
        }
    }
}

fn push_ts_member_symbol<'tree>(
    symbols: &mut Vec<Symbol>,
    source: &str,
    name_node: Node<'tree>,
    def_node: Node<'tree>,
    parent_name: &str,
    kind: SymbolKind,
    nav: impl Nav<'tree>,
) {
    symbols.push(Symbol {
        name: node_text(source, &name_node).to_string(),
        kind,
        range: node_range_with_decorators_with(&def_node, source, LangId::TypeScript, nav),
        signature: Some(extract_signature(source, &def_node)),
        scope_chain: vec![parent_name.to_string()],
        exported: false,
        parent: Some(parent_name.to_string()),
    });
}

/// Extract symbols from TypeScript / TSX source.
fn extract_ts_symbols<'tree>(
    source: &str,
    root: &Node<'tree>,
    query: &Query,
    nav: impl Nav<'tree>,
) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::TypeScript;
    let capture_names = query.capture_names();

    let ExportIndex {
        ranges: export_ranges,
        names: exported_names,
    } = collect_exports(source, root, nav);

    let mut symbols = Vec::new();
    let mut variable_names = VariableNameIndex::default();
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
        let mut class_property_class_name_node = None;
        let mut class_property_name_node = None;
        let mut class_property_def_node = None;
        let mut interface_name_node = None;
        let mut interface_def_node = None;
        let mut interface_member_interface_name_node = None;
        let mut interface_member_name_node = None;
        let mut interface_member_def_node = None;
        let mut interface_property_interface_name_node = None;
        let mut interface_property_name_node = None;
        let mut interface_property_def_node = None;
        let mut object_property_object_name_node = None;
        let mut object_property_name_node = None;
        let mut object_property_value_node = None;
        let mut object_property_def_node = None;
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
                "class_property.class_name" => class_property_class_name_node = Some(cap.node),
                "class_property.name" => class_property_name_node = Some(cap.node),
                "class_property.def" => class_property_def_node = Some(cap.node),
                "interface.name" => interface_name_node = Some(cap.node),
                "interface.def" => interface_def_node = Some(cap.node),
                "interface_member.interface_name" => {
                    interface_member_interface_name_node = Some(cap.node)
                }
                "interface_member.name" => interface_member_name_node = Some(cap.node),
                "interface_member.def" => interface_member_def_node = Some(cap.node),
                "interface_property.interface_name" => {
                    interface_property_interface_name_node = Some(cap.node)
                }
                "interface_property.name" => interface_property_name_node = Some(cap.node),
                "interface_property.def" => interface_property_def_node = Some(cap.node),
                "object_property.object_name" => object_property_object_name_node = Some(cap.node),
                "object_property.name" => object_property_name_node = Some(cap.node),
                "object_property.value" => object_property_value_node = Some(cap.node),
                "object_property.def" => object_property_def_node = Some(cap.node),
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
                range: node_range_with_decorators_with(&def_node, source, lang, nav),
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
                range: node_range_with_decorators_with(&range_node, source, lang, nav),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        // Anonymous/default function or class expression
        if let (Some(body_node), Some(def_node)) = (default_body_node, default_def_node) {
            push_default_export_symbol(&mut symbols, source, lang, body_node, def_node, nav);
        }

        // Class declaration
        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators_with(&def_node, source, lang, nav),
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
                range: node_range_with_decorators_with(&def_node, source, lang, nav),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![class_name.clone()],
                exported: false, // methods inherit export from class
                parent: Some(class_name),
            });
        }

        // Callable class property
        if let (Some(class_name_node), Some(name_node), Some(def_node)) = (
            class_property_class_name_node,
            class_property_name_node,
            class_property_def_node,
        ) {
            let class_name = node_text(source, &class_name_node);
            push_ts_member_symbol(
                &mut symbols,
                source,
                name_node,
                def_node,
                class_name,
                SymbolKind::Method,
                nav,
            );
        }

        // Interface declaration
        if let (Some(name_node), Some(def_node)) = (interface_name_node, interface_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Interface,
                range: node_range_with_decorators_with(&def_node, source, lang, nav),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        // Interface method signature
        if let (Some(interface_name_node), Some(name_node), Some(def_node)) = (
            interface_member_interface_name_node,
            interface_member_name_node,
            interface_member_def_node,
        ) {
            let interface_name = node_text(source, &interface_name_node);
            push_ts_member_symbol(
                &mut symbols,
                source,
                name_node,
                def_node,
                interface_name,
                SymbolKind::Method,
                nav,
            );
        }

        // Function-valued interface property signature
        if let (Some(interface_name_node), Some(name_node), Some(def_node)) = (
            interface_property_interface_name_node,
            interface_property_name_node,
            interface_property_def_node,
        ) {
            if node_contains_descendant_kind(&def_node, "function_type") {
                let interface_name = node_text(source, &interface_name_node);
                push_ts_member_symbol(
                    &mut symbols,
                    source,
                    name_node,
                    def_node,
                    interface_name,
                    SymbolKind::Method,
                    nav,
                );
            }
        }

        // Callable object-literal property. The property key is the public callable name.
        if let (Some(object_name_node), Some(name_node), Some(def_node)) = (
            object_property_object_name_node,
            object_property_name_node,
            object_property_def_node,
        ) {
            let object_name = node_text(source, &object_name_node);
            let property_name = node_text(source, &name_node);
            push_ts_member_symbol(
                &mut symbols,
                source,
                name_node,
                def_node,
                object_name,
                SymbolKind::Method,
                nav,
            );

            // A named function expression also has a lexical name that callers may search for.
            if let Some(value_node) = object_property_value_node {
                if let Some(function_name_node) = value_node.child_by_field_name("name") {
                    if node_text(source, &function_name_node) != property_name {
                        push_ts_member_symbol(
                            &mut symbols,
                            source,
                            function_name_node,
                            value_node,
                            object_name,
                            SymbolKind::Function,
                            nav,
                        );
                    }
                }
            }
        }

        // Enum declaration
        if let (Some(name_node), Some(def_node)) = (enum_name_node, enum_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Enum,
                range: node_range_with_decorators_with(&def_node, source, lang, nav),
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
                range: node_range_with_decorators_with(&def_node, source, lang, nav),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        // Top-level const/let variable declaration (not arrow functions — those are handled above)
        if let (Some(name_node), Some(def_node)) = (var_name_node, var_def_node) {
            // Only include module-scope variables (parent is program/export_statement, not inside a function)
            let is_top_level = nav
                .parent(def_node)
                .map(|p| p.kind() == "program" || p.kind() == "export_statement")
                .unwrap_or(false);
            let range_node = var_decl_node.unwrap_or(def_node);
            let is_function_like = if range_node.kind() == "variable_declarator" {
                variable_declarator_has_function_value(&range_node)
            } else {
                lexical_declaration_has_function_value(&def_node)
            };
            let name = node_text(source, &name_node);
            if variable_names.should_capture(&symbols, name, is_top_level && !is_function_like) {
                symbols.push(Symbol {
                    name: name.to_string(),
                    kind: SymbolKind::Variable,
                    range: node_range_with_decorators_with(&range_node, source, lang, nav),
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

/// Name membership in the append-only query result, before final deduplication.
#[derive(Default)]
struct VariableNameIndex {
    names: HashSet<String>,
    indexed: usize,
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static VARIABLE_NAME_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub static LEGACY_VARIABLE_NAMES: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl VariableNameIndex {
    fn should_capture(&mut self, symbols: &[Symbol], name: &str, eligible: bool) -> bool {
        #[cfg(any(test, feature = "test-support"))]
        if LEGACY_VARIABLE_NAMES.with(|legacy| legacy.get()) {
            return Self::legacy_capture(symbols, name, eligible);
        }
        // Local and function-valued declarations cannot add a variable symbol.
        if !eligible {
            return false;
        }
        // Query captures append several kinds of symbols. Index only the new
        // suffix so name-only suppression preserves the original query order.
        for symbol in &symbols[self.indexed..] {
            #[cfg(any(test, feature = "test-support"))]
            VARIABLE_NAME_WORK.with(|work| work.set(work.get() + 1));
            self.names.insert(symbol.name.clone());
        }
        self.indexed = symbols.len();
        #[cfg(any(test, feature = "test-support"))]
        VARIABLE_NAME_WORK.with(|work| work.set(work.get() + 1));
        !self.names.contains(name)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn legacy_capture(symbols: &[Symbol], name: &str, eligible: bool) -> bool {
        let captured = symbols.iter().any(|symbol| {
            VARIABLE_NAME_WORK.with(|work| work.set(work.get() + 1));
            symbol.name == name
        });
        eligible && !captured
    }
}

/// Extract symbols from JavaScript source.
fn extract_js_symbols<'tree>(
    source: &str,
    root: &Node<'tree>,
    query: &Query,
    nav: impl Nav<'tree>,
) -> Result<Vec<Symbol>, AftError> {
    let lang = LangId::JavaScript;
    let capture_names = query.capture_names();

    let ExportIndex {
        ranges: export_ranges,
        names: exported_names,
    } = collect_exports(source, root, nav);

    let mut symbols = Vec::new();
    let mut variable_names = VariableNameIndex::default();
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
                range: node_range_with_decorators_with(&def_node, source, lang, nav),
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
                range: node_range_with_decorators_with(&range_node, source, lang, nav),
                signature: Some(extract_signature(source, &def_node)),
                scope_chain: vec![],
                exported: is_exported(&def_node, &export_ranges),
                parent: None,
            });
        }

        if let (Some(body_node), Some(def_node)) = (default_body_node, default_def_node) {
            push_default_export_symbol(&mut symbols, source, lang, body_node, def_node, nav);
        }

        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators_with(&def_node, source, lang, nav),
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
                range: node_range_with_decorators_with(&def_node, source, lang, nav),
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
            let is_top_level = nav
                .parent(def_node)
                .map(|p| p.kind() == "program" || p.kind() == "export_statement")
                .unwrap_or(false);
            let range_node = var_decl_node.unwrap_or(def_node);
            let is_function_like = if range_node.kind() == "variable_declarator" {
                variable_declarator_has_function_value(&range_node)
            } else {
                lexical_declaration_has_function_value(&def_node)
            };
            let name = node_text(source, &name_node);
            if variable_names.should_capture(&symbols, name, is_top_level && !is_function_like) {
                symbols.push(Symbol {
                    name: name.to_string(),
                    kind: SymbolKind::Variable,
                    range: node_range_with_decorators_with(&range_node, source, lang, nav),
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
fn py_scope_chain<'tree>(node: &Node<'tree>, source: &str, nav: impl Nav<'tree>) -> Vec<String> {
    let mut chain = Vec::new();
    let mut current = nav.parent(*node);
    while let Some(parent) = current {
        if nav.kind(parent) == "class_definition" {
            if let Some(name_node) = parent.child_by_field_name("name") {
                chain.push(node_text(source, &name_node).to_string());
            }
        }
        current = nav.parent(parent);
    }
    chain.reverse();
    chain
}

/// Extract symbols from Python source.
fn extract_py_symbols<'tree>(
    source: &str,
    root: &Node<'tree>,
    query: &Query,
    nav: impl Nav<'tree>,
) -> Result<Vec<Symbol>, AftError> {
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
            let scope = py_scope_chain(&def_node, source, nav);
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
                let mut parent = nav.parent(def_node);
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
                    parent = nav.parent(p);
                }
                sig_parts.push(extract_signature(source, &def_node));
                Some(sig_parts.join("\n"))
            } else {
                Some(extract_signature(source, &def_node))
            };

            symbols.push(Symbol {
                name,
                kind,
                range: node_range_with_decorators_with(&def_node, source, lang, nav),
                signature: sig,
                scope_chain: scope.clone(),
                exported: false, // Python has no export concept
                parent: scope.last().cloned(),
            });
        }

        // Class definition
        if let (Some(name_node), Some(def_node)) = (class_name_node, class_def_node) {
            let scope = py_scope_chain(&def_node, source, nav);

            // Build signature — include decorator if decorated
            let sig = if decorated_fn_lines.contains(&def_node.start_position().row) {
                let mut sig_parts = Vec::new();
                let mut parent = nav.parent(def_node);
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
                    parent = nav.parent(p);
                }
                sig_parts.push(extract_signature(source, &def_node));
                Some(sig_parts.join("\n"))
            } else {
                Some(extract_signature(source, &def_node))
            };

            symbols.push(Symbol {
                name: node_text(source, &name_node).to_string(),
                kind: SymbolKind::Class,
                range: node_range_with_decorators_with(&def_node, source, lang, nav),
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

test_support_visibility! {
fn rust_mod_scope_chain<'tree>(
    node: &Node<'tree>,
    source: &str,
    nav: impl Nav<'tree>,
) -> Vec<String> {
    let mut scopes = Vec::new();
    let mut current = nav.parent(*node);

    while let Some(parent) = current {
        if nav.kind(parent) == "mod_item" {
            if let Some(name_node) = parent.child_by_field_name("name") {
                scopes.push(node_text(source, &name_node).to_string());
            }
        }
        current = nav.parent(parent);
    }

    scopes.reverse();
    scopes
}
}

/// Rust function attributes that expose the function to code outside the Rust
/// call graph. This list is deliberately conservative: it covers known ABI,
/// wasm, constructor/destructor, and Tauri command entry points, but it does not
/// guess route macro shapes from web frameworks.
pub const RUST_ENTRY_POINT_ATTRIBUTES: &[&str] = &[
    "tauri::command",
    "wasm_bindgen",
    "no_mangle",
    "export_name",
    "ctor",
    "dtor",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustAttributeEntryPoint {
    pub name: String,
    pub scoped_name: String,
    pub attribute: &'static str,
}

/// Return Rust functions whose attributes make them external entry points.
pub fn rust_attribute_entry_points<'a>(
    source: &str,
    root: Node<'a>,
) -> Vec<RustAttributeEntryPoint> {
    rust_attribute_entry_points_with(source, root, Direct)
}

/// [`rust_attribute_entry_points`], walking and looking up with `nav`.
pub fn rust_attribute_entry_points_with<'tree>(
    source: &str,
    root: Node<'tree>,
    nav: impl Nav<'tree>,
) -> Vec<RustAttributeEntryPoint> {
    let bare_command_imported = rust_file_imports_tauri_command(source, root, nav);
    let mut function_nodes = Vec::new();
    nav.walk(root, |node, kind| {
        if kind == "function_item" {
            function_nodes.push(node);
        }
        true
    });

    let mut entry_points = BTreeMap::new();
    for function_node in function_nodes {
        let Some(attribute) = rust_entry_point_attribute_for_function(
            source,
            &function_node,
            bare_command_imported,
            nav,
        ) else {
            continue;
        };
        let Some((name, scoped_name)) = rust_function_identity(source, &function_node, nav) else {
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

fn rust_function_identity<'tree>(
    source: &str,
    function_node: &Node<'tree>,
    nav: impl Nav<'tree>,
) -> Option<(String, String)> {
    let name_node = function_node.child_by_field_name("name")?;
    let name = node_text(source, &name_node).to_string();
    let declaration_list_owner = rust_function_declaration_list_owner(function_node, nav);

    match declaration_list_owner.map(|owner| nav.kind(owner)) {
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
            let scope_chain = rust_mod_scope_chain(function_node, source, nav);
            let scoped_name = if scope_chain.is_empty() {
                name.clone()
            } else {
                format!("{}::{name}", scope_chain.join("::"))
            };
            Some((name, scoped_name))
        }
    }
}

test_support_visibility! {
fn rust_function_declaration_list_owner<'tree>(
    function_node: &Node<'tree>,
    nav: impl Nav<'tree>,
) -> Option<Node<'tree>> {
    nav.parent(*function_node)
        .filter(|parent| nav.kind(*parent) == "declaration_list")
        .and_then(|parent| nav.parent(parent))
}
}

test_support_visibility! {
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
}

fn rust_entry_point_attribute_for_function<'tree>(
    source: &str,
    function_node: &Node<'tree>,
    bare_command_imported: bool,
    nav: impl Nav<'tree>,
) -> Option<&'static str> {
    let mut current = *function_node;
    while let Some(prev) = nav.prev_sibling(current) {
        let kind = nav.kind(prev);
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

fn rust_file_imports_tauri_command<'tree>(
    source: &str,
    root: Node<'tree>,
    nav: impl Nav<'tree>,
) -> bool {
    // Any `use` that imports the attribute decides it, wherever it is.
    let mut imported = false;
    nav.walk(root, |node, kind| {
        imported |= kind == "use_declaration"
            && rust_use_declaration_imports_tauri_command(node_text(source, &node));
        !imported
    });
    imported
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

thread_local! {
    // Macro bodies are parsed with included ranges; keeping that parser apart
    // from the shared per-language parsers means a leftover range restriction
    // can never leak into an ordinary whole-file parse.
    static RUST_MACRO_BODY_PARSER: RefCell<Option<Parser>> = const { RefCell::new(None) };
}

/// Navigation includes trait members already known to the call graph without
/// changing the parser table persisted in content records. Both stored and
/// freshly parsed tables use the same ownership and duplicate checks.
pub fn complete_rust_trait_members(
    symbols: &mut Vec<Symbol>,
    members: impl IntoIterator<Item = Symbol>,
) {
    // Stored symbol ordinals and live tree walkers need not visit traits in
    // the same order; navigation always appends members in source order.
    let mut members = members.into_iter().collect::<Vec<_>>();
    members.sort_by_key(|member| (member.range.start_line, member.range.start_col));
    for mut member in members {
        if member.kind != SymbolKind::Method
            || !symbols.iter().any(|owner| {
                owner.kind == SymbolKind::Interface
                    && member.parent.as_deref() == Some(owner.name.as_str())
                    && (owner.range.start_line, owner.range.start_col)
                        <= (member.range.start_line, member.range.start_col)
                    && (member.range.end_line, member.range.end_col)
                        <= (owner.range.end_line, owner.range.end_col)
            })
        {
            continue;
        }
        // Like impl members, navigation names trait members as Trait::method,
        // even inside a module. Stored call-graph identities keep module scope.
        member.scope_chain = member.parent.iter().cloned().collect();
        if symbols.iter().any(|existing| {
            existing.kind == member.kind
                && existing.name == member.name
                && existing.range == member.range
                && existing.scope_chain == member.scope_chain
        }) {
            continue;
        }
        symbols.push(member);
    }
}

test_support_visibility! {
/// The type an impl block's methods belong to, as outlines name it: generic
/// arguments and path qualifiers are dropped, so `impl<T> Trait for a::Box<T>`
/// nests its methods under the `Box` struct instead of a name no symbol has.
fn rust_impl_parent_name(impl_target: &str) -> String {
    let base_type = impl_target.split('<').next().unwrap_or(impl_target).trim();
    base_type
        .rsplit("::")
        .next()
        .unwrap_or(base_type)
        .to_string()
}
}

test_support_visibility! {
/// Extract symbols from Rust source without compiling a tree-sitter query.
/// Handles: free functions, struct, enum, trait (as Interface), impl methods with scope chains.
///
/// tree-sitter parses the body of a `macro_rules!` arm and of an item-position
/// macro invocation as an opaque token tree, so items written there would be
/// invisible. Code bases that group whole runtimes of methods inside macro
/// bodies depend on seeing them, so each such body is parsed again on its own
/// and contributes its items when, and only when, it parses cleanly as Rust.
fn extract_rs_symbols<'tree>(
    source: &str,
    root: &Node<'tree>,
    nav: impl Nav<'tree>,
) -> Result<Vec<Symbol>, AftError> {
    let mut symbols = Vec::new();
    let mut macro_bodies = Vec::new();
    extract_rs_symbols_from_root(source, *root, &mut symbols, &mut macro_bodies, nav);

    // Bodies found inside a macro body are queued on the same worklist rather
    // than handled by recursion, so deeply nested macros cannot exhaust the
    // call stack.
    let mut found_macro_symbols = false;
    while let Some(body) = macro_bodies.pop() {
        let Some(tree) = parse_rust_macro_body(source, body) else {
            continue;
        };
        let before = symbols.len();
        extract_rs_symbols_from_root(
            source,
            tree.root_node(),
            &mut symbols,
            &mut macro_bodies,
            Direct,
        );
        found_macro_symbols |= symbols.len() > before;
    }

    if found_macro_symbols {
        // Macro bodies are visited after the rest of the file; restore source
        // order so outlines read top to bottom.
        symbols.sort_by_key(|symbol| (symbol.range.start_line, symbol.range.start_col));
    }
    dedup_symbols(&mut symbols);
    Ok(symbols)
}
}

/// The byte span strictly inside a token tree's delimiters, or `None` when the
/// tree holds nothing but whitespace.
fn rust_token_tree_interior(source: &str, token_tree: &Node) -> Option<tree_sitter::Range> {
    let open = token_tree.child(0)?;
    let last = u32::try_from(token_tree.child_count().checked_sub(1)?).ok()?;
    let close = token_tree.child(last)?;
    if open.id() == close.id() || !matches!(open.kind(), "{" | "(" | "[") {
        return None;
    }
    let start_byte = open.end_byte();
    let end_byte = close.start_byte();
    if source.get(start_byte..end_byte)?.trim().is_empty() {
        return None;
    }
    Some(tree_sitter::Range {
        start_byte,
        end_byte,
        start_point: open.end_position(),
        end_point: close.start_position(),
    })
}

/// Parse one macro body as Rust items. The parse is restricted to the body's
/// byte range of the original source, so it costs only the body's size and
/// every node position already refers to the original file; line ranges and
/// symbol text need no remapping. A body that does not parse cleanly (for
/// example a template using `$(...)*` repetitions) yields `None` and
/// contributes nothing.
fn parse_rust_macro_body(source: &str, body: tree_sitter::Range) -> Option<Tree> {
    RUST_MACRO_BODY_PARSER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let mut parser = Parser::new();
            parser.set_language(&grammar_for(LangId::Rust).ok()?).ok()?;
            *slot = Some(parser);
        }
        let parser = slot.as_mut()?;
        parser.set_included_ranges(&[body]).ok()?;
        let tree = parser.parse(source, None)?;
        if tree.root_node().has_error() {
            return None;
        }
        Some(tree)
    })
}

/// Walk one Rust syntax tree, appending its symbols and queueing the bodies of
/// item-position macros for a separate parse.
fn extract_rs_symbols_from_root<'tree>(
    source: &str,
    root: Node<'tree>,
    symbols: &mut Vec<Symbol>,
    macro_bodies: &mut Vec<tree_sitter::Range>,
    nav: impl Nav<'tree>,
) {
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

    // Inside a `macro_rules!` template an item may be named by a metavariable
    // such as `$name`. That is a placeholder filled in per invocation, not a
    // symbol anyone can look up, so such items are left out.
    let symbol_name = |node: &Node<'_>| -> Option<String> {
        let name_node = node.child_by_field_name("name")?;
        if name_node.kind() == "metavariable" {
            return None;
        }
        Some(node_text(source, &name_node).to_string())
    };

    let item_symbol = |node: Node<'tree>, kind: SymbolKind| -> Option<Symbol> {
        let name = symbol_name(&node)?;
        Some(Symbol {
            name,
            kind,
            range: node_range_with_decorators_with(&node, source, lang, nav),
            signature: Some(extract_signature(source, &node)),
            scope_chain: Vec::new(),
            exported: is_pub(&node),
            parent: None,
        })
    };

    // Every node in preorder.
    nav.walk(root, |node, kind| {
        match kind {
            "macro_definition" => {
                let mut rule_cursor = node.walk();
                for rule in node.children(&mut rule_cursor) {
                    if rule.kind() != "macro_rule" {
                        continue;
                    }
                    if let Some(body) = rule
                        .child_by_field_name("right")
                        .and_then(|right| rust_token_tree_interior(source, &right))
                    {
                        macro_bodies.push(body);
                    }
                }
            }
            // Only invocations in item position can expand to items. Skipping
            // the ones inside function bodies (`println!`, `vec!`, ...) keeps
            // the extra parsing proportional to item-level macros.
            "macro_invocation"
                if nav.parent(node).is_some_and(|parent| {
                    matches!(nav.kind(parent), "source_file" | "declaration_list")
                }) =>
            {
                let body = node
                    .child_by_field_name("macro")
                    .and_then(|name| name.next_sibling())
                    .and_then(|bang| bang.next_sibling())
                    .filter(|tree| tree.kind() == "token_tree")
                    .and_then(|tree| rust_token_tree_interior(source, &tree));
                if let Some(body) = body {
                    macro_bodies.push(body);
                }
            }
            "function_item" => {
                let declaration_list_owner = nav
                    .parent(node)
                    .filter(|parent| nav.kind(*parent) == "declaration_list")
                    .and_then(|parent| nav.parent(parent));
                let in_non_module_declaration_list =
                    declaration_list_owner.is_some_and(|owner| nav.kind(owner) != "mod_item");

                if !in_non_module_declaration_list {
                    if let Some(name) = symbol_name(&node) {
                        let scope_chain = rust_mod_scope_chain(&node, source, nav);
                        symbols.push(Symbol {
                            name,
                            kind: SymbolKind::Function,
                            range: node_range_with_decorators_with(&node, source, lang, nav),
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
                let parent_name = type_names
                    .last()
                    .map(|name| rust_impl_parent_name(name))
                    .unwrap_or_default();

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
                                        if let Some(name) = symbol_name(&method) {
                                            symbols.push(Symbol {
                                                name,
                                                kind: SymbolKind::Method,
                                                range: node_range_with_decorators_with(
                                                    &method, source, lang, nav,
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
        true
    });
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
            // A grouped declaration owns several specs; only an ungrouped spec
            // includes the `type` keyword in its replacement range.
            let def_node = def_node
                .parent()
                .filter(|parent| {
                    parent.kind() == "type_declaration" && {
                        let mut cursor = parent.walk();
                        let ungrouped = !parent
                            .children(&mut cursor)
                            .any(|child| child.kind() == "(");
                        ungrouped
                    }
                })
                .unwrap_or(def_node);
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

    // Only declarations directly in the source file initialize a package.
    // Local bindings must not become callgraph owners or outline entries.
    for declaration in root.named_children(&mut root.walk()) {
        if !matches!(declaration.kind(), "var_declaration" | "const_declaration") {
            continue;
        }
        let mut stack = vec![declaration];
        while let Some(spec) = stack.pop() {
            if matches!(spec.kind(), "var_spec" | "const_spec") {
                for name_node in spec.children_by_field_name("name", &mut spec.walk()) {
                    let name = node_text(source, &name_node).to_string();
                    if name == "_" {
                        continue;
                    }
                    symbols.push(Symbol {
                        exported: is_go_exported(&name),
                        name,
                        kind: SymbolKind::Variable,
                        range: node_range_with_decorators(&spec, source, lang),
                        signature: Some(extract_signature(source, &spec)),
                        scope_chain: vec![],
                        parent: None,
                    });
                }
            } else {
                stack.extend(spec.named_children(&mut spec.walk()));
            }
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

fn cpp_function_kind(source: &str, definition: &Node, lang: LangId) -> SymbolKind {
    let is_cuda_kernel = lang == LangId::Cuda
        && node_text(source, definition)
            .split(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .any(|token| token == "__global__");
    if is_cuda_kernel {
        SymbolKind::Kernel
    } else {
        SymbolKind::Function
    }
}

/// Extract symbols from C++ source.
fn extract_cpp_symbols(
    source: &str,
    root: &Node,
    query: &Query,
    lang: LangId,
) -> Result<Vec<Symbol>, AftError> {
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
                    kind: cpp_function_kind(source, &def_node, lang),
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
    let mut variable_names = VariableNameIndex::default();
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
            let name = node_text(source, &name_node);
            if variable_names.should_capture(&symbols, name, is_const && !is_container) {
                symbols.push(Symbol {
                    name: name.to_string(),
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

/// Find the first type_identifier node in a subtree, in source order. The
/// search keeps its place in a tree cursor rather than recursing, so a
/// deeply nested type cannot overflow the stack.
fn find_type_identifier_recursive(node: &Node, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    loop {
        if cursor.node().kind() == "type_identifier" {
            return Some(node_text(source, &cursor.node()).to_string());
        }
        if cursor.goto_first_child() {
            continue;
        }
        // Move past this subtree: to the next sibling, or up until an
        // ancestor has one. The cursor cannot climb above `node`.
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return None;
            }
        }
    }
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

/// Whether a node of `kind` occurs in `node`'s subtree (`node` included).
/// The search keeps its pending nodes on the heap rather than recursing, so
/// a deeply nested expression cannot overflow the stack.
fn r_node_contains_kind(node: &Node, kind: &str) -> bool {
    let mut pending = vec![*node];
    while let Some(node) = pending.pop() {
        if node.kind() == kind {
            return true;
        }
        pending.extend(node.children(&mut node.walk()));
    }
    false
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

/// The name a declarator declares: the first name found searching the
/// `declarator` field first, then the named children from the last one
/// back. The search keeps its pending nodes on the heap rather than
/// recursing, so deeply nested declarators cannot overflow the stack.
fn objc_declarator_name(source: &str, node: &Node) -> Option<String> {
    let mut pending = vec![*node];
    while let Some(node) = pending.pop() {
        if matches!(
            node.kind(),
            "identifier" | "field_identifier" | "type_identifier"
        ) {
            return Some(node_text(source, &node).to_string());
        }
        // Pushed in reverse of the search order: the first child, up to the
        // last, then the declarator, which is popped first. The declarator
        // is usually one of the children too; searching it a second time
        // could find nothing new, so it is pushed once.
        let declarator = node.child_by_field_name("declarator");
        for child_index in 0..node.child_count() {
            if let Some(child) = node.child(child_index as u32) {
                if child.is_named() && Some(child) != declarator {
                    pending.push(child);
                }
            }
        }
        pending.extend(declarator);
    }
    None
}

fn toml_key_text(source: &str, key: &Node) -> String {
    let raw = node_text(source, key).trim();
    if raw.len() >= 2
        && ((raw.starts_with('"') && raw.ends_with('"'))
            || (raw.starts_with('\'') && raw.ends_with('\'')))
    {
        raw[1..raw.len() - 1].to_string()
    } else {
        raw.to_string()
    }
}

fn toml_pair_symbol(source: &str, pair: &Node, section: Option<&str>) -> Option<Symbol> {
    let key = pair.child_by_field_name("key").or_else(|| {
        let mut cursor = pair.walk();
        let first = pair.named_children(&mut cursor).next();
        first
    })?;
    let name = toml_key_text(source, &key);
    if name.is_empty() {
        return None;
    }

    let scope_chain = section
        .map(|name| vec![name.to_string()])
        .unwrap_or_default();
    Some(Symbol {
        name,
        kind: SymbolKind::Variable,
        range: node_range_with_decorators(pair, source, LangId::Toml),
        signature: None,
        scope_chain,
        exported: false,
        parent: section.map(str::to_string),
    })
}

fn toml_table_name(source: &str, table: &Node) -> Option<String> {
    let first_line = node_text(source, table).lines().next()?.trim();
    let name = first_line
        .strip_prefix("[[")
        .and_then(|line| line.strip_suffix("]]"))
        .or_else(|| {
            first_line
                .strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
        })?
        .trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn toml_table_range(source: &str, table: &Node) -> Range {
    let start = table.start_position();
    let text = node_text(source, table).trim_end();
    let mut lines = text.lines();
    let line_count = lines.clone().count().max(1);
    let end_col = lines.next_back().map(str::len).unwrap_or_default();
    Range {
        start_line: start.row as u32,
        start_col: start.column as u32,
        end_line: start.row as u32 + line_count as u32 - 1,
        end_col: end_col as u32,
    }
}

fn extract_toml_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let mut symbols = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "pair" => {
                if let Some(symbol) = toml_pair_symbol(source, &child, None) {
                    symbols.push(symbol);
                }
            }
            "table" | "table_array_element" => {
                let Some(name) = toml_table_name(source, &child) else {
                    continue;
                };
                symbols.push(Symbol {
                    name: name.clone(),
                    kind: SymbolKind::Variable,
                    range: toml_table_range(source, &child),
                    signature: None,
                    scope_chain: Vec::new(),
                    exported: false,
                    parent: None,
                });

                let mut table_cursor = child.walk();
                for item in child.named_children(&mut table_cursor) {
                    if item.kind() == "pair" {
                        if let Some(symbol) = toml_pair_symbol(source, &item, Some(&name)) {
                            symbols.push(symbol);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(symbols)
}

// Node-kind and field names in this block (`binding_set`,
// `function_expression`, `formal`, `let_attrset_expression`, …) couple to the
// pinned tree-sitter-nix version in Cargo.lock; a grammar bump can reshape
// them, and the `nix_` tests in this module are the pin that catches it.
/// Depth of nested bindings the Nix outline expands beneath their parent.
/// A named constant so widening it later does not touch the rest of the module.
const NIX_OUTLINE_DEPTH: usize = 1;

/// Upper bound on the name bytes one file's Nix outline may keep: symbol
/// names plus the parent and scope-chain copies each child carries.
///
/// Every child of a wide binding copies the parent name into two fields
/// (`parent` and the one-element `scope_chain`), so a file with one long
/// parent name and many small children retains about 2 × name length ×
/// child count bytes — a factor that grows with `NIX_OUTLINE_DEPTH` as
/// children copy the whole scope chain — quadratically more than the file
/// itself, which cold indexing would pay for a file even under the
/// scanner's read limit. Four MiB per file — the read limit itself —
/// leaves every real file whole (a normal outline keeps a small multiple
/// of the file's own attr-path bytes) and caps the pathological one; past
/// the bound the walk stops emitting, so the outline keeps its leading,
/// exact portion.
const NIX_OUTLINE_NAME_BUDGET: usize = 4 << 20;

/// Read an `attrpath` node as the dotted name written in the source.
fn nix_attrpath_name(source: &str, attrpath: &Node) -> String {
    let mut cursor = attrpath.walk();
    let parts: Vec<String> = attrpath
        .children_by_field_name("attr", &mut cursor)
        .map(|attr| node_text(source, &attr).trim().to_string())
        .filter(|part| !part.is_empty())
        .collect();
    parts.join(".")
}

/// Unwrap parenthesized expressions until reaching the inner value.
fn nix_unwrap_parentheses(mut node: Node) -> Node {
    while node.kind() == "parenthesized_expression" {
        match node.child_by_field_name("expression") {
            Some(inner) => node = inner,
            None => break,
        }
    }
    node
}

/// The `binding_set` a value produces after unwrapping parentheses, a function
/// body, an application's final argument, or a `let … in` body. `None` when the
/// value is not an attribute set.
fn nix_value_binding_set(node: Node) -> Option<Node> {
    let mut current = nix_unwrap_parentheses(node);
    loop {
        match current.kind() {
            "function_expression" | "with_expression" | "assert_expression" => {
                current = current.child_by_field_name("body")?;
                current = nix_unwrap_parentheses(current);
            }
            "let_expression" => {
                current = current.child_by_field_name("body")?;
                current = nix_unwrap_parentheses(current);
            }
            // Module and package values are commonly wrapped in a call
            // (`config = lib.mkIf cond { … }`); the final argument is the
            // configuration attrset, the same rule the entry walk applies.
            "apply_expression" => {
                current = current.child_by_field_name("argument")?;
                current = nix_unwrap_parentheses(current);
            }
            "attrset_expression" | "rec_attrset_expression" | "let_attrset_expression" => {
                let mut cursor = current.walk();
                return current
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "binding_set");
            }
            _ => return None,
        }
    }
}

/// Collect byte ranges of comment nodes below `node`, skipping `body_id`. The
/// subtree branches, so the descent uses an explicit worklist rather than
/// recursion: a function whose formal defaults or non-body children nest deeply
/// would otherwise spend a stack frame per level and overflow a scan thread.
fn nix_comment_ranges(node: Node, body_id: Option<usize>, ranges: &mut Vec<(usize, usize)>) {
    let mut worklist = vec![node];
    while let Some(node) = worklist.pop() {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if Some(child.id()) == body_id {
                continue;
            }
            if child.kind() == "comment" {
                ranges.push((child.start_byte(), child.end_byte()));
            } else {
                worklist.push(child);
            }
        }
    }
}

/// `{ config, ... }:` or `x:` for a function expression, taken from the source
/// between the function's start and the end of its parameters.
fn nix_function_signature(source: &str, node: Node) -> Option<String> {
    let function = nix_unwrap_parentheses(node);
    if function.kind() != "function_expression" {
        return None;
    }
    // Take the header from the function start to the end of its parameters, so
    // a comment between the formals and the body stays out of the signature,
    // and splice comments that sit inside the formals out of the header.
    let body_id = function.child_by_field_name("body").map(|body| body.id());
    let mut end = function.start_byte();
    let mut cursor = function.walk();
    for child in function.named_children(&mut cursor) {
        if Some(child.id()) == body_id || child.kind() == "comment" {
            continue;
        }
        end = end.max(child.end_byte());
    }
    if end <= function.start_byte() {
        return None;
    }
    let start = function.start_byte();
    let mut comments = Vec::new();
    nix_comment_ranges(function, body_id, &mut comments);
    comments.sort_unstable();
    let mut header = String::new();
    let mut cursor_byte = start;
    for (comment_start, comment_end) in comments {
        if comment_start < cursor_byte || comment_end > end {
            continue;
        }
        header.push_str(source.get(cursor_byte..comment_start)?);
        cursor_byte = comment_end;
    }
    header.push_str(source.get(cursor_byte..end)?);
    // The outline renders a signature instead of the name, as it does for
    // `fn foo(…)`, so the header keeps the binding name and stays one line even
    // when the formals span several.
    let signature = header
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" ,", ",");
    // Display-only collapse of wrapped-formal commas; it also rewrites a
    // literal " ," inside a formal default's string. Cosmetic on a
    // display field; quote-aware spans are out of scope here.
    Some(format!("{signature}:"))
}

fn nix_emit_binding(
    source: &str,
    binding: Node,
    scope_chain: &[String],
    parent: Option<&str>,
    depth: usize,
    budget: &mut usize,
    symbols: &mut Vec<Symbol>,
) {
    // Inherit clauses are aliases, not definitions; only `binding` nodes count.
    if binding.kind() != "binding" {
        return;
    }
    let Some(attrpath) = binding.child_by_field_name("attrpath") else {
        return;
    };
    let name = nix_attrpath_name(source, &attrpath);
    if name.is_empty() {
        return;
    }

    // Charge this symbol's name bytes before allocating anything they would
    // be copied into (`parent` and `scope_chain` below repeat the enclosing
    // name for every child); a binding whose copies no longer fit is dropped
    // without descending, so the walk stops spending work with the budget.
    let name_bytes = name.len()
        + parent.map(str::len).unwrap_or(0)
        + scope_chain.iter().map(String::len).sum::<usize>();
    if name_bytes > *budget {
        return;
    }
    *budget -= name_bytes;

    let value = binding.child_by_field_name("expression");
    let is_function = value
        .map(nix_unwrap_parentheses)
        .is_some_and(|node| node.kind() == "function_expression");
    let (kind, signature) = if is_function {
        (
            SymbolKind::Function,
            value
                .and_then(|v| nix_function_signature(source, v))
                .map(|header| format!("{name} = {header}")),
        )
    } else {
        (SymbolKind::Variable, None)
    };

    symbols.push(Symbol {
        name: name.clone(),
        kind,
        range: node_range(&binding),
        signature,
        scope_chain: scope_chain.to_vec(),
        exported: false,
        parent: parent.map(str::to_string),
    });

    if depth >= NIX_OUTLINE_DEPTH {
        return;
    }
    let Some(set) = value.and_then(nix_value_binding_set) else {
        return;
    };
    let mut child_scope = scope_chain.to_vec();
    child_scope.push(name.clone());
    let mut cursor = set.walk();
    let children: Vec<Node> = set.named_children(&mut cursor).collect();
    for child in children {
        nix_emit_binding(
            source,
            child,
            &child_scope,
            Some(&name),
            depth + 1,
            budget,
            symbols,
        );
    }
}

/// Collect entry binding sets, looking through wrappers, plus the bindings of a
/// top-level `let` block, which belong to the file rather than the body. The
/// wrappers form a single chain (one descent per level), so the walk is a loop
/// rather than recursion: a file of nested parens, `let … in`, or applied
/// functions would otherwise spend one stack frame per level and overflow the
/// 2 MiB stack a store-scan thread runs on.
fn nix_collect_entry<'a>(node: Node<'a>, sets: &mut Vec<Node<'a>>, loose: &mut Vec<Node<'a>>) {
    let mut node = node;
    loop {
        match node.kind() {
            "source_code" => {
                let Some(expression) = node.child_by_field_name("expression") else {
                    return;
                };
                node = expression;
            }
            "function_expression" | "with_expression" | "assert_expression" => {
                let Some(body) = node.child_by_field_name("body") else {
                    return;
                };
                node = body;
            }
            "parenthesized_expression" => {
                let Some(inner) = node.child_by_field_name("expression") else {
                    return;
                };
                node = inner;
            }
            "let_expression" => {
                let mut cursor = node.walk();
                if let Some(set) = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "binding_set")
                {
                    let mut set_cursor = set.walk();
                    loose.extend(set.named_children(&mut set_cursor));
                }
                let Some(body) = node.child_by_field_name("body") else {
                    return;
                };
                node = body;
            }
            // Package and module files usually return an applied function whose
            // last argument is the attrset that carries the bindings. The argument
            // goes through entry discovery itself so a `let … in { … }` argument
            // also contributes its own file-level bindings.
            "apply_expression" => {
                let Some(argument) = node.child_by_field_name("argument") else {
                    return;
                };
                node = argument;
            }
            "attrset_expression" | "rec_attrset_expression" | "let_attrset_expression" => {
                let mut cursor = node.walk();
                let set = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "binding_set");
                if let Some(set) = set {
                    sets.push(set);
                }
                return;
            }
            _ => return,
        }
    }
}

fn extract_nix_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let mut symbols = Vec::new();
    let mut sets = Vec::new();
    let mut loose = Vec::new();
    nix_collect_entry(*root, &mut sets, &mut loose);

    let full_name = |binding: Node<'_>| {
        binding
            .child_by_field_name("attrpath")
            .map(|attrpath| nix_attrpath_name(source, &attrpath))
            .filter(|name| !name.is_empty())
    };

    // Nix attributes and `let` variables are separate namespaces, and two `let`
    // bindings can share a name too, so identical names are legal. The outline
    // keys nesting by parent name and cannot tell them apart: keep both
    // definitions and skip their nested expansion only when the full names are
    // identical.
    // Entry-set and `let` bindings are one flat file-level list: emit them in
    // source order, as every other language's outline does.
    let mut top_level: Vec<Node> = Vec::new();
    for set in sets {
        let mut cursor = set.walk();
        top_level.extend(set.named_children(&mut cursor));
    }
    top_level.extend(loose);
    top_level.sort_by_key(|binding| binding.start_byte());

    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for binding in &top_level {
        if let Some(name) = full_name(*binding) {
            *counts.entry(name).or_default() += 1;
        }
    }
    let collided: std::collections::HashSet<&str> = counts
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(name, _)| name.as_str())
        .collect();

    // One budget for the file: the walk charges every emitted symbol against
    // it, so a wide or huge file caps its retained names and the walk stops
    // early instead of copying an enclosing name once per child.
    let mut budget = NIX_OUTLINE_NAME_BUDGET;
    for binding in top_level {
        let depth = if full_name(binding).is_some_and(|name| collided.contains(name.as_str())) {
            NIX_OUTLINE_DEPTH
        } else {
            0
        };
        nix_emit_binding(source, binding, &[], None, depth, &mut budget, &mut symbols);
    }
    Ok(symbols)
}

/// Return the first non-comment value in a JSON document.
///
/// JSONC permits comments before the document value. Tree-sitter exposes those
/// comments as named children, so callers must not assume `named_child(0)` is the
/// object or array that contains the document's data.
pub fn json_document_value<'a>(root: &Node<'a>) -> Option<Node<'a>> {
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

/// Collect `key=value` pairs for the given keys (e.g. image, cpu, memory,
/// verbs) anywhere below `node`, in document order, to enrich embed_text for
/// semantic search. Sequence values are flattened to comma-joined scalars.
/// Capped to avoid runaway output. The walk keeps its pending nodes on the
/// heap rather than recursing, so a deeply nested document cannot overflow
/// the stack.
fn yaml_collect_values(
    source: &str,
    node: &Node,
    keys: &[&str],
    out: &mut Vec<String>,
    cap: usize,
) {
    // Children are pushed in reverse so they are popped in document order.
    let mut pending: Vec<Node> = node.named_children(&mut node.walk()).collect();
    pending.reverse();
    while let Some(child) = pending.pop() {
        if out.len() >= cap {
            return;
        }
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
        let start = pending.len();
        pending.extend(child.named_children(&mut child.walk()));
        pending[start..].reverse();
    }
}

/// Collect the `name:` field from every item of a `<parent_key>:` sequence of
/// mappings. The bare `name` key is too generic to match globally (it collides
/// with metadata/container names), so these are gathered by parent key and
/// emitted as `<label>=A,B,...`. Handles k8s `env: [{name,value}]` and Argo
/// Workflow `templates: [{name, ...}]`. Capped. The walk keeps its pending
/// nodes on the heap rather than recursing, so a deeply nested document
/// cannot overflow the stack.
fn yaml_collect_named_items(
    source: &str,
    node: &Node,
    parent_key: &str,
    label: &str,
    out: &mut Vec<String>,
    cap: usize,
) {
    // Children are pushed in reverse so they are popped in document order.
    let mut pending: Vec<Node> = node.named_children(&mut node.walk()).collect();
    pending.reverse();
    while let Some(child) = pending.pop() {
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
        let start = pending.len();
        pending.extend(child.named_children(&mut child.walk()));
        pending[start..].reverse();
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

pub fn luau_symbol(source: &str, declaration: crate::luau::Declaration<'_>) -> Symbol {
    let mut symbols = Vec::new();
    push_captured_symbol(
        &mut symbols,
        source,
        LangId::Luau,
        declaration.name,
        declaration.definition,
        declaration.kind,
        declaration.parent.into_iter().collect(),
        declaration.exported,
    );
    symbols.remove(0)
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

/// Adds a heading symbol for each Vue section block below `node`, in
/// document order, when `allow_sections` holds; sections count only
/// directly under the document. Only nodes outside sections are searched.
/// The walk keeps its pending nodes on the heap rather than recursing, so a
/// deeply nested template cannot overflow the stack.
fn collect_vue_sections(
    source: &str,
    node: &Node,
    symbols: &mut Vec<Symbol>,
    allow_sections: bool,
) {
    /// Queues `parent`'s children, each with whether a section there
    /// counts, so they are popped in document order.
    fn push_children<'tree>(
        pending: &mut Vec<(Node<'tree>, bool)>,
        parent: Node<'tree>,
        allow_sections: bool,
    ) {
        let start = pending.len();
        pending.extend(
            parent
                .children(&mut parent.walk())
                .map(|child| (child, allow_sections)),
        );
        pending[start..].reverse();
    }

    let mut pending = Vec::new();
    push_children(&mut pending, *node, allow_sections);
    while let Some((child, allow_sections)) = pending.pop() {
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
            push_children(
                &mut pending,
                child,
                allow_sections && child.kind() == "document",
            );
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

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    pub static HEADING_COLUMN_LINE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

test_support_visibility! {
fn source_line_end_cols(source: &str) -> Vec<u32> {
    source
        .lines()
        .map(|line| {
            #[cfg(any(test, feature = "test-support"))]
            HEADING_COLUMN_LINE_VISITS.with(|visits| visits.set(visits.get() + 1));
            line.len() as u32
        })
        .collect()
}
}

fn extract_html_symbols(source: &str, root: &Node) -> Result<Vec<Symbol>, AftError> {
    let mut headings = Vec::new();
    collect_html_headings(source, root, &mut headings);

    let end_cols = source_line_end_cols(source);
    let total_lines = end_cols.len() as u32;

    // Extend each heading's end_line to just before the next heading at the
    // same or shallower level (or EOF). This makes aft_zoom return the full
    // section content rather than just the heading element's single line.
    for i in 0..headings.len() {
        let level = headings[i].level;
        let section_end = headings[i + 1..]
            .iter()
            .find(|heading| heading.level <= level)
            .map(|heading| heading.symbol.range.start_line.saturating_sub(1))
            .unwrap_or_else(|| total_lines.saturating_sub(1))
            .max(headings[i].symbol.range.start_line);
        headings[i].symbol.range.end_line = section_end;
        if section_end != headings[i].symbol.range.start_line {
            headings[i].symbol.range.end_col =
                end_cols.get(section_end as usize).copied().unwrap_or(0);
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

/// Collect h1-h6 elements from the HTML tree under `node`, in document
/// order, nested headings included. The walk keeps its place in a tree
/// cursor rather than recursing, so a deeply nested document cannot
/// overflow the stack, which would abort the process.
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
        }
        // Every node's children are searched too: headings nest in
        // elements and in the document's other nodes alike.
        if cursor.goto_first_child() {
            continue;
        }
        // Move past this subtree: to the next sibling, or up until an
        // ancestor has one. The cursor cannot climb above `node`.
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() || cursor.node().id() == node.id() {
                return;
            }
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

/// Extract text content from an HTML element, stripping tags. Text inside
/// nested elements (e.g., <strong>, <em>, <a>) is included, in document
/// order; the walk uses a tree cursor rather than recursion, so deep
/// nesting cannot overflow the stack.
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
            "element" if cursor.goto_first_child() => continue,
            _ => {}
        }
        // Move past this subtree: to the next sibling, or up until an
        // ancestor has one. The cursor cannot climb above `node`.
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() || cursor.node().id() == node.id() {
                return text;
            }
        }
    }
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

/// Collects the ATX and setext headings in `node`'s subtree (`node`
/// included), in document order, without descending into a heading. The
/// walk keeps its place in a tree cursor rather than recursing, so deeply
/// nested blocks cannot overflow the stack.
fn collect_headings(source: &str, node: &Node, headings: &mut Vec<RawHeading>) {
    let mut cursor = node.walk();
    loop {
        let current = cursor.node();
        let kind = current.kind();
        let heading = if kind == "atx_heading" {
            Some(parse_atx_heading(source, &current))
        } else if kind == "setext_heading" {
            Some(parse_setext_heading(source, &current))
        } else {
            None
        };
        match heading {
            Some(parsed) => {
                if let Some((name, level)) = parsed {
                    headings.push(RawHeading {
                        name,
                        level,
                        range: node_range(&current),
                    });
                }
            }
            None if cursor.goto_first_child() => continue,
            None => {}
        }
        // Move past this subtree: to the next sibling, or up until an
        // ancestor has one. The cursor cannot climb above `node`.
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
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
    let end_cols = source_line_end_cols(source);
    let total_lines = end_cols.len() as u32;

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
            end_cols.get(end_line as usize).copied().unwrap_or(0)
        } else {
            0
        };

        for j in (i + 1)..raw_headings.len() {
            if raw_headings[j].level <= h.level {
                // The section ends before this heading starts
                let next_start_line = raw_headings[j].range.start_line;
                end_line = next_start_line.saturating_sub(1).max(start_line);
                end_col = end_cols.get(end_line as usize).copied().unwrap_or(0);
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

test_support_visibility! {
/// Remove duplicate symbols based on (name, kind, start_line).
/// Class declarations can match both "class" and "method" patterns,
/// producing duplicates.
fn dedup_symbols(symbols: &mut Vec<Symbol>) {
    // Keys borrow from the symbols instead of cloning the name and formatting
    // the kind for every symbol.
    let keep: Vec<bool> = {
        let mut seen = std::collections::HashSet::with_capacity(symbols.len());
        symbols
            .iter()
            .map(|s| seen.insert((s.name.as_str(), &s.kind, s.range.start_line)))
            .collect()
    };
    let mut keep = keep.into_iter();
    symbols.retain(|_| keep.next().unwrap_or(true));
}
}

/// Provider that uses tree-sitter for real symbol extraction.
/// Implements the `LanguageProvider` trait from `language.rs`.
pub struct TreeSitterProvider {
    symbol_cache: SharedSymbolCache,
}

/// Share the production provider's immutable table, while retaining the owned
/// trait API for alternate providers and callers that need to modify symbols.
pub fn list_symbols_shared(
    provider: &dyn crate::language::LanguageProvider,
    file: &Path,
    dynamic_languages: bool,
) -> Result<Arc<Vec<Symbol>>, AftError> {
    if let Some(provider) = provider.as_any().downcast_ref::<TreeSitterProvider>() {
        FileParser::with_symbol_cache(provider.symbol_cache())
            .with_dynamic_languages(dynamic_languages)
            .extract_symbols_shared(file)
    } else {
        provider.list_symbols(file).map(Arc::new)
    }
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

        let symbols = parser.extract_symbols_shared(file)?;
        let local_matches = symbol_matches_in_file(file, &symbols, name);
        if !local_matches.is_empty() {
            return Ok(local_matches);
        }

        // Only JS-family modules have default exports and re-export clauses.
        // Other languages cannot resolve a missing local name by reparsing.
        if !matches!(
            detect_language(file),
            Some(LangId::TypeScript | LangId::Tsx | LangId::JavaScript)
        ) {
            return Ok(Vec::new());
        }

        // Both the default-export lookup and the re-export scan need the
        // file's source and tree; read and hash it once for both.
        let parsed = self.read_parsed_file(parser, file)?;
        if name == "default" {
            let default_matches = self.resolve_local_default_export(&parsed, file, &symbols)?;
            if !default_matches.is_empty() {
                return Ok(default_matches);
            }
        }

        let reexport_targets = self.collect_reexport_targets(&parsed, file, name)?;
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
        (source, tree, lang): &(String, Tree, LangId),
        file: &Path,
        requested_name: &str,
    ) -> Result<Vec<ReExportTarget>, AftError> {
        let (source, lang) = (source.as_str(), *lang);
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
        (source, tree, lang): &(String, Tree, LangId),
        file: &Path,
        symbols: &[Symbol],
    ) -> Result<Vec<SymbolMatch>, AftError> {
        let (source, lang) = (source.as_str(), *lang);
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
        #[cfg(any(test, feature = "test-support"))]
        work_counters::PARSED_FILE_READS.with(|count| count.set(count.get() + 1));
        let current_mtime = crate::bounded_io::metadata(file)
            .and_then(|m| m.modified())
            .map_err(|e| AftError::FileNotFound {
                path: format!("{}: {}", file.display(), e),
            })?;
        let source =
            crate::bounded_io::read_to_string(file).map_err(|e| AftError::FileNotFound {
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
#[cfg(any(test, feature = "test-support"))]
pub mod work_counters {
    use std::cell::Cell;

    thread_local! {
        /// Tree-sitter parses of whole files done through `FileParser`.
        pub static TREE_PARSES: Cell<usize> = const { Cell::new(0) };
        /// Parser allocations with a grammar load.
        pub static GRAMMAR_LOADS: Cell<usize> = const { Cell::new(0) };
        /// Source reads the language provider does to inspect a file's tree.
        pub static PARSED_FILE_READS: Cell<usize> = const { Cell::new(0) };
        /// Reads performed by FileParser to construct a tree, not by a provider.
        pub static TREE_SOURCE_READS: Cell<usize> = const { Cell::new(0) };
        /// Copies of a cached symbol list handed out by `FileParser`.
        pub static CACHED_SYMBOL_COPIES: Cell<usize> = const { Cell::new(0) };
        pub static FILE_PARSE_READS: Cell<usize> = const { Cell::new(0) };
        pub static SOURCE_READS: Cell<usize> = const { Cell::new(0) };
        pub static OWNED_SYMBOL_COPIES: Cell<usize> = const { Cell::new(0) };
        pub static RUST_MODULE_SCANS: Cell<usize> = const { Cell::new(0) };
        /// Tree-sitter parses through this thread's retained parsers, by any
        /// caller (FileParser, semantic chunking, symbol extraction).
        pub static CACHED_PARSER_PARSES: Cell<usize> = const { Cell::new(0) };
    }

    pub fn cached_parser_parses() -> usize {
        CACHED_PARSER_PARSES.with(Cell::get)
    }

    pub fn tree_parses() -> usize {
        TREE_PARSES.with(Cell::get)
    }

    pub fn grammar_loads() -> usize {
        GRAMMAR_LOADS.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nix_symbols(source: &str) -> Vec<Symbol> {
        let mut parser = Parser::new();
        parser
            .set_language(&grammar_for(LangId::Nix).unwrap())
            .expect("nix grammar loads");
        let tree = parser.parse(source, None).expect("parse nix source");
        assert!(
            !tree.root_node().has_error(),
            "fixture must parse without errors: {source}"
        );
        extract_symbols_from_tree(source, &tree, LangId::Nix).expect("extract nix symbols")
    }

    fn nix_names(source: &str) -> Vec<String> {
        nix_symbols(source)
            .into_iter()
            .map(|symbol| symbol.name)
            .collect()
    }

    #[test]
    fn nix_detection_and_clean_parse() {
        assert_eq!(detect_language(Path::new("flake.nix")), Some(LangId::Nix));
        let symbols = nix_symbols("{\n  description = \"demo\";\n}\n");
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "description");
    }

    #[test]
    fn nix_entry_discovery_covers_every_wrapper_and_container() {
        let cases: &[(&str, &str)] = &[
            ("function body", "{ lib, ... }: {\n  a = 1;\n}\n"),
            ("let", "let\n  version = \"1\";\nin\n{\n  a = 1;\n}\n"),
            ("with", "with pkgs;\n{\n  a = 1;\n}\n"),
            ("assert", "assert true;\n{\n  a = 1;\n}\n"),
            ("parenthesized", "({\n  a = 1;\n})\n"),
            ("attrset", "{\n  a = 1;\n}\n"),
            ("rec attrset", "rec {\n  a = 1;\n}\n"),
            ("let attrset", "let {\n  a = 1;\n}\n"),
        ];
        for (label, source) in cases {
            assert!(
                nix_names(source).contains(&"a".to_string()),
                "{label} should outline `a`"
            );
        }

        let let_names = nix_names("let\n  version = \"1\";\nin\n{\n  pname = \"demo\";\n}\n");
        assert!(let_names.contains(&"version".to_string()));
        assert!(let_names.contains(&"pname".to_string()));
    }

    #[test]
    fn nix_module_outline_uses_dotted_attrpaths() {
        let source = "{ config, lib, pkgs, ... }:\n{\n  options.services.demo.enable = lib.mkEnableOption \"demo\";\n  config.services.demo = { enable = true; port = 80; nested = { deep = 1; }; };\n}\n";
        let symbols = nix_symbols(source);
        let names: Vec<&str> = symbols.iter().map(|symbol| symbol.name.as_str()).collect();
        assert!(names.contains(&"options.services.demo.enable"));
        assert!(names.contains(&"config.services.demo"));
        assert!(names.contains(&"enable"));
        assert!(names.contains(&"port"));
        assert!(!names.contains(&"deep"), "depth stops after one level");

        let enable = symbols
            .iter()
            .find(|symbol| symbol.name == "enable")
            .unwrap();
        assert_eq!(enable.scope_chain, vec!["config.services.demo"]);
        assert_eq!(enable.parent.as_deref(), Some("config.services.demo"));
        assert_eq!(
            symbol_query_matches(enable, "config.services.demo.enable"),
            true
        );
        assert_eq!(
            symbol_query_matches(
                &symbols
                    .iter()
                    .find(|symbol| symbol.name == "config.services.demo")
                    .unwrap()
                    .clone(),
                "config.services.demo"
            ),
            true
        );
    }

    #[test]
    fn nix_applied_attrset_arguments_are_outlined() {
        let derivation = "{\n  lib,\n  stdenv,\n}:\nstdenv.mkDerivation {\n  pname = \"demo\";\n  version = \"1.0\";\n  src = ./.;\n}\n";
        let names = nix_names(derivation);
        assert!(names.contains(&"pname".to_string()), "{names:?}");
        assert!(names.contains(&"version".to_string()), "{names:?}");

        assert!(
            nix_names("stdenv.mkDerivation (finalAttrs: { pname = \"x\"; })\n")
                .contains(&"pname".to_string())
        );

        let conditional =
            nix_names("{ lib, ... }: lib.mkIf true { services.demo.enable = true; }\n");
        assert!(
            conditional.contains(&"services.demo.enable".to_string()),
            "{conditional:?}"
        );

        let wrapped = nix_names("with pkgs; stdenv.mkDerivation { pname = \"x\"; }\n");
        assert!(wrapped.contains(&"pname".to_string()), "{wrapped:?}");

        // Deliberate limits: non-attrset shapes and non-final arguments.
        for source in [
            "lib.mkMerge [ { a = 1; } { b = 2; } ]\n",
            "if true then { a = 1; } else { b = 2; }\n",
            "{ a = 1; } // { b = 2; }\n",
            "f { config = 1; } other\n",
        ] {
            assert!(
                nix_names(source).is_empty(),
                "expected no outline for {source:?}"
            );
        }
    }

    #[test]
    fn nix_nested_application_values_expand_one_level() {
        let names = nix_names("{\n  config = lib.mkIf true { services.demo.enable = true; };\n}\n");
        assert!(
            names.contains(&"services.demo.enable".to_string()),
            "an application-wrapped value must nest one level: {names:?}"
        );

        // The depth stop still applies: the nested binding's own child is not
        // expanded even though its value is an attrset.
        let deep = nix_names("{\n  config = f { a = { b = 1; }; };\n}\n");
        assert!(deep.contains(&"a".to_string()), "{deep:?}");
        assert!(!deep.contains(&"b".to_string()), "{deep:?}");

        // Only an attribute-set argument is followed; lists and `//` splits
        // nested in a binding still contribute nothing.
        let shapes = nix_names(
            "{\n  config = lib.mkMerge [ { a = 1; } { b = 2; } ];\n  other = { x = 1; } // { y = 2; };\n}\n",
        );
        assert_eq!(shapes, vec!["config".to_string(), "other".to_string()]);
    }

    #[test]
    fn nix_applied_let_argument_keeps_file_level_bindings() {
        let names = nix_names("stdenv.mkDerivation (let v = \"1\"; in { version = v; })\n");
        assert!(names.contains(&"v".to_string()), "{names:?}");
        assert!(names.contains(&"version".to_string()), "{names:?}");
    }

    #[test]
    fn nix_wide_binding_file_stays_inside_the_outline_name_budget() {
        // One long parent name over many small children: without the budget
        // each child copies the parent name twice (`parent` plus its
        // one-element `scope_chain`), so retained name bytes grow with the
        // square of the file and a small source becomes quadratic work.
        let parent = "p".repeat(3000);
        let children = 2000;
        let mut source = format!("{{ {parent} = {{\n");
        for child in 0..children {
            source.push_str(&format!("  c{child} = 1;\n"));
        }
        source.push_str("}; }\n");
        assert!(
            source.len() < 64 << 10,
            "the input itself must stay small: {} bytes",
            source.len()
        );

        let symbols = nix_symbols(&source);
        let kept: usize = symbols
            .iter()
            .map(|symbol| {
                symbol.name.len()
                    + symbol.parent.as_deref().map_or(0, str::len)
                    + symbol.scope_chain.iter().map(String::len).sum::<usize>()
            })
            .sum();
        assert!(
            kept <= NIX_OUTLINE_NAME_BUDGET,
            "the outline kept {kept} name bytes from a {}-byte file, over the bound",
            source.len()
        );

        // The stop truncates the tail, not the head: the first children are
        // emitted with exact names and parents, and the stop actually fired.
        assert!(symbols.len() > 1, "the outline emitted nothing");
        assert!(
            symbols.len() < children + 1,
            "the budget never fired: {} symbols for {children} children",
            symbols.len()
        );
        let first = symbols
            .iter()
            .find(|symbol| symbol.name == "c0")
            .expect("the first child is emitted");
        assert_eq!(first.parent.as_deref(), Some(parent.as_str()));
        assert_eq!(first.scope_chain, vec![parent.clone()]);
    }

    #[test]
    fn nix_collisions_use_full_names_across_every_top_level_binding() {
        let dotted = nix_symbols("let foo.bar = { x = 1; }; in { foo.baz = { y = 2; }; }\n");
        let names: Vec<&str> = dotted.iter().map(|symbol| symbol.name.as_str()).collect();
        assert!(names.contains(&"x"), "let binding expands: {names:?}");
        assert!(names.contains(&"y"), "entry binding expands: {names:?}");

        let same_name = nix_symbols("let x = { a = 1; }; in let x = { b = 2; }; in { y = 1; }\n");
        let lines: Vec<u32> = same_name
            .iter()
            .filter(|symbol| symbol.name == "x")
            .map(|symbol| symbol.range.start_line)
            .collect();
        assert_eq!(lines.len(), 2, "{same_name:?}");
        assert!(
            !same_name
                .iter()
                .any(|symbol| matches!(symbol.name.as_str(), "a" | "b")),
            "identical names expand neither: {same_name:?}"
        );
    }

    #[test]
    fn nix_function_bindings_carry_signatures() {
        let source = "{\n  config = { lib, pkgs, ... }: { a = 1; };\n  split =\n    { lib\n    , pkgs\n    }: { };\n  mkDefault = value: { b = value; };\n  run = (x: x);\n  enable = true;\n}\n";
        let symbols = nix_symbols(source);
        let find = |name: &str| symbols.iter().find(|symbol| symbol.name == name).unwrap();

        assert_eq!(find("config").kind, SymbolKind::Function);
        assert_eq!(
            find("config").signature.as_deref(),
            Some("config = { lib, pkgs, ... }:")
        );
        assert_eq!(
            find("split").signature.as_deref(),
            Some("split = { lib, pkgs }:"),
            "multiline formals collapse to one line"
        );

        let commented = nix_symbols("{\n  foo = { lib\n  # note\n  , pkgs }: { a = 1; };\n}\n");
        let foo = commented
            .iter()
            .find(|symbol| symbol.name == "foo")
            .unwrap();
        assert_eq!(
            foo.signature.as_deref(),
            Some("foo = { lib, pkgs }:"),
            "comments inside the formals are stripped"
        );
        assert!(!foo.signature.as_deref().unwrap().contains('#'));
        assert_eq!(find("mkDefault").kind, SymbolKind::Function);
        assert_eq!(
            find("mkDefault").signature.as_deref(),
            Some("mkDefault = value:")
        );
        assert_eq!(find("run").kind, SymbolKind::Function);
        assert_eq!(find("run").signature.as_deref(), Some("run = x:"));
        assert_eq!(find("enable").kind, SymbolKind::Variable);
        assert_eq!(find("enable").signature, None);
    }

    #[test]
    fn nix_flake_outline_nests_outputs_one_level() {
        let source = "{\n  outputs = inputs@{ self, nixpkgs }: {\n    devShells.x86_64-linux.default = nixpkgs.legacyPackages.x86_64-linux.mkShell { };\n  };\n}\n";
        let symbols = nix_symbols(source);
        let outputs = symbols
            .iter()
            .find(|symbol| symbol.name == "outputs")
            .unwrap();
        assert_eq!(outputs.kind, SymbolKind::Function);
        assert!(
            outputs
                .signature
                .as_deref()
                .unwrap()
                .starts_with("outputs = inputs@"),
            "{:?}",
            outputs.signature
        );

        let dev_shell = symbols
            .iter()
            .find(|symbol| symbol.name == "devShells.x86_64-linux.default")
            .expect("nested flake output");
        assert_eq!(dev_shell.scope_chain, vec!["outputs"]);
        assert_eq!(dev_shell.parent.as_deref(), Some("outputs"));
        assert!(symbol_query_matches(
            dev_shell,
            "outputs.devShells.x86_64-linux.default"
        ));
    }

    #[test]
    fn nix_signature_excludes_comments_and_shadowed_names_keep_entry_children() {
        let commented = "{\n  foo = { lib }: # docs\n    { a = 1; };\n}\n";
        let symbols = nix_symbols(commented);
        let foo = symbols.iter().find(|symbol| symbol.name == "foo").unwrap();
        assert_eq!(foo.signature.as_deref(), Some("foo = { lib }:"));

        // Attributes and `let` variables are separate namespaces: both rows
        // exist, and neither expands nested bindings while the parent name is
        // ambiguous.
        let shadowed =
            nix_symbols("let\n  enable = { x = 1; };\nin\n{\n  enable = { y = 2; };\n}\n");
        let names: Vec<&str> = shadowed.iter().map(|symbol| symbol.name.as_str()).collect();
        assert_eq!(names, vec!["enable", "enable"]);
        let lines: Vec<u32> = shadowed
            .iter()
            .map(|symbol| symbol.range.start_line)
            .collect();
        assert_eq!(lines, vec![1, 4], "outline follows source order");
        assert_eq!(
            shadowed
                .iter()
                .filter(|symbol| symbol.parent.is_some())
                .count(),
            0,
            "ambiguous parents do not expand"
        );
    }

    #[test]
    fn nix_inherit_clauses_are_not_outlined() {
        let names = nix_names("{\n  inherit (lib) mkIf;\n  inherit pkgs;\n}\n");
        assert!(!names.contains(&"mkIf".to_string()));
        assert!(!names.contains(&"pkgs".to_string()));
        assert!(names.is_empty());
    }

    #[test]
    fn nix_indented_string_semicolons_neither_trigger_nor_suppress_the_hint() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        // The only top-level `;` shape here sits inside an indented string;
        // the scanner must not read it as a second binding.
        assert!(
            detect_pattern_hint("$A = ''a; b'';", &lang).is_none(),
            "a semicolon inside an indented string must not earn the hint"
        );
        // Two real bindings still earn it even when each value is an
        // indented string.
        let hint = detect_pattern_hint("$A = ''x''; $B = ''y'';", &lang)
            .expect("two real bindings earn the nix hint");
        assert!(
            hint.contains("nix pattern must contain one binding or one inherit clause"),
            "hint missing the exact wording: {hint}"
        );
    }

    #[test]
    fn nix_two_binding_pattern_error_carries_hint() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        let pattern = "$A = true; $B = false;";
        assert!(
            lang.compile_pattern(pattern).is_err(),
            "an attrset body has no single node to bind"
        );
        let hint = detect_pattern_hint(pattern, &lang)
            .expect("a multi-binding pattern earns the nix hint");
        assert!(
            hint.contains("nix pattern must contain one binding or one inherit clause"),
            "hint missing the exact wording: {hint}"
        );
    }

    #[test]
    fn nix_binding_plus_inherit_pattern_error_carries_hint() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        let pattern = "$A = true; inherit $B;";
        assert!(
            lang.compile_pattern(pattern).is_err(),
            "a binding plus an inherit clause is two members"
        );
        let hint = detect_pattern_hint(pattern, &lang)
            .expect("a binding-plus-inherit pattern earns the nix hint");
        assert!(
            hint.contains("nix pattern must contain one binding or one inherit clause"),
            "hint missing the exact wording: {hint}"
        );
    }

    #[test]
    fn nix_dynamic_attrpath_names_stay_as_written() {
        let syms = nix_symbols("{ \"${x}\" = 1; a.\"${b}\".c = 2; plain = 3; }\n");
        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["\"${x}\"", "a.\"${b}\".c", "plain"],
            "interpolated and dynamic path segments keep the source-written name"
        );
    }

    #[test]
    fn nix_comment_or_quoted_member_after_semicolon_still_carries_hint() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        for pattern in ["$A = true; # c\n $B = false;", "$A = true; \"quoted\" = 2;"] {
            let hint = detect_pattern_hint(pattern, &lang)
                .expect("a member hidden by a comment or quote still earns the hint");
            assert!(
                hint.contains("nix pattern must contain one binding or one inherit clause"),
                "hint missing the exact wording: {hint}"
            );
        }
        assert!(
            detect_pattern_hint("$A = true; # trailing comment only\n", &lang).is_none(),
            "a single binding with a trailing comment earns no hint"
        );
        assert!(
            detect_pattern_hint("$A = { b = 1; }; } ; $B = 2;", &lang).is_some(),
            "a stray closer must not mask the second member"
        );
    }

    #[test]
    fn nix_indented_string_escapes_keep_the_hint_scanner_in_range() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        assert!(
            detect_pattern_hint("$A = ''x''${y};'';", &lang).is_none(),
            "a `;` inside an indented string (past a double-quote-dollar escape) is not a separator"
        );
        assert!(
            detect_pattern_hint("$A = ''x'';''; $B = 2;", &lang).is_some(),
            "the real separator after the closed indented string still earns the hint"
        );
        assert!(
            detect_pattern_hint("$A = ''''; $B = 1;", &lang).is_some(),
            "'''' is an empty indented string; the ; after it is top level"
        );
    }

    #[test]
    fn nix_quotes_nested_in_interpolation_keep_the_hint_scanner_in_range() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        // Interpolation bodies are full expressions and may nest strings; a
        // `;` inside a nested string must not read as a member terminator.
        for pattern in [
            "$A = \"${ f \"x;y\" }\";",
            "$A = ''${ f \"x;y\" }'';",
            "$A = \"${ \"\" } x; y\";",
            "$A = ''hi ${ \"don''t; do\" } x'';",
            "$A = \"${ /* \" */ toString 1 }\"; # note",
            "$A = ''${ /* ; */ f }'';",
        ] {
            assert!(
                detect_pattern_hint(pattern, &lang).is_none(),
                "nested string contents must not trigger the multi-binding hint: {pattern}"
            );
        }
    }

    #[test]
    fn nix_line_comment_hides_semicolons_from_the_hint_scanner() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        // One binding with a trailing comment: a `;` inside the comment must
        // not read as a member terminator.
        assert!(
            detect_pattern_hint("a = 1; # TODO ; revisit later", &lang).is_none(),
            "a `;` inside a line comment is not a member terminator"
        );
    }

    #[test]
    fn nix_block_comment_hides_semicolons_from_the_hint_scanner() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        // A single-binding pattern whose block comment contains `; x = 2;`
        // must not read as multiple bindings.
        assert!(
            detect_pattern_hint("$A = /* ; x = 2; */ true;", &lang).is_none(),
            "a `;` inside a block comment is not a member terminator"
        );
        // Positive control: a real second member behind a block comment is
        // still two bindings and still gets the hint.
        assert!(
            detect_pattern_hint("$A = 1; /* why */ $B = 2;", &lang).is_some(),
            "a member hidden behind a block comment is still a second binding"
        );
    }

    #[test]
    fn nix_let_in_terminator_after_semicolon_carries_no_hint() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        for pattern in ["let a = 1; in a", "let a = 1; in b", "let a = 1; in $B"] {
            assert!(
                detect_pattern_hint(pattern, &lang).is_none(),
                "`{pattern}` is one let-expression member, not two bindings"
            );
        }
    }

    #[test]
    fn nix_apostrophe_in_identifier_does_not_open_a_string() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        // Nix identifiers may contain apostrophes: `foo''` is one
        // identifier, not an indented-string opener. A `;` after it is a
        // real member terminator, so the multi-binding hint must fire.
        let hint = detect_pattern_hint("foo'' = true; bar = 1;", &lang)
            .expect("apostrophe pair must not swallow the member terminator");
        assert!(hint.contains("one binding or one inherit"), "{hint}");
    }

    #[test]
    fn nix_single_binding_pattern_compiles_without_hint() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        let pattern = "$NAME = true;";
        assert!(
            lang.compile_pattern(pattern).is_ok(),
            "a single binding compiles via the contextual fallback"
        );
        assert!(
            detect_pattern_hint(pattern, &lang).is_none(),
            "a valid single binding earns no hint"
        );
    }

    #[test]
    fn nix_composed_pattern_error_names_nix() {
        use crate::ast_grep_hints::detect_pattern_hint;
        use crate::ast_grep_lang::AstGrepLang;
        let lang = AstGrepLang::Nix;
        let pattern = "$A = true; $B = false;";
        let err = lang
            .compile_pattern(pattern)
            .expect_err("two bindings cannot compile");
        let hint = detect_pattern_hint(pattern, &lang).expect("the hint fires");
        // Mirrors how the ast-search/replace commands compose the user-facing
        // message: the generic pattern error plus the hint on a blank line.
        let message = format!("invalid AST pattern: {err}\n\n{hint}");
        assert!(
            message.to_lowercase().contains("nix"),
            "composed error must identify nix: {message}"
        );
    }
}
