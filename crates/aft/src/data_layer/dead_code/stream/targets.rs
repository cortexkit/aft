//! The symbol facts that a reference into another file can read.

use parking_lot::Mutex;

use super::cache::TargetCache;
use super::rows::{file_nodes, TargetNodes};
use crate::data_layer::checkout::ViewGraphFiles;
use crate::data_layer::dead_code_graph::CallGraphFiles;

pub(super) struct Targets<'a> {
    source: ViewGraphFiles<'a>,
    cache: Mutex<TargetCache<FileTargets>>,
    failure: Mutex<Option<String>>,
}

type Lookup = Box<[(Box<str>, Box<str>)]>;
type Names = Box<[(Box<str>, Box<str>, Box<str>)]>;

struct FileTargets {
    ordinary: Lookup,
    scoped: Lookup,
    names: Names,
}

impl FileTargets {
    fn build(path: &str, parse: &crate::callgraph_store::join::ParseBlob) -> Self {
        let (nodes, lookup) = file_nodes(path, parse);
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
        Self {
            ordinary: ordinary.into_boxed_slice(),
            scoped: scoped.into_boxed_slice(),
            names,
        }
    }

    fn heap_bytes(&self) -> usize {
        let table_bytes = |table: &Lookup| {
            std::mem::size_of_val(&**table)
                + table.iter().map(|(a, b)| a.len() + b.len()).sum::<usize>()
        };
        table_bytes(&self.ordinary)
            + table_bytes(&self.scoped)
            + std::mem::size_of_val(&*self.names)
            + self
                .names
                .iter()
                .map(|(a, b, c)| a.len() + b.len() + c.len())
                .sum::<usize>()
    }

    fn node(&self, id: &str) -> Option<(&str, &str)> {
        let index = self
            .names
            .binary_search_by(|(key, _, _)| key.as_ref().cmp(id))
            .ok()?;
        let (_, name, kind) = &self.names[index];
        Some((name, kind))
    }
}

impl<'a> Targets<'a> {
    pub(super) fn new(source: ViewGraphFiles<'a>) -> Self {
        Self {
            source,
            cache: Mutex::default(),
            failure: Mutex::default(),
        }
    }

    fn read<R>(&self, path: &str, use_value: impl FnOnce(&FileTargets) -> R) -> Option<R> {
        self.cache.lock().read(
            path,
            || {
                if !self.source.has_file(path) {
                    return None;
                }
                let parse = match self.source.parse(path) {
                    Ok(parse) => parse,
                    Err(error) => {
                        self.failure.lock().get_or_insert(error);
                        return None;
                    }
                };
                let index = FileTargets::build(path, &parse);
                Some((index.heap_bytes(), index))
            },
            use_value,
        )
    }

    pub(super) fn check(&self) -> Result<(), String> {
        self.failure.lock().clone().map_or(Ok(()), Err)
    }

    pub(super) fn held(&self) -> (usize, usize) {
        self.cache.lock().held()
    }
}

fn node_id<'a>(table: &'a Lookup, symbol: &str) -> Option<&'a str> {
    let index = table
        .binary_search_by(|(key, _)| key.as_ref().cmp(symbol))
        .ok()?;
    Some(&table[index].1)
}

impl TargetNodes for Targets<'_> {
    fn lookup(&self, file: &str, symbol: &str) -> Option<String> {
        self.read(file, |index| {
            node_id(&index.ordinary, symbol).map(str::to_owned)
        })
        .flatten()
    }

    fn scoped(&self, file: &str, symbol: &str) -> Option<String> {
        self.read(file, |index| {
            node_id(&index.scoped, symbol).map(str::to_owned)
        })
        .flatten()
    }

    fn name(&self, file: &str, id: &str) -> Option<String> {
        self.read(file, |index| {
            index.node(id).map(|(name, _)| name.to_owned())
        })
        .flatten()
    }

    fn is_constructor(&self, file: &str, id: &str) -> bool {
        self.read(file, |index| {
            index
                .node(id)
                .is_some_and(|(_, kind)| matches!(kind, "function" | "method"))
        })
        .unwrap_or(false)
    }
}
