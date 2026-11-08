//! Content records: everything parsing one file content yields, stored once
//! per content and shared by every checkout and path that holds it.
//!
//! A record has two parts. The **surface** is what other files may read
//! about this one when they are resolved: its symbols, exports, default
//! export, entry points and type facts. The **body** is what only this
//! file's own resolution reads: its references, imports, Python bindings and
//! method-call sites. Resolving a file reads its own body and the surfaces it
//! depends on, never another file's body.
//!
//! Configuration files are stored as their raw bytes: how they are
//! interpreted depends on the checkout they configure.
//!
//! Records hold no paths. Positions (lines, columns, byte ranges, syntax-tree
//! ordinals) are within the content, so they are the same wherever the
//! content appears.
//!
//! A record can also hold the content's TODO markers ([`TodoMarkers`]),
//! found in its comments when it was parsed, so counting a checkout's TODOs
//! never parses a file. They are written in the first part, right after
//! the record's kind, though no other file reads them: that part is stored
//! uncompressed, so a count reads each file's markers
//! ([`ContentRecord::todos_of`]) without unpacking its body. Files the call
//! graph does not parse but whose comments are counted (Markdown, minified
//! bundles) get a record holding only their markers ([`RecordBody::Markers`]).

pub mod codec;
mod sections;

pub use sections::{
    ContentReadError, ContentSectionsBuilder, ContentSegment, EncodedRecord, KNOWN, write_folded,
};

use codec::{Decoder, Encoder};

use crate::ids::{ContentKey, Digest128};

/// The key of a content record: the content, the language it is parsed as,
/// and the extractor version. The same bytes at a `.ts` and a `.js` path are
/// two records; a new extractor version never reads an old one.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RecordKey(Digest128);

impl RecordKey {
    #[must_use]
    pub fn new(content: ContentKey, language: &str, extractor_version: &str) -> Self {
        let mut hasher = blake3::Hasher::new_derive_key("aft-store 2026-11 content record key");
        hasher.update(content.digest().as_bytes());
        for part in [language, extractor_version] {
            hasher.update(&(part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
        Self(Digest128::from_hash(&hasher.finalize()))
    }

    /// A key read back from storage.
    #[must_use]
    pub const fn from_digest(digest: Digest128) -> Self {
        Self(digest)
    }

    #[must_use]
    pub const fn digest(&self) -> Digest128 {
        self.0
    }
}

/// Why stored bytes are not a well-formed record.
#[derive(Clone, Copy, PartialEq, Eq, Debug, thiserror::Error)]
#[error("content record does not decode: {0}")]
pub struct DecodeError(pub &'static str);

/// One symbol a file defines.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Symbol {
    pub is_test: Option<bool>,
    /// The syntax-tree pre-order position of the defining node.
    pub ordinal: u32,
    pub name: String,
    pub scoped_name: String,
    pub kind: String,
    pub exported: bool,
    pub is_default_export: bool,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub signature: Option<String>,
    /// Whether the program starts at this symbol, as its language defines
    /// it (`fn main`, `func main` in a Go `package main`, a Go
    /// `func init()`, a C `main`).
    pub program_start: bool,
    /// The entry attribute or decorator extraction recorded on the symbol,
    /// such as `no_mangle` or `tauri::command`.
    pub entry_attribute: Option<String>,
}

/// The parse-level class of a reference.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum RefKind {
    Construction,
    CallbackRef,
    Call,
    ValueRef,
    Import,
    Module,
    Reexport,
    ExportAlias,
    /// A name inside a Rust macro whose tokens could not be parsed as
    /// Rust: a possible call the graph cannot see.
    MacroMention,
}

impl RefKind {
    const ALL: [Self; 9] = [
        Self::Construction,
        Self::CallbackRef,
        Self::Call,
        Self::ValueRef,
        Self::Import,
        Self::Module,
        Self::Reexport,
        Self::ExportAlias,
        Self::MacroMention,
    ];

    fn code(self) -> u8 {
        Self::ALL
            .iter()
            .position(|kind| *kind == self)
            .and_then(|index| u8::try_from(index).ok())
            .expect("every kind is in ALL")
    }

    fn from_code(code: u8) -> Result<Self, DecodeError> {
        Self::ALL
            .get(usize::from(code))
            .copied()
            .ok_or(DecodeError("reference kind"))
    }
}

/// A reference as extracted, before resolution: no target is known.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Reference {
    pub ordinal: u32,
    pub kind: RefKind,
    pub caller_symbol: Option<String>,
    pub short_name: Option<String>,
    pub full_ref: Option<String>,
    pub module_path: Option<String>,
    pub line: u32,
    pub byte_start: u64,
    pub byte_end: u64,
    pub path_override: Option<String>,
    pub local_name: Option<String>,
    pub requested_name: Option<String>,
    pub namespace_alias: Option<String>,
    pub wildcard: bool,
    pub import_kind: Option<String>,
}

/// An import statement.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Import {
    pub ordinal: u32,
    pub module_path: String,
    pub names: Vec<String>,
    pub default_import: Option<String>,
    pub namespace_import: Option<String>,
    pub byte_start: u64,
    pub byte_end: u64,
    pub raw_text: String,
    pub type_only: bool,
    pub side_effect: bool,
}

