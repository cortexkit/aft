//! Pass 4 of a cold build, in two steps: each file's sites and
//! declarations projected in file order (see `dispatch::projected`), then
//! every site matched against the projected records alone (see
//! `key_order`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use rayon::prelude::*;

use crate::callgraph_store::join::dispatch::projected::{
    FileRecord, ProjectedMro, ProjectedType, Projections, Recorded,
};
use crate::callgraph_store::join::dispatch::{DispatchFiles, Resolver};
use crate::callgraph_store::join::{ManifestJoinError, ParseBlob};

use super::{CheckoutInputs, MethodCallInputs, NoResolutions, SiteSource};

/// Where the projected records are kept between the two steps. Every
/// record is written once, in path order, and read by path.
pub(crate) trait RecordStore: Sync {
    fn insert(&mut self, path: String, record: FileRecord);
    fn get(&self, path: &str) -> Option<&FileRecord>;
    /// Every path from `start` on, in path order.
    fn paths_from<'s>(&'s self, start: &str) -> Box<dyn Iterator<Item = &'s str> + 's>;
}

/// Records held in memory.
#[derive(Default)]
pub(crate) struct MemoryRecords(std::collections::BTreeMap<String, FileRecord>);

impl RecordStore for MemoryRecords {
    fn insert(&mut self, path: String, record: FileRecord) {
        self.0.insert(path, record);
    }

    fn get(&self, path: &str) -> Option<&FileRecord> {
        self.0.get(path)
    }

    fn paths_from<'s>(&'s self, start: &str) -> Box<dyn Iterator<Item = &'s str> + 's> {
        Box::new(
            self.0
                .range::<str, _>((std::ops::Bound::Included(start), std::ops::Bound::Unbounded))
                .map(|(path, _)| path.as_str()),
        )
    }
}

impl MemoryRecords {
    /// What the records hold, as named measurements: their heap, the heap
    /// of the projected answers alone, how many answers there are, and how
    /// many of those answer a Rust file's resolvable names only.
    pub(crate) fn census(&self) -> [(&'static str, usize); 4] {
        use aft_store::heap::HeapBytes as _;
        let mut seen = aft_store::heap::Seen::new();
        let total = self.0.heap_bytes(&mut seen);
        let mut seen = aft_store::heap::Seen::new();
        let (mut answers, mut rust_names, mut bytes) = (0, 0, 0);
        for record in self.0.values() {
            let (all, rust, heap) = record.projection_census(&mut seen);
            answers += all;
            rust_names += rust;
            bytes += heap;
        }
        [
            ("resolve.site_records", total),
            ("resolve.projection_bytes", bytes),
            ("resolve.projected_answers", answers),
            ("resolve.rust_name_answers", rust_names),
        ]
    }
}

/// Projects the files of `batch`, whose method-call inputs `calls` keeps,
/// into `records`, in parallel chunks, each with its own resolver.
///
/// # Errors
///
/// When a file has no parse or no inputs, or reading another file's
/// inputs failed.
pub(super) fn project_batch(
    inputs: &CheckoutInputs,
    calls: &MethodCallInputs,
    batch: &[&String],
    records: &mut dyn RecordStore,
) -> Result<(), ManifestJoinError> {
    let source = SiteSource::kept(inputs, &NoResolutions, calls);
    let chunk = batch
        .len()
        .div_ceil(rayon::current_num_threads() * 4)
        .max(1);
    let carry = crate::alloc_trace::carry();
    let projected = batch
        .par_chunks(chunk)
        .map(|chunk| {
            let _label = carry.enter();
            let resolver = Resolver::over(&source);
            chunk
                .iter()
                .map(|path| {
                    let (_, parse) = inputs.parses.get(path).ok_or_else(|| {
                        ManifestJoinError::Parse(format!("missing parse for {path}"))
                    })?;
                    let site = source.site(path).ok_or_else(|| {
                        ManifestJoinError::Parse(format!("missing method-call inputs for {path}"))
                    })?;
                    let ordinary = &site.ordinary;
                    let record = resolver
                        .project_file(
                            path,
                            parse.bound_dispatch_sites(),
                            &mut site.facts.type_targets.keys().map(String::as_str),
                            &|ordinal| ordinary.associated.contains(&ordinal),
                            &|ordinal| {
                                ordinary.edges.contains(&ordinal)
                                    && ordinary.syntax.contains(&ordinal)
                            },
                        )
                        .ok_or_else(|| {
                            ManifestJoinError::Parse(format!(
                                "missing method-call facts for {path}"
                            ))
                        })?;
                    Ok(((*path).clone(), record))
                })
                .collect::<Result<Vec<_>, ManifestJoinError>>()
        })
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(error) = source
        .error
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    {
        return Err(ManifestJoinError::Parse(error));
    }
    for (path, record) in projected.into_iter().flatten() {
        records.insert(path, record);
    }
    Ok(())
}

/// The method-call resolver's view of the projected records: each file's
/// declarations, found by method name as the in-memory inputs file them,
/// and the projections recorded for it.
pub(super) struct RecordSource<'r> {
    pub(super) records: &'r dyn RecordStore,
    /// Method name to the files declaring it, in path order.
    methods: HashMap<String, Vec<String>>,
    /// Method name to the Go files whose interfaces require it.
    go_required: HashMap<String, Vec<String>>,
    error: Mutex<Option<String>>,
    /// Whether each record lookup is counted for the thread's
    /// [`Touches`], for measurement.
    count_touches: bool,
    #[cfg(feature = "callgraph-shape-census")]
    pub(super) census: Mutex<crate::data_layer::shape_census::Census>,
}

