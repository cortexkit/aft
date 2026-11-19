//! The symbol facts that a reference into another file can read.

use std::collections::{BTreeMap, HashMap};

use super::rows::TargetNodes;
use crate::data_layer::graph_rows::Nodes;

#[derive(Default)]
pub(super) struct Targets(BTreeMap<String, FileTargets>);

type Lookup = Box<[(Box<str>, Box<str>)]>;
type Names = Box<[(Box<str>, Box<str>, Box<str>)]>;

struct FileTargets {
    ordinary: Lookup,
    scoped: Lookup,
    names: Names,
}

impl Targets {
    pub(super) fn insert(&mut self, path: String, nodes: Nodes, lookup: HashMap<String, String>) {
        let mut ordinary = lookup
            .into_iter()
            .map(|(symbol, id)| (symbol.into_boxed_str(), id.into_boxed_str()))
            .collect::<Vec<_>>();
        ordinary.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        let mut scoped = nodes
            .by_scoped_name()
            .into_iter()
            .map(|((_, symbol), id)| (Box::<str>::from(symbol), Box::<str>::from(id)))
            .collect::<Vec<_>>();
        scoped.sort_unstable_by(|(left, _), (right, _)| left.cmp(right));
        let names = nodes
            .rows
            .into_iter()
            .map(|(id, (_, node))| {
                (
                    id.into_boxed_str(),
                    node.name.into_boxed_str(),
                    node.kind.into_boxed_str(),
                )
            })
            .collect();
        self.0.insert(
            path,
            FileTargets {
                ordinary: ordinary.into_boxed_slice(),
                scoped: scoped.into_boxed_slice(),
                names,
            },
        );
    }
}

fn node_id<'a>(table: &'a Lookup, symbol: &str) -> Option<&'a str> {
    let index = table
        .binary_search_by(|(key, _)| key.as_ref().cmp(symbol))
        .ok()?;
    Some(&table[index].1)
}

impl TargetNodes for Targets {
    fn lookup(&self, file: &str, symbol: &str) -> Option<&str> {
        node_id(&self.0.get(file)?.ordinary, symbol)
    }

    fn scoped(&self, file: &str, symbol: &str) -> Option<&str> {
        node_id(&self.0.get(file)?.scoped, symbol)
    }

    fn node(&self, file: &str, id: &str) -> Option<(&str, &str)> {
        let table = &self.0.get(file)?.names;
        let index = table
            .binary_search_by(|(key, _, _)| key.as_ref().cmp(id))
            .ok()?;
        let (_, name, kind) = &table[index];
        Some((name, kind))
    }
}