/// A Python name binding at one span: the module and symbol it refers to,
/// or `None` when it is shadowed locally.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PythonBinding {
    pub byte_start: u64,
    pub byte_end: u64,
    pub target: Option<(String, String)>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TypeFact {
    pub name: String,
    pub bases: Vec<String>,
    pub interface: bool,
    pub closed: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MethodFact {
    pub owner: String,
    pub trait_name: Option<String>,
    pub name: String,
    pub symbol: String,
    pub has_body: bool,
    pub shape: String,
    pub private: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FieldFact {
    pub owner: String,
    pub name: String,
    pub ty: String,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ReturnFact {
    pub symbol: String,
    pub ty: String,
}

/// A method-call site: a call whose target depends on a receiver's type.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CallSite {
    pub ordinal: u32,
    pub caller: Option<String>,
    pub line: u32,
    pub member: Option<String>,
    pub receiver: Option<String>,
    pub dynamic: bool,
}

/// What other files may read about a parsed file.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Surface {
    pub symbols: Vec<Symbol>,
    pub default_export_symbol: Option<String>,
    pub exported_symbols: Vec<String>,
    pub callable_symbols: Vec<String>,
    pub entry_point_symbols: Vec<String>,
    /// Go's package clause, which tells co-located test packages apart.
    pub go_package: Option<String>,
    pub types: Vec<TypeFact>,
    pub methods: Vec<MethodFact>,
    pub fields: Vec<FieldFact>,
    pub returns: Vec<ReturnFact>,
}

/// One symbol as outline lists it. Outline reads the parser's own symbol
/// table for a file, which differs from [`Surface::symbols`]: the call graph
/// qualifies, adds and merges symbols for resolution, outline shows them as
/// written. Lines and columns are zero-based.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OutlineSymbol {
    pub name: String,
    /// The parser's symbol kind, in its snake-case name (`function`,
    /// `type_alias`).
    pub kind: String,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub signature: Option<String>,
    /// The enclosing scopes, outermost first.
    pub scope_chain: Vec<String>,
    pub exported: bool,
    pub parent: Option<String>,
}

/// What only reads of this one file need: its own resolution, and its
/// outline.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Body {
    pub imports: Vec<Import>,
    pub python_bindings: Vec<PythonBinding>,
    pub references: Vec<Reference>,
    pub call_sites: Vec<CallSite>,
    /// The file's symbols as outline lists them, in the parser's order.
    /// `None` in a record from an extractor that did not store them.
    pub outline: Option<Vec<OutlineSymbol>>,
    /// What dead-code analysis reads from the file's syntax tree, recorded
    /// when the file is parsed so an analysis never parses it again. The
    /// store does not read these bytes: their format belongs to the
    /// extractor that wrote them, which the record's extractor version
    /// names. `None` in a record from an extractor that did not store them.
    pub dead_code: Option<Vec<u8>>,
}

/// What a content record holds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RecordBody {
    Parsed {
        surface: Box<Surface>,
        body: Box<Body>,
    },
    /// A configuration file's raw bytes.
    Config { source: Vec<u8> },
    /// Nothing but the record's [`ContentRecord::todos`]: a file the call
    /// graph does not parse whose comments are still counted.
    Markers,
}

/// One TODO marker comment line, as AFT's TODO count reports it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TodoMarker {
    /// The one-based line the marker is on.
    pub line: u32,
    /// The marker word, such as `TODO` or `FIXME`.
    pub marker: String,
    /// The name in parentheses after the marker (`TODO(name):`), if any.
    pub author: Option<String>,
    /// The comment's text after the marker, as the count shows it.
    pub text: String,
}

/// The TODO markers of one content.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TodoMarkers {
    /// The language the content's comments were read as. The same bytes can
    /// be read as another language at another path, so a reader checks it
    /// against the path's.
    pub language: String,
    /// The markers in line order; `None` when the content is not text the
    /// count reads (not UTF-8, or holding a NUL byte).
    pub items: Option<Vec<TodoMarker>>,
}

/// Everything parsing one content yields.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ContentRecord {
    pub language: String,
    /// The extractor that produced the record. A record from another
    /// version is extracted again, never reused.
    pub extractor_version: String,
    pub data: RecordBody,
    /// The content's TODO markers. `None` in a record from an extractor
    /// that did not look for them, or of a file whose comments are not
    /// counted.
    pub todos: Option<TodoMarkers>,
}

/// A parsed record whose body holds no outline. Records of this kind encode
/// exactly as they did before records could hold one.
const PARSED: u8 = 1;
const CONFIG: u8 = 2;
/// A parsed record whose body ends with its outline. The kind byte, not the
/// body's length, says whether an outline follows, so a body cut short
/// before its outline still fails to decode.
const PARSED_WITH_OUTLINE: u8 = 3;
/// [`PARSED`], with each symbol's entry facts ([`Symbol::program_start`],
/// [`Symbol::entry_attribute`]) after the rest of the surface. Records of
/// the two kinds above predate those facts and read back without them.
const PARSED_WITH_ENTRIES: u8 = 4;
/// [`PARSED_WITH_OUTLINE`], with each symbol's entry facts after the rest
/// of the surface.
const PARSED_WITH_ENTRIES_AND_OUTLINE: u8 = 5;
/// [`PARSED_WITH_ENTRIES_AND_OUTLINE`], whose body ends with its dead-code
/// facts after the outline.
const PARSED_WITH_ENTRIES_OUTLINE_AND_DEAD_CODE: u8 = 6;
/// [`PARSED_WITH_ENTRIES`], whose body ends with its dead-code facts.
const PARSED_WITH_ENTRIES_AND_DEAD_CODE: u8 = 7;
/// A parsed record with its TODO markers. After the kind come a byte of
/// [`HAS_OUTLINE`] and [`HAS_DEAD_CODE`] flags, saying what the body ends
/// with, then the markers, the surface and each symbol's entry facts.
const PARSED_WITH_TODOS: u8 = 8;
/// [`CONFIG`] with the TODO markers between the kind and the source.
const CONFIG_WITH_TODOS: u8 = 9;
/// A [`RecordBody::Markers`] record: the kind, then whether markers follow
/// (a byte, 0 or 1), then the markers. Its body part is empty.
const MARKERS: u8 = 10;
/// In a [`PARSED_WITH_TODOS`] record's flags: the body ends with an outline.
const HAS_OUTLINE: u8 = 1;
/// In a [`PARSED_WITH_TODOS`] record's flags: the body ends with dead-code
/// facts, after the outline when there is one.
const HAS_DEAD_CODE: u8 = 2;

