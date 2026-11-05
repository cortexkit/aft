//! What the call-graph extractor reads and which version of it wrote a
//! stored extraction: the source files it parses, the resolver
//! configuration files it reads raw, and its version string.

use std::path::Path;

/// Bump when extracted evidence, its encoding or interpretation changes. Version
/// 9 omits the extraction-only AST index from stored payloads; facts are unchanged.
/// Version 10 stores the parser's corrected symbols: Go type aliases, separate
/// ranges for specs in a grouped `type (...)` block, and non-inverted ranges for
/// HTML headings that share a row.
/// Version 14 records the facts behind precise Go receiver, Python package and
/// `super()`, TypeScript object-member and Rust trait and associated calls.
pub const PRODUCER: &str = "ruled-callgraph-v14-resolver-precision";

/// The language the call graph parses the source file at `path` as, or `None`
/// for a file it does not parse. Markdown has outline headings, not callable
/// definitions or call references, so it is never parsed.
pub(crate) fn source_language(path: &Path) -> Option<crate::parser::LangId> {
    crate::parser::detect_language(path)
        .filter(|language| *language != crate::parser::LangId::Markdown)
}

/// Whether the file at root-relative `path` (with `/` separators) is resolver
/// configuration, such as `package.json` or `Cargo.toml`, that the call graph
/// reads raw instead of parsing as source.
pub(crate) fn is_resolution_input(path: &[u8]) -> bool {
    let name = path.rsplit(|byte| *byte == b'/').next().unwrap_or(path);
    name == b"package.json"
        || name == b"Cargo.toml"
        || name == b"go.mod"
        || name == b".gitignore"
        || name.starts_with(b"tsconfig") && name.ends_with(b".json")
}
