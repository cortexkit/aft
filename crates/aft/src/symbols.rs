use serde::{Deserialize, Serialize};

/// The kind of a discovered symbol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Function,
    Class,
    Method,
    Struct,
    Interface,
    Enum,
    TypeAlias,
    /// Top-level const/let variable declaration
    Variable,
    /// Markdown heading (h1, h2, h3, etc.)
    Heading,
    /// Synthetic file-level summary chunk for generic entry-point files.
    FileSummary,
}

/// Location range within a source file (line/column, 0-indexed internally).
///
/// **Serialization**: JSON output is 1-based (matches editor/git conventions).
/// All internal Rust code uses 0-indexed values. The custom `Serialize` impl
/// adds +1 to all fields during serialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Range {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

impl serde::Serialize for Range {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("Range", 4)?;
        s.serialize_field("start_line", &(self.start_line + 1))?;
        s.serialize_field("start_col", &(self.start_col + 1))?;
        s.serialize_field("end_line", &(self.end_line + 1))?;
        s.serialize_field("end_col", &(self.end_col + 1))?;
        s.end()
    }
}

impl<'de> serde::Deserialize<'de> for Range {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct SerializedRange {
            start_line: u32,
            start_col: u32,
            end_line: u32,
            end_col: u32,
        }

        let range = SerializedRange::deserialize(deserializer)?;
        Ok(Self {
            start_line: range.start_line.saturating_sub(1),
            start_col: range.start_col.saturating_sub(1),
            end_line: range.end_line.saturating_sub(1),
            end_col: range.end_col.saturating_sub(1),
        })
    }
}

/// A symbol discovered in a source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub range: Range,
    /// Function/method signature, e.g. `fn foo(x: i32) -> bool`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Scope chain from outermost to innermost parent, e.g. `["ClassName"]` for a method.
    pub scope_chain: Vec<String>,
    /// Whether this symbol is exported (relevant for TS/JS).
    pub exported: bool,
    /// The direct parent symbol name, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// A resolved symbol match — a `Symbol` plus the file it was found in.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolMatch {
    pub symbol: Symbol,
    pub file: String,
}