impl ContentRecord {
    /// The surface part's bytes and the body part's bytes. A configuration
    /// or markers-only record has an empty body part. A record without TODO
    /// markers encodes exactly as it did before records could hold them.
    #[must_use]
    pub fn encode(&self) -> (Vec<u8>, Vec<u8>) {
        let mut head = Encoder::default();
        head.str(&self.language);
        head.str(&self.extractor_version);
        match &self.data {
            RecordBody::Config { source } => {
                match &self.todos {
                    None => head.byte(CONFIG),
                    Some(todos) => {
                        head.byte(CONFIG_WITH_TODOS);
                        encode_todos(&mut head, todos);
                    }
                }
                head.blob(source);
                (head.finish(), Vec::new())
            }
            RecordBody::Markers => {
                head.byte(MARKERS);
                head.bool(self.todos.is_some());
                if let Some(todos) = &self.todos {
                    encode_todos(&mut head, todos);
                }
                (head.finish(), Vec::new())
            }
            RecordBody::Parsed { surface, body } => {
                match &self.todos {
                    None => head.byte(match (body.outline.is_some(), body.dead_code.is_some()) {
                        (true, true) => PARSED_WITH_ENTRIES_OUTLINE_AND_DEAD_CODE,
                        (true, false) => PARSED_WITH_ENTRIES_AND_OUTLINE,
                        (false, true) => PARSED_WITH_ENTRIES_AND_DEAD_CODE,
                        (false, false) => PARSED_WITH_ENTRIES,
                    }),
                    Some(todos) => {
                        head.byte(PARSED_WITH_TODOS);
                        let mut flags = 0;
                        if body.outline.is_some() {
                            flags |= HAS_OUTLINE;
                        }
                        if body.dead_code.is_some() {
                            flags |= HAS_DEAD_CODE;
                        }
                        head.byte(flags);
                        encode_todos(&mut head, todos);
                    }
                }
                encode_surface(&mut head, surface);
                encode_entries(&mut head, surface);
                let mut tail = Encoder::default();
                encode_body(&mut tail, body);
                (head.finish(), tail.finish())
            }
        }
    }

    /// Reads a record from its two parts.
    ///
    /// # Errors
    ///
    /// When either part is not well formed.
    pub fn decode(surface: &[u8], body: &[u8]) -> Result<Self, DecodeError> {
        let mut head = Decoder::new(surface)?;
        let language = head.string()?;
        let extractor_version = head.string()?;
        let mut todos = None;
        let data = match head.byte()? {
            kind @ (CONFIG | CONFIG_WITH_TODOS) => {
                if kind == CONFIG_WITH_TODOS {
                    todos = Some(decode_todos(&mut head)?);
                }
                let source = head.blob()?.to_vec();
                head.finish()?;
                if !body.is_empty() {
                    return Err(DecodeError("a configuration record has a body"));
                }
                RecordBody::Config { source }
            }
            MARKERS => {
                if head.bool()? {
                    todos = Some(decode_todos(&mut head)?);
                }
                head.finish()?;
                if !body.is_empty() {
                    return Err(DecodeError("a markers record has a body"));
                }
                RecordBody::Markers
            }
            kind @ (PARSED
            | PARSED_WITH_OUTLINE
            | PARSED_WITH_ENTRIES
            | PARSED_WITH_ENTRIES_AND_OUTLINE
            | PARSED_WITH_ENTRIES_OUTLINE_AND_DEAD_CODE
            | PARSED_WITH_ENTRIES_AND_DEAD_CODE
            | PARSED_WITH_TODOS) => {
                // What the kind says follows: entry facts after the surface,
                // and an outline and dead-code facts at the body's end.
                let (entries, outline, dead_code) = if kind == PARSED_WITH_TODOS {
                    let flags = head.byte()?;
                    if flags & !(HAS_OUTLINE | HAS_DEAD_CODE) != 0 {
                        return Err(DecodeError("parsed record flags"));
                    }
                    todos = Some(decode_todos(&mut head)?);
                    (true, flags & HAS_OUTLINE != 0, flags & HAS_DEAD_CODE != 0)
                } else {
                    (
                        matches!(
                            kind,
                            PARSED_WITH_ENTRIES
                                | PARSED_WITH_ENTRIES_AND_OUTLINE
                                | PARSED_WITH_ENTRIES_OUTLINE_AND_DEAD_CODE
                                | PARSED_WITH_ENTRIES_AND_DEAD_CODE
                        ),
                        matches!(
                            kind,
                            PARSED_WITH_OUTLINE
                                | PARSED_WITH_ENTRIES_AND_OUTLINE
                                | PARSED_WITH_ENTRIES_OUTLINE_AND_DEAD_CODE
                        ),
                        matches!(
                            kind,
                            PARSED_WITH_ENTRIES_OUTLINE_AND_DEAD_CODE
                                | PARSED_WITH_ENTRIES_AND_DEAD_CODE
                        ),
                    )
                };
                let mut surface = decode_surface(&mut head)?;
                if entries {
                    decode_entries(&mut head, &mut surface)?;
                }
                head.finish()?;
                let mut tail = Decoder::new(body)?;
                let mut body = decode_body(&mut tail)?;
                if outline {
                    body.outline = Some(decode_outline(&mut tail)?);
                }
                if dead_code {
                    body.dead_code = Some(tail.blob()?.to_vec());
                }
                tail.finish()?;
                RecordBody::Parsed {
                    surface: Box::new(surface),
                    body: Box::new(body),
                }
            }
            _ => return Err(DecodeError("record kind")),
        };
        Ok(Self {
            language,
            extractor_version,
            data,
            todos,
        })
    }

