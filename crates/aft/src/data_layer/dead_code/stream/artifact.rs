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
    out.write_all(b"{\"calls\":[")?;
    let mut calls = List::new(out);
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
    out.write_all(b"],\"entry_point_symbols\":")?;
    serde_json::to_writer(&mut *out, &serde_json::to_value(roots)?)?;
    out.write_all(b",\"entry_points\":")?;
    serde_json::to_writer(&mut *out, &snapshot.entry_points)?;
    out.write_all(b",\"exports\":[")?;
    let mut exports = List::new(out);
    for (_, path) in rows.paths() {
        for export in rows.exports(path)? {
            exports.item(export)?;
        }
    }
    out.write_all(b"],\"files\":")?;
    serde_json::to_writer(&mut *out, &snapshot.files)?;
    out.write_all(b",\"go_initializers\":[")?;
    let mut groups = List::new(out);
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
    out.write_all(b"]}")?;
    Ok(())
}

struct List<'a, W> {
    out: &'a mut W,
    first: bool,
}

impl<'a, W: Write> List<'a, W> {
    fn new(out: &'a mut W) -> Self {
        Self { out, first: true }
    }

    fn item(&mut self, item: impl Serialize) -> Result<()> {
        if !std::mem::take(&mut self.first) {
            self.out.write_all(b",")?;
        }
        // Sort object keys as in the gate's JSON artifact format.
        serde_json::to_writer(&mut *self.out, &serde_json::to_value(item)?)?;
        Ok(())
    }
}
