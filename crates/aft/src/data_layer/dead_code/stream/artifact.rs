//! Write the exactness artifact in its original JSON order without building
//! a checkout-wide JSON value or retaining the call rows between files.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufWriter, Write};
use std::path::Path;

use serde::Serialize;
use serde_json::json;

use super::Rows;
use crate::data_layer::dead_code_snapshot::CallgraphSnapshot;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(crate) fn write(path: &Path, rows: &Rows<'_>, snapshot: &CallgraphSnapshot) -> Result<()> {
    let mut out = BufWriter::new(std::fs::File::create(path)?);
    write_rows(&mut out, rows, snapshot)?;
    out.flush()?;
    Ok(())
}

fn write_rows(out: &mut impl Write, rows: &Rows<'_>, snapshot: &CallgraphSnapshot) -> Result<()> {
    let mut roots = BTreeMap::<_, BTreeSet<_>>::new();
    // Match the object order of the non-streaming JSON Value even if another
    // crate enables serde_json's preserve_order feature through Cargo.
    let keys = json!({"files": null, "exports": null, "calls": null, "entry_points": null,
        "entry_point_symbols": null, "go_initializers": null});
    out.write_all(b"{")?;
    for (index, key) in keys.as_object().unwrap().keys().enumerate() {
        if index != 0 {
            out.write_all(b",")?;
        }
        serde_json::to_writer(&mut *out, key)?;
        out.write_all(b":")?;
        match key.as_str() {
            "files" => serde_json::to_writer(&mut *out, &snapshot.files)?,
            "entry_points" => serde_json::to_writer(&mut *out, &snapshot.entry_points)?,
            "entry_point_symbols" => {
                serde_json::to_writer(&mut *out, &serde_json::to_value(&roots)?)?
            }
            "exports" => {
                let mut exports = List::start(out)?;
                for (_, path) in rows.paths() {
                    for export in rows.exports(path)? {
                        exports.item(export)?;
                    }
                }
                exports.end()?;
            }
            "calls" => {
                let mut calls = List::start(out)?;
                rows.compare(snapshot, |file, facts| -> Result<()> {
                    for call in facts.calls {
                        calls.item(call)?;
                    }
                    if !facts.roots.is_empty() {
                        roots
                            .entry(file.to_path_buf())
                            .or_default()
                            .extend(facts.roots);
                    }
                    Ok(())
                })?;
                calls.end()?;
            }
            "go_initializers" => {
                let mut groups = List::start(out)?;
                // Unlike calls and exports, initializer groups are sorted by store path.
                for path in rows.go_paths() {
                    if let Some(group) = rows.file(path)?.go {
                        groups.item(json!({
                            "caller_file": group.caller_file,
                            "callers": group.callers,
                            "targets": group.targets.iter().map(|(file, symbol)| (file.as_ref(), symbol.as_ref())).collect::<Vec<_>>(),
                        }))?;
                    }
                }
                groups.end()?;
            }
            _ => unreachable!("the artifact keys are defined above"),
        }
    }
    out.write_all(b"}")?;
    Ok(())
}

struct List<'a, W> {
    out: &'a mut W,
    first: bool,
}

impl<'a, W: Write> List<'a, W> {
    fn start(out: &'a mut W) -> Result<Self> {
        out.write_all(b"[")?;
        Ok(Self { out, first: true })
    }

    fn end(self) -> Result<()> {
        self.out.write_all(b"]")?;
        Ok(())
    }

    fn item(&mut self, item: impl Serialize) -> Result<()> {
        if !std::mem::take(&mut self.first) {
            self.out.write_all(b",")?;
        }
        // Match serde_json::Value's object order instead of serializing
        // directly in Rust struct field order.
        serde_json::to_writer(&mut *self.out, &serde_json::to_value(item)?)?;
        Ok(())
    }
}