    /// The TODO markers of the record whose surface part is `surface`,
    /// read without decoding the rest of the record: they come right after
    /// its kind. `None` when the record holds none.
    ///
    /// # Errors
    ///
    /// When the bytes before and of the markers are not well formed.
    pub fn todos_of(surface: &[u8]) -> Result<Option<TodoMarkers>, DecodeError> {
        let mut head = Decoder::new(surface)?;
        head.str()?;
        head.str()?;
        match head.byte()? {
            CONFIG_WITH_TODOS => decode_todos(&mut head).map(Some),
            MARKERS if head.bool()? => decode_todos(&mut head).map(Some),
            PARSED_WITH_TODOS => {
                head.byte()?;
                decode_todos(&mut head).map(Some)
            }
            CONFIG
            | MARKERS
            | PARSED
            | PARSED_WITH_OUTLINE
            | PARSED_WITH_ENTRIES
            | PARSED_WITH_ENTRIES_AND_OUTLINE
            | PARSED_WITH_ENTRIES_OUTLINE_AND_DEAD_CODE
            | PARSED_WITH_ENTRIES_AND_DEAD_CODE => Ok(None),
            _ => Err(DecodeError("record kind")),
        }
    }
}

/// The markers' language, then a byte saying whether the content was text
/// (1) or not (0), then for text the count of markers and each marker.
fn encode_todos(out: &mut Encoder, todos: &TodoMarkers) {
    out.str(&todos.language);
    out.bool(todos.items.is_some());
    if let Some(items) = &todos.items {
        out.usize(items.len());
        for item in items {
            out.u32(item.line);
            out.str(&item.marker);
            out.opt_str(item.author.as_deref());
            out.str(&item.text);
        }
    }
}

fn decode_todos(input: &mut Decoder<'_>) -> Result<TodoMarkers, DecodeError> {
    let language = input.string()?;
    let items = if input.bool()? {
        let count = input.count()?;
        let mut items = Vec::with_capacity(count);
        for _ in 0..count {
            items.push(TodoMarker {
                line: input.u32()?,
                marker: input.string()?,
                author: input.opt_string()?,
                text: input.string()?,
            });
        }
        Some(items)
    } else {
        None
    };
    Ok(TodoMarkers { language, items })
}

fn encode_surface(out: &mut Encoder, surface: &Surface) {
    out.usize(surface.symbols.len());
    for symbol in &surface.symbols {
        out.byte(match symbol.is_test {
            None => 0,
            Some(false) => 1,
            Some(true) => 2,
        });
        out.u32(symbol.ordinal);
        out.str(&symbol.name);
        out.str(&symbol.scoped_name);
        out.str(&symbol.kind);
        out.bool(symbol.exported);
        out.bool(symbol.is_default_export);
        out.u32(symbol.start_line);
        out.u32(symbol.start_col);
        out.u32(symbol.end_line);
        out.u32(symbol.end_col);
        out.opt_str(symbol.signature.as_deref());
    }
    out.opt_str(surface.default_export_symbol.as_deref());
    out.strs(&surface.exported_symbols);
    out.strs(&surface.callable_symbols);
    out.strs(&surface.entry_point_symbols);
    out.opt_str(surface.go_package.as_deref());
    out.usize(surface.types.len());
    for fact in &surface.types {
        out.str(&fact.name);
        out.strs(&fact.bases);
        out.bool(fact.interface);
        out.bool(fact.closed);
    }
    out.usize(surface.methods.len());
    for fact in &surface.methods {
        out.str(&fact.owner);
        out.opt_str(fact.trait_name.as_deref());
        out.str(&fact.name);
        out.str(&fact.symbol);
        out.bool(fact.has_body);
        out.str(&fact.shape);
        out.bool(fact.private);
    }
    out.usize(surface.fields.len());
    for fact in &surface.fields {
        out.str(&fact.owner);
        out.str(&fact.name);
        out.str(&fact.ty);
    }
    out.usize(surface.returns.len());
    for fact in &surface.returns {
        out.str(&fact.symbol);
        out.str(&fact.ty);
    }
}

