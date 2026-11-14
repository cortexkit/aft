//! Pass 4 of a cold build, in two steps: each file's sites and
//! declarations projected in file order (see `dispatch::projected`), then
//! every site matched against the projected records alone.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use rayon::prelude::*;

use crate::callgraph_store::join::dispatch::projected::{
    FileRecord, ProjectedMro, ProjectedType, Projections, Recorded,
};
use crate::callgraph_store::join::dispatch::{
    dependency, DispatchFiles, MemoizedResolver, Resolver,
};
use crate::callgraph_store::join::{ManifestJoinError, ParseBlob};

use super::{
    CheckoutInputs, MethodCallInputs, NoResolutions, SiteResolution, SiteSource, SMALL_SITE_RUN,
};

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
    records: &'r dyn RecordStore,
    /// Method name to the files declaring it, in path order.
    methods: HashMap<String, Vec<String>>,
    /// Method name to the Go files whose interfaces require it.
    go_required: HashMap<String, Vec<String>>,
    error: Mutex<Option<String>>,
    #[cfg(feature = "callgraph-shape-census")]
    pub(super) census: Mutex<crate::data_layer::shape_census::Census>,
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
            #[cfg(feature = "callgraph-shape-census")]
            census: Mutex::default(),
        }
    }

    fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

impl Projections for RecordSource<'_> {
    fn projected_type(&self, file: &str, name: &str) -> Recorded<'_, ProjectedType> {
        self.records
            .get(file)
            .map_or(Recorded::Missing, |record| record.projected_type(name))
    }

    fn projected_mro(&self, file: &str, owner: &str) -> Recorded<'_, ProjectedMro> {
        self.records
            .get(file)
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
        self.records.get(path).map(FileRecord::facts)
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
        self.records
            .get(file)
            .is_some_and(|record| record.associated(ordinal))
    }

    fn projections(&'a self) -> Option<&'a (dyn Projections + 'a)> {
        Some(self)
    }
}

/// Pass 4 for `paths` over projected records: each file's kept method-call
/// sites and the dependency keys their resolutions read, as
/// `MethodCallInputs::resolve_sites` gives them. A few files run on this
/// thread; more run in parallel chunks, each with its own resolver.
///
/// # Errors
///
/// When a file has no record, or a record the resolver needed is missing.
pub(super) fn resolve_records(
    source: &RecordSource<'_>,
    paths: &[&String],
) -> Result<Vec<(String, Vec<SiteResolution>, BTreeSet<String>)>, ManifestJoinError> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let results = if paths.len() <= SMALL_SITE_RUN {
        let resolver = Resolver::over(source);
        resolve_chunk(source, &resolver, paths)
    } else {
        let chunk = paths
            .len()
            .div_ceil(rayon::current_num_threads() * 4)
            .max(1);
        let carry = crate::alloc_trace::carry();
        paths
            .par_chunks(chunk)
            .map(|chunk| {
                let _label = carry.enter();
                let resolver = Resolver::over(source);
                resolve_chunk(source, &resolver, chunk)
            })
            .collect::<Result<Vec<_>, _>>()
            .map(|chunks| chunks.into_iter().flatten().collect())
    }?;
    if let Some(error) = source.take_error() {
        return Err(ManifestJoinError::Parse(error));
    }
    Ok(results)
}

fn resolve_chunk(
    source: &RecordSource<'_>,
    resolver: &Resolver<'_>,
    paths: &[&String],
) -> Result<Vec<(String, Vec<SiteResolution>, BTreeSet<String>)>, ManifestJoinError> {
    let memoized = MemoizedResolver::new(resolver);
    resolver.take_dependencies();
    #[cfg(feature = "callgraph-shape-census")]
    let mut census = crate::data_layer::shape_census::Census::default();
    let result = paths
        .iter()
        .map(|path| {
            let record = source.records.get(path).ok_or_else(|| {
                ManifestJoinError::Parse(format!("missing method-call record for {path}"))
            })?;
            let language = record.facts().language.as_str();
            let mut sites = Vec::new();
            for site in record.sites() {
                let resolution = memoized.resolve(path, &site.site);
                // This runs for memo hits too, and before Rust's ordinary-edge
                // preservation can omit a supplemental method-call row.
                #[cfg(feature = "callgraph-shape-census")]
                census.record(
                    language,
                    &site.site,
                    resolver.census_exit.get(),
                    &resolution,
                );
                // Keep proven self-call and scoped module-function bindings
                // when dispatch facts do not identify a project receiver type.
                if language == "rust"
                    && resolution.external > 0
                    && resolution.targets.is_empty()
                    && site.keeps_ordinary_edge
                {
                    continue;
                }
                sites.push(SiteResolution {
                    site: site.site.clone(),
                    resolution: (*resolution).clone(),
                });
            }
            // Every file depends on its own facts, which `Resolver::resolve`
            // reads without recording. Taken after every site, those left
            // out above included.
            let mut dependencies = resolver.take_dependencies();
            dependencies.insert(dependency::file(path));
            Ok(((*path).clone(), sites, dependencies))
        })
        .collect();
    #[cfg(feature = "callgraph-shape-census")]
    source
        .census
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .merge(census);
    result
}