/// The record lookups one thread made while counting (see
/// [`RecordSource::counting_touches`]): how many, and of how many files.
#[derive(Debug, Default)]
pub(super) struct Touches {
    pub(super) lookups: u64,
    /// The records looked up, by address: each file has one record.
    pub(super) files: HashSet<usize>,
}

impl Touches {
    pub(super) fn merge(&mut self, other: Touches) {
        self.lookups += other.lookups;
        self.files.extend(other.files);
    }
}

thread_local! {
    /// What this thread's record lookups touched since [`start_touches`].
    static TOUCHES: RefCell<Option<Touches>> = const { RefCell::new(None) };
}

/// Starts counting this thread's record lookups, when the source counts
/// them at all.
pub(super) fn start_touches() {
    TOUCHES.with(|touches| *touches.borrow_mut() = Some(Touches::default()));
}

/// What this thread's record lookups touched since [`start_touches`], and
/// stops counting.
pub(super) fn take_touches() -> Touches {
    TOUCHES.with(|touches| touches.borrow_mut().take().unwrap_or_default())
}

impl<'r> RecordSource<'r> {
    pub(super) fn new(records: &'r dyn RecordStore) -> Self {
        let mut methods = HashMap::<String, Vec<String>>::new();
        let mut go_required = HashMap::<String, Vec<String>>::new();
        for path in records.paths_from("") {
            let Some(record) = records.get(path) else {
                continue;
            };
            for name in record.method_names() {
                methods
                    .entry(name.to_string())
                    .or_default()
                    .push(path.to_string());
            }
            for name in record.go_required() {
                go_required
                    .entry(name.to_string())
                    .or_default()
                    .push(path.to_string());
            }
        }
        Self {
            records,
            methods,
            go_required,
            error: Mutex::new(None),
            count_touches: false,
            #[cfg(feature = "callgraph-shape-census")]
            census: Mutex::default(),
        }
    }

    /// Counts every record lookup for the looking thread's [`Touches`].
    pub(super) fn counting_touches(mut self) -> Self {
        self.count_touches = true;
        self
    }

    /// `path`'s record, counted for the thread's [`Touches`] when counting.
    pub(super) fn record(&self, path: &str) -> Option<&'r FileRecord> {
        let record = self.records.get(path);
        if self.count_touches {
            TOUCHES.with(|touches| {
                if let Some(touches) = touches.borrow_mut().as_mut() {
                    touches.lookups += 1;
                    if let Some(record) = record {
                        touches.files.insert(std::ptr::from_ref(record) as usize);
                    }
                }
            });
        }
        record
    }

    /// How many files declare a method named `member`.
    pub(super) fn declarers(&self, member: &str) -> usize {
        self.methods.get(member).map_or(0, Vec::len)
    }

    pub(super) fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

impl Projections for RecordSource<'_> {
    fn projected_type(&self, file: &str, name: &str) -> Recorded<'_, ProjectedType> {
        self.record(file)
            .map_or(Recorded::Missing, |record| record.projected_type(name))
    }

    fn projected_mro(&self, file: &str, owner: &str) -> Recorded<'_, ProjectedMro> {
        self.record(file)
            .map_or(Recorded::Missing, |record| record.projected_mro(owner))
    }

    fn missing(&self, what: String) {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get_or_insert_with(|| format!("pass 4 found no record of {what}"));
    }
}

impl<'a, 'r: 'a> DispatchFiles<'a> for RecordSource<'r> {
    fn file(&'a self, path: &str) -> Option<&'a ParseBlob> {
        self.record(path).map(FileRecord::facts)
    }

    fn files_with_method(&self, name: &str) -> Vec<String> {
        self.methods.get(name).cloned().unwrap_or_default()
    }

    fn files_in(&self, directory: &str) -> Vec<String> {
        let prefix = if directory.is_empty() {
            String::new()
        } else {
            format!("{directory}/")
        };
        self.records
            .paths_from(&prefix)
            .take_while(|path| path.starts_with(&prefix))
            .filter(|path| !path[prefix.len()..].contains('/'))
            .map(str::to_string)
            .collect()
    }

    fn go_files_requiring(&self, name: &str) -> Vec<String> {
        self.go_required.get(name).cloned().unwrap_or_default()
    }

    // Import and type targets are read only while projecting a type, which
    // the records answer instead.
    fn import_target(&self, file: &str, module: &str) -> Option<String> {
        self.missing(format!("the import target of {module} in {file}"));
        None
    }

    fn type_target(&self, file: &str, name: &str) -> Option<(String, String)> {
        self.missing(format!("the type target of {name} in {file}"));
        None
    }

    fn associated_site(&self, file: &str, ordinal: u32) -> bool {
        self.record(file)
            .is_some_and(|record| record.associated(ordinal))
    }

    fn projections(&'a self) -> Option<&'a (dyn Projections + 'a)> {
        Some(self)
    }
}