fn decode_surface(input: &mut Decoder<'_>) -> Result<Surface, DecodeError> {
    let count = input.count()?;
    let mut symbols = Vec::with_capacity(count);
    for _ in 0..count {
        symbols.push(Symbol {
            is_test: match input.byte()? {
                0 => None,
                1 => Some(false),
                2 => Some(true),
                _ => return Err(DecodeError("is_test")),
            },
            ordinal: input.u32()?,
            name: input.string()?,
            scoped_name: input.string()?,
            kind: input.string()?,
            exported: input.bool()?,
            is_default_export: input.bool()?,
            start_line: input.u32()?,
            start_col: input.u32()?,
            end_line: input.u32()?,
            end_col: input.u32()?,
            signature: input.opt_string()?,
            program_start: false,
            entry_attribute: None,
        });
    }
    let default_export_symbol = input.opt_string()?;
    let exported_symbols = input.strings()?;
    let callable_symbols = input.strings()?;
    let entry_point_symbols = input.strings()?;
    let go_package = input.opt_string()?;
    let count = input.count()?;
    let mut types = Vec::with_capacity(count);
    for _ in 0..count {
        types.push(TypeFact {
            name: input.string()?,
            bases: input.strings()?,
            interface: input.bool()?,
            closed: input.bool()?,
        });
    }
    let count = input.count()?;
    let mut methods = Vec::with_capacity(count);
    for _ in 0..count {
        methods.push(MethodFact {
            owner: input.string()?,
            trait_name: input.opt_string()?,
            name: input.string()?,
            symbol: input.string()?,
            has_body: input.bool()?,
            shape: input.string()?,
            private: input.bool()?,
        });
    }
    let count = input.count()?;
    let mut fields = Vec::with_capacity(count);
    for _ in 0..count {
        fields.push(FieldFact {
            owner: input.string()?,
            name: input.string()?,
            ty: input.string()?,
        });
    }
    let count = input.count()?;
    let mut returns = Vec::with_capacity(count);
    for _ in 0..count {
        returns.push(ReturnFact {
            symbol: input.string()?,
            ty: input.string()?,
        });
    }
    Ok(Surface {
        symbols,
        default_export_symbol,
        exported_symbols,
        callable_symbols,
        entry_point_symbols,
        go_package,
        types,
        methods,
        fields,
        returns,
    })
}

/// Each symbol's entry facts, in symbol order, after the rest of the
/// surface of a [`PARSED_WITH_ENTRIES`] or
/// [`PARSED_WITH_ENTRIES_AND_OUTLINE`] record.
fn encode_entries(out: &mut Encoder, surface: &Surface) {
    for symbol in &surface.symbols {
        out.bool(symbol.program_start);
        out.opt_str(symbol.entry_attribute.as_deref());
    }
}

fn decode_entries(input: &mut Decoder<'_>, surface: &mut Surface) -> Result<(), DecodeError> {
    for symbol in &mut surface.symbols {
        symbol.program_start = input.bool()?;
        symbol.entry_attribute = input.opt_string()?;
    }
    Ok(())
}

fn encode_body(out: &mut Encoder, body: &Body) {
    out.usize(body.imports.len());
    for import in &body.imports {
        out.u32(import.ordinal);
        out.str(&import.module_path);
        out.strs(&import.names);
        out.opt_str(import.default_import.as_deref());
        out.opt_str(import.namespace_import.as_deref());
        out.u64(import.byte_start);
        out.u64(import.byte_end);
        out.str(&import.raw_text);
        out.bool(import.type_only);
        out.bool(import.side_effect);
    }
    out.usize(body.python_bindings.len());
    for binding in &body.python_bindings {
        out.u64(binding.byte_start);
        out.u64(binding.byte_end);
        match &binding.target {
            None => out.bool(false),
            Some((module, symbol)) => {
                out.bool(true);
                out.str(module);
                out.str(symbol);
            }
        }
    }
    out.usize(body.references.len());
    for reference in &body.references {
        out.u32(reference.ordinal);
        out.byte(reference.kind.code());
        out.opt_str(reference.caller_symbol.as_deref());
        out.opt_str(reference.short_name.as_deref());
        out.opt_str(reference.full_ref.as_deref());
        out.opt_str(reference.module_path.as_deref());
        out.u32(reference.line);
        out.u64(reference.byte_start);
        out.u64(reference.byte_end);
        out.opt_str(reference.path_override.as_deref());
        out.opt_str(reference.local_name.as_deref());
        out.opt_str(reference.requested_name.as_deref());
        out.opt_str(reference.namespace_alias.as_deref());
        out.bool(reference.wildcard);
        out.opt_str(reference.import_kind.as_deref());
    }
    out.usize(body.call_sites.len());
    for site in &body.call_sites {
        out.u32(site.ordinal);
        out.opt_str(site.caller.as_deref());
        out.u32(site.line);
        out.opt_str(site.member.as_deref());
        out.opt_str(site.receiver.as_deref());
        out.bool(site.dynamic);
    }
    if let Some(outline) = &body.outline {
        out.usize(outline.len());
        for symbol in outline {
            out.str(&symbol.name);
            out.str(&symbol.kind);
            out.u32(symbol.start_line);
            out.u32(symbol.start_col);
            out.u32(symbol.end_line);
            out.u32(symbol.end_col);
            out.opt_str(symbol.signature.as_deref());
            out.strs(&symbol.scope_chain);
            out.bool(symbol.exported);
            out.opt_str(symbol.parent.as_deref());
        }
    }
    if let Some(facts) = &body.dead_code {
        out.blob(facts);
    }
}

