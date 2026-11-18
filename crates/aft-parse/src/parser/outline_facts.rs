//! Syntax facts shared by the parser cache and outline renderer.

use crate::symbols::{Range, Symbol};
use serde::Serialize;

/// A single entry in the outline tree.
///
/// Top-level symbols have an empty `members` vec. Classes/structs contain
/// their methods and nested types in `members`, forming a recursive tree.
#[derive(Debug, Clone, Serialize)]
pub struct OutlineEntry {
    pub name: String,
    pub kind: String,
    pub range: Range,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub exported: bool,
    pub members: Vec<OutlineEntry>,
}

pub fn symbol_to_entry(sym: &Symbol) -> OutlineEntry {
    OutlineEntry {
        name: sym.name.clone(),
        kind: serde_json::to_value(&sym.kind)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{:?}", sym.kind).to_lowercase()),
        range: sym.range.clone(),
        signature: sym.signature.clone(),
        exported: sym.exported,
        members: Vec::new(),
    }
}

#[derive(Clone)]
pub struct TestSummary {
    pub name: String,
    pub range: Range,
    pub items: usize,
    pub unknown_reason: Option<&'static str>,
}

#[derive(Clone)]
pub struct TestRegion {
    pub summary: TestSummary,
    pub included_path: Option<String>,
    pub implicit_include: bool,
    pub module_scope: Vec<String>,
    pub standalone: bool,
}

/// What the multi-file structure map needs from one parse of a file beyond
/// its symbols: the test regions it collapses and, for Rust, the methods
/// declared in each trait it previews. Both depend only on the file's own
/// text, so they are stored next to the symbols in the shared symbol cache
/// under the same freshness key, and a warm outline renders the file without
/// reading or parsing it. Test includes are resolved at render time, because
/// they depend on other files and on the caller's read policy.
pub struct OutlineParseFacts {
    pub test_regions: Vec<TestRegion>,
    pub trait_previews: Vec<(Range, Vec<OutlineEntry>)>,
}

impl OutlineParseFacts {
    /// Rough resident size, for the symbol cache's memory estimate.
    pub fn estimated_bytes(&self) -> u64 {
        fn entry_bytes(entry: &OutlineEntry) -> u64 {
            (std::mem::size_of::<OutlineEntry>()
                + entry.name.len()
                + entry.kind.len()
                + entry.signature.as_ref().map_or(0, String::len)) as u64
                + entry.members.iter().map(entry_bytes).sum::<u64>()
        }
        let regions = self
            .test_regions
            .iter()
            .map(|region| {
                (std::mem::size_of::<TestRegion>()
                    + region.summary.name.len()
                    + region.included_path.as_ref().map_or(0, String::len)
                    + region.module_scope.iter().map(String::len).sum::<usize>())
                    as u64
            })
            .sum::<u64>();
        let previews = self
            .trait_previews
            .iter()
            .map(|(_, methods)| {
                std::mem::size_of::<(Range, Vec<OutlineEntry>)>() as u64
                    + methods.iter().map(entry_bytes).sum::<u64>()
            })
            .sum::<u64>();
        std::mem::size_of::<Self>() as u64 + regions + previews
    }
}