fn decode_body(input: &mut Decoder<'_>) -> Result<Body, DecodeError> {
    let count = input.count()?;
    let mut imports = Vec::with_capacity(count);
    for _ in 0..count {
        imports.push(Import {
            ordinal: input.u32()?,
            module_path: input.string()?,
            names: input.strings()?,
            default_import: input.opt_string()?,
            namespace_import: input.opt_string()?,
            byte_start: input.u64()?,
            byte_end: input.u64()?,
            raw_text: input.string()?,
            type_only: input.bool()?,
            side_effect: input.bool()?,
        });
    }
    let count = input.count()?;
    let mut python_bindings = Vec::with_capacity(count);
    for _ in 0..count {
        let byte_start = input.u64()?;
        let byte_end = input.u64()?;
        let target = if input.bool()? {
            Some((input.string()?, input.string()?))
        } else {
            None
        };
        python_bindings.push(PythonBinding {
            byte_start,
            byte_end,
            target,
        });
    }
    let count = input.count()?;
    let mut references = Vec::with_capacity(count);
    for _ in 0..count {
        references.push(Reference {
            ordinal: input.u32()?,
            kind: RefKind::from_code(input.byte()?)?,
            caller_symbol: input.opt_string()?,
            short_name: input.opt_string()?,
            full_ref: input.opt_string()?,
            module_path: input.opt_string()?,
            line: input.u32()?,
            byte_start: input.u64()?,
            byte_end: input.u64()?,
            path_override: input.opt_string()?,
            local_name: input.opt_string()?,
            requested_name: input.opt_string()?,
            namespace_alias: input.opt_string()?,
            wildcard: input.bool()?,
            import_kind: input.opt_string()?,
        });
    }
    let count = input.count()?;
    let mut call_sites = Vec::with_capacity(count);
    for _ in 0..count {
        call_sites.push(CallSite {
            ordinal: input.u32()?,
            caller: input.opt_string()?,
            line: input.u32()?,
            member: input.opt_string()?,
            receiver: input.opt_string()?,
            dynamic: input.bool()?,
        });
    }
    Ok(Body {
        imports,
        python_bindings,
        references,
        call_sites,
        outline: None,
        dead_code: None,
    })
}

/// The outline a body of a [`PARSED_WITH_OUTLINE`] record ends with.
fn decode_outline(input: &mut Decoder<'_>) -> Result<Vec<OutlineSymbol>, DecodeError> {
    let count = input.count()?;
    let mut outline = Vec::with_capacity(count);
    for _ in 0..count {
        outline.push(OutlineSymbol {
            name: input.string()?,
            kind: input.string()?,
            start_line: input.u32()?,
            start_col: input.u32()?,
            end_line: input.u32()?,
            end_col: input.u32()?,
            signature: input.opt_string()?,
            scope_chain: input.strings()?,
            exported: input.bool()?,
            parent: input.opt_string()?,
        });
    }
    Ok(outline)
}

crate::heap_bytes_none!(RecordKey, RefKind);
crate::heap_bytes_of_fields!(Symbol {
    is_test,
    ordinal,
    name,
    scoped_name,
    kind,
    exported,
    is_default_export,
    start_line,
    start_col,
    end_line,
    end_col,
    signature,
    program_start,
    entry_attribute,
});
crate::heap_bytes_of_fields!(Reference {
    ordinal,
    kind,
    caller_symbol,
    short_name,
    full_ref,
    module_path,
    line,
    byte_start,
    byte_end,
    path_override,
    local_name,
    requested_name,
    namespace_alias,
    wildcard,
    import_kind,
});
crate::heap_bytes_of_fields!(Import {
    ordinal,
    module_path,
    names,
    default_import,
    namespace_import,
    byte_start,
    byte_end,
    raw_text,
    type_only,
    side_effect,
});
crate::heap_bytes_of_fields!(PythonBinding {
    byte_start,
    byte_end,
    target,
});
crate::heap_bytes_of_fields!(TypeFact {
    name,
    bases,
    interface,
    closed,
});
crate::heap_bytes_of_fields!(MethodFact {
    owner,
    trait_name,
    name,
    symbol,
    has_body,
    shape,
    private,
});
crate::heap_bytes_of_fields!(FieldFact { owner, name, ty });
crate::heap_bytes_of_fields!(ReturnFact { symbol, ty });
crate::heap_bytes_of_fields!(CallSite {
    ordinal,
    caller,
    line,
    member,
    receiver,
    dynamic,
});
crate::heap_bytes_of_fields!(Surface {
    symbols,
    default_export_symbol,
    exported_symbols,
    callable_symbols,
    entry_point_symbols,
    go_package,
    types,
    methods,
    fields,
    returns,
});
crate::heap_bytes_of_fields!(OutlineSymbol {
    name,
    kind,
    start_line,
    start_col,
    end_line,
    end_col,
    signature,
    scope_chain,
    exported,
    parent,
});
crate::heap_bytes_of_fields!(Body {
    imports,
    python_bindings,
    references,
    call_sites,
    outline,
    dead_code,
});
crate::heap_bytes_of_fields!(TodoMarker {
    line,
    marker,
    author,
    text,
});
crate::heap_bytes_of_fields!(TodoMarkers { language, items });
crate::heap_bytes_of_fields!(ContentRecord {
    language,
    extractor_version,
    data,
    todos,
});

impl crate::heap::HeapBytes for RecordBody {
    fn heap_bytes(&self, seen: &mut crate::heap::Seen) -> usize {
        match self {
            Self::Parsed { surface, body } => {
                crate::heap::HeapBytes::heap_bytes(surface, seen)
                    + crate::heap::HeapBytes::heap_bytes(body, seen)
            }
            Self::Config { source } => crate::heap::HeapBytes::heap_bytes(source, seen),
            Self::Markers => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ContentRecord {
        ContentRecord {
            language: "rust".into(),
            extractor_version: "v1".into(),
            data: RecordBody::Parsed {
                surface: Box::new(Surface {
                    symbols: vec![Symbol {
                        is_test: Some(false),
                        ordinal: 7,
                        name: "open".into(),
                        scoped_name: "Store::open".into(),
                        kind: "method".into(),
                        exported: true,
                        is_default_export: false,
                        start_line: 3,
                        start_col: 4,
                        end_line: 9,
                        end_col: 5,
                        signature: Some("fn open() -> Self".into()),
                        program_start: false,
                        entry_attribute: Some("no_mangle".into()),
                    }],
                    exported_symbols: vec!["Store".into()],
                    callable_symbols: vec!["Store::open".into()],
                    go_package: None,
                    types: vec![TypeFact {
                        name: "Store".into(),
                        bases: vec!["Base".into()],
                        interface: false,
                        closed: true,
                    }],
                    methods: vec![MethodFact {
                        owner: "Store".into(),
                        trait_name: None,
                        name: "open".into(),
                        symbol: "Store::open".into(),
                        has_body: true,
                        shape: "()".into(),
                        private: false,
                    }],
                    fields: vec![FieldFact {
                        owner: "Store".into(),
                        name: "path".into(),
                        ty: "PathBuf".into(),
                    }],
                    returns: vec![ReturnFact {
                        symbol: "Store::open".into(),
                        ty: "Store".into(),
                    }],
                    ..Surface::default()
                }),
                body: Box::new(Body {
                    imports: vec![Import {
                        ordinal: 1,
                        module_path: "std::path".into(),
                        names: vec!["PathBuf".into()],
                        default_import: None,
                        namespace_import: None,
                        byte_start: 0,
                        byte_end: 22,
                        raw_text: "use std::path::PathBuf;".into(),
                        type_only: false,
                        side_effect: false,
                    }],
                    python_bindings: vec![PythonBinding {
                        byte_start: 2,
                        byte_end: 3,
                        target: Some(("m".into(), "f".into())),
                    }],
                    references: vec![Reference {
                        ordinal: 9,
                        kind: RefKind::Call,
                        caller_symbol: Some("Store::open".into()),
                        short_name: Some("new".into()),
                        full_ref: Some("PathBuf::new".into()),
                        module_path: None,
                        line: 5,
                        byte_start: 100,
                        byte_end: 112,
                        path_override: None,
                        local_name: None,
                        requested_name: None,
                        namespace_alias: None,
                        wildcard: false,
                        import_kind: None,
                    }],
                    call_sites: vec![CallSite {
                        ordinal: 11,
                        caller: Some("Store::open".into()),
                        line: 6,
                        member: Some("join".into()),
                        receiver: Some("self.path".into()),
                        dynamic: false,
                    }],
                    outline: None,
                    dead_code: None,
                }),
            },
            todos: None,
        }
    }

    fn todos(language: &str) -> TodoMarkers {
        TodoMarkers {
            language: language.into(),
            items: Some(vec![
                TodoMarker {
                    line: 4,
                    marker: "TODO".into(),
                    author: Some("ana".into()),
                    text: "close the store".into(),
                },
                TodoMarker {
                    line: 9,
                    marker: "FIXME".into(),
                    author: None,
                    text: String::new(),
                },
            ]),
        }
    }

    /// Every kind of record round-trips its TODO markers, which
    /// `todos_of` reads from the first part alone; a record without them
    /// encodes exactly as before records could hold them, and every cut of
    /// the first part is refused.
    #[test]
    fn todo_markers_round_trip_in_every_kind_and_read_from_the_first_part() {
        let mut parsed = sample();
        let RecordBody::Parsed { body, .. } = &mut parsed.data else {
            unreachable!("the sample is parsed");
        };
        body.outline = Some(outline());
        body.dead_code = Some(b"facts".to_vec());
        let without = parsed.clone();
        parsed.todos = Some(todos("rust"));
        let config = ContentRecord {
            language: "config".into(),
            extractor_version: "v1".into(),
            data: RecordBody::Config {
                source: b"# TODO: x\n".to_vec(),
            },
            todos: Some(todos("toml")),
        };
        let markers = ContentRecord {
            language: "markers-markdown".into(),
            extractor_version: "v1".into(),
            data: RecordBody::Markers,
            todos: Some(todos("markdown")),
        };
        let not_text = ContentRecord {
            todos: Some(TodoMarkers {
                language: "markdown".into(),
                items: None,
            }),
            ..markers.clone()
        };
        let empty_markers = ContentRecord {
            todos: None,
            ..markers.clone()
        };
        for record in [&parsed, &config, &markers, &not_text, &empty_markers] {
            let (surface, body) = record.encode();
            assert_eq!(&ContentRecord::decode(&surface, &body).unwrap(), record);
            assert_eq!(ContentRecord::todos_of(&surface).unwrap(), record.todos);
            for cut in 0..surface.len() {
                assert!(ContentRecord::decode(&surface[..cut], &body).is_err());
            }
        }
        let (surface, body) = without.encode();
        assert_eq!(ContentRecord::todos_of(&surface).unwrap(), None);
        assert_eq!(ContentRecord::decode(&surface, &body).unwrap(), without);
        // Without markers the kind is the one records had before them.
        let mut head = Decoder::new(&surface).unwrap();
        head.str().unwrap();
        head.str().unwrap();
        assert_eq!(
            head.byte().unwrap(),
            PARSED_WITH_ENTRIES_OUTLINE_AND_DEAD_CODE
        );
    }

    fn outline() -> Vec<OutlineSymbol> {
        vec![
            OutlineSymbol {
                name: "Store".into(),
                kind: "struct".into(),
                start_line: 1,
                start_col: 0,
                end_line: 1,
                end_col: 33,
                signature: Some("pub struct Store".into()),
                scope_chain: Vec::new(),
                exported: true,
                parent: None,
            },
            OutlineSymbol {
                name: "open".into(),
                kind: "method".into(),
                start_line: 3,
                start_col: 4,
                end_line: 9,
                end_col: 5,
                signature: Some("fn open() -> Self".into()),
                scope_chain: vec!["Store".into()],
                exported: true,
                parent: Some("Store".into()),
            },
        ]
    }

    #[test]
    fn an_outline_round_trips_and_its_absence_is_told_by_the_record_kind() {
        let without = sample();
        let mut with = sample();
        let RecordBody::Parsed { body, .. } = &mut with.data else {
            unreachable!("the sample is parsed");
        };
        body.outline = Some(outline());
        let (surface, body) = with.encode();
        assert_eq!(ContentRecord::decode(&surface, &body).unwrap(), with);
        let (plain_surface, plain_body) = without.encode();
        let decoded = ContentRecord::decode(&plain_surface, &plain_body).unwrap();
        assert_eq!(decoded, without);

        // The two surfaces differ only in the record kind.
        assert_eq!(surface.len(), plain_surface.len());
        let differing = surface
            .iter()
            .zip(&plain_surface)
            .filter(|(a, b)| a != b)
            .map(|(a, b)| (*a, *b))
            .collect::<Vec<_>>();
        assert_eq!(
            differing,
            vec![(PARSED_WITH_ENTRIES_AND_OUTLINE, PARSED_WITH_ENTRIES)]
        );

        // A body without its outline, or with one the kind does not
        // announce, is refused, as is every cut of a body with one.
        assert!(ContentRecord::decode(&surface, &plain_body).is_err());
        assert!(ContentRecord::decode(&plain_surface, &body).is_err());
        for cut in 0..body.len() {
            assert!(ContentRecord::decode(&surface, &body[..cut]).is_err());
        }

        // An empty outline is still an outline.
        let mut empty = sample();
        let RecordBody::Parsed { body, .. } = &mut empty.data else {
            unreachable!("the sample is parsed");
        };
        body.outline = Some(Vec::new());
        let (surface, body) = empty.encode();
        assert_eq!(ContentRecord::decode(&surface, &body).unwrap(), empty);
    }

    #[test]
    fn dead_code_facts_round_trip_with_and_without_an_outline() {
        for outline in [None, Some(outline())] {
            let without = {
                let mut record = sample();
                let RecordBody::Parsed { body, .. } = &mut record.data else {
                    unreachable!("the sample is parsed");
                };
                body.outline.clone_from(&outline);
                record
            };
            let mut with = without.clone();
            let RecordBody::Parsed { body, .. } = &mut with.data else {
                unreachable!("the sample is parsed");
            };
            body.dead_code = Some(b"{\"facts\":1}".to_vec());
            let (surface, body) = with.encode();
            assert_eq!(ContentRecord::decode(&surface, &body).unwrap(), with);
            let (plain_surface, plain_body) = without.encode();
            assert_eq!(
                ContentRecord::decode(&plain_surface, &plain_body).unwrap(),
                without
            );

            // The record kind alone says whether facts follow: a body without
            // them, or with facts the kind does not announce, is refused, as
            // is every cut of a body with them.
            assert!(ContentRecord::decode(&surface, &plain_body).is_err());
            assert!(ContentRecord::decode(&plain_surface, &body).is_err());
            for cut in 0..body.len() {
                assert!(ContentRecord::decode(&surface, &body[..cut]).is_err());
            }
        }
    }

    #[test]
    fn records_round_trip_and_refuse_damage() {
        let record = sample();
        let (surface, body) = record.encode();
        assert_eq!(ContentRecord::decode(&surface, &body).unwrap(), record);
        // Strings repeat across fields but are stored once.
        let text = String::from_utf8_lossy(&surface);
        assert_eq!(text.matches("Store::open").count(), 1);

        let config = ContentRecord {
            language: "config".into(),
            extractor_version: "v1".into(),
            data: RecordBody::Config {
                source: b"[package]\n".to_vec(),
            },
            todos: None,
        };
        let (head, tail) = config.encode();
        assert!(tail.is_empty());
        assert_eq!(ContentRecord::decode(&head, &tail).unwrap(), config);

        for cut in 0..body.len() {
            assert!(ContentRecord::decode(&surface, &body[..cut]).is_err());
        }
        for cut in 0..surface.len() {
            assert!(ContentRecord::decode(&surface[..cut], &body).is_err());
        }
    }

    /// A record of a kind written before symbols carried entry facts reads
    /// back with none, and the facts round-trip in the kinds that hold them.
    #[test]
    fn entry_facts_round_trip_and_older_records_read_without_them() {
        let mut record = sample();
        let RecordBody::Parsed { surface, .. } = &mut record.data else {
            unreachable!("the sample is parsed");
        };
        surface.symbols[0].program_start = true;
        let (head, tail) = record.encode();
        assert_eq!(ContentRecord::decode(&head, &tail).unwrap(), record);

        // The encoding records had before entry facts: the same surface and
        // body under the older kind byte, without the facts.
        let RecordBody::Parsed { surface, body } = &record.data else {
            unreachable!("the sample is parsed");
        };
        let mut old = Encoder::default();
        old.str(&record.language);
        old.str(&record.extractor_version);
        old.byte(PARSED);
        encode_surface(&mut old, surface);
        let mut old_tail = Encoder::default();
        encode_body(&mut old_tail, body);
        let decoded = ContentRecord::decode(&old.finish(), &old_tail.finish()).unwrap();
        let RecordBody::Parsed { surface, .. } = decoded.data else {
            unreachable!("the record is parsed");
        };
        assert!(!surface.symbols[0].program_start);
        assert_eq!(surface.symbols[0].entry_attribute, None);
        assert_eq!(surface.symbols[0].scoped_name, "Store::open");
    }
}
