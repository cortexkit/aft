//! Resolve checkout-backed calls using object declarations and explicit factory
//! returns. The immutable-blob graph resolver supplies the same binding rules;
//! its object types and return information remain temporary, not new SQL tables.
use super::*;

pub(super) type FactsCache = HashMap<String, Option<Arc<join::ParseBlob>>>;

pub(super) fn facts_weight(parse: &join::ParseBlob) -> usize {
    let strings = |values: &[String]| {
        values
            .iter()
            .map(|s| std::mem::size_of::<String>() + s.capacity())
            .sum::<usize>()
    };
    96 + std::mem::size_of::<join::ParseBlob>()
        + parse
            .symbols
            .iter()
            .map(|s| {
                std::mem::size_of::<join::BlobSymbol>()
                    + s.name.capacity()
                    + s.scoped_name.capacity()
                    + s.signature.as_ref().map_or(0, String::capacity)
            })
            .sum::<usize>()
        + parse
            .imports
            .iter()
            .map(|i| {
                std::mem::size_of::<join::BlobImport>()
                    + i.module_path.capacity()
                    + i.raw_text.capacity()
                    + strings(&i.names)
                    + i.default_import.as_ref().map_or(0, String::capacity)
                    + i.namespace_import.as_ref().map_or(0, String::capacity)
            })
            .sum::<usize>()
        + parse
            .refs
            .iter()
            .map(|r| {
                std::mem::size_of::<join::BlobRef>()
                    + r.caller_symbol.as_ref().map_or(0, String::capacity)
                    + r.short_name.as_ref().map_or(0, String::capacity)
                    + r.full_ref.as_ref().map_or(0, String::capacity)
            })
            .sum::<usize>()
        + parse
            .dispatch
            .types
            .iter()
            .map(|t| {
                std::mem::size_of::<join::dispatch::TypeHint>()
                    + t.name.capacity()
                    + strings(&t.bases)
            })
            .sum::<usize>()
        + parse
            .dispatch
            .methods
            .iter()
            .map(|m| {
                std::mem::size_of::<join::dispatch::MethodHint>()
                    + m.owner.capacity()
                    + m.name.capacity()
                    + m.symbol.capacity()
                    + m.shape.capacity()
            })
            .sum::<usize>()
        + parse
            .dispatch
            .fields
            .iter()
            .map(|f| {
                std::mem::size_of::<join::dispatch::FieldHint>()
                    + f.owner.capacity()
                    + f.name.capacity()
                    + f.ty.capacity()
            })
            .sum::<usize>()
        + parse
            .dispatch
            .returns
            .iter()
            .map(|r| {
                std::mem::size_of::<join::dispatch::ReturnHint>()
                    + r.symbol.capacity()
                    + r.ty.capacity()
            })
            .sum::<usize>()
        + parse
            .dispatch
            .sites
            .iter()
            .map(|s| {
                std::mem::size_of::<join::dispatch::SiteHint>()
                    + s.caller.as_ref().map_or(0, String::capacity)
                    + s.member.as_ref().map_or(0, String::capacity)
                    + s.receiver.as_ref().map_or(0, String::capacity)
            })
            .sum::<usize>()
        + strings(&parse.exported_symbols)
}

pub(super) struct ObjectDispatch<'a> {
    root: PathBuf,
    index: ProjectIndex<'a>,
    parses: BTreeMap<String, Arc<join::ParseBlob>>,
    targets: BTreeMap<(String, String), (String, String)>,
    resolutions: HashMap<String, HashMap<usize, join::dispatch::Target>>,
    cached: Option<&'a FactsCache>,
    loaded: HashSet<String>,
    cached_files: usize,
    source_extracts: usize,
}

impl<'a> ObjectDispatch<'a> {
    pub(super) fn new(
        connection: &'a Connection,
        root: &Path,
        cached: Option<&'a FactsCache>,
    ) -> Self {
        let mut index = ProjectIndex::from_parts(
            root,
            HashMap::new(),
            HashMap::new(),
            WorkspaceCratePrefixCache::default(),
            Rc::new(DiskFacts::new(root)),
        );
        // The pass below also needs the stored loader's module resolution,
        // so the same loader goes in both fields.
        let lazy = Rc::new(LazyDbFileIndexes::new(connection, root, HashSet::new()));
        index.lazy = Some(Rc::clone(&lazy) as Rc<dyn super::LoadedFileIndexes + 'a>);
        index.stored_lazy = Some(lazy);
        Self {
            root: root.to_path_buf(),
            index,
            parses: BTreeMap::new(),
            targets: BTreeMap::new(),
            resolutions: HashMap::new(),
            cached,
            loaded: HashSet::new(),
            cached_files: 0,
            source_extracts: 0,
        }
    }

    fn load(&mut self, file: &str) -> Result<()> {
        if !self.loaded.insert(file.to_string()) {
            return Ok(());
        }
        let parse = if let Some(cached) = self.cached.and_then(|cache| cache.get(file)) {
            self.cached_files += 1;
            let Some(parse) = cached else { return Ok(()) };
            Arc::clone(parse)
        } else {
            let Some(LangId::TypeScript | LangId::Tsx) = self.index.lang_for(file) else {
                return Ok(());
            };
            let source = crate::project_content::read_to_string(self.root.join(file))?;
            self.source_extracts += 1;
            let (_, objects) =
                callgraph::build_file_data_with_typescript_objects(&self.root.join(file), &source)?;
            let Some(parse) = objects else {
                return Ok(());
            };
            Arc::new(parse)
        };
        let data = parse
            .file_data(file)
            .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
        // The lazy index may not have loaded imports for files using cached
        // object facts. Install only imports naming functions whose returned
        // objects are used as receivers.
        if let Some(mut file_index) = self.index.file_index(file).map(|index| (*index).clone()) {
            let lazy = self.index.stored_lazy.as_ref().unwrap();
            let facts = lazy.fact_paths();
            let needed = join::dispatch_type_names(&parse);
            for import in parse.imports.iter().filter(|import| {
                import
                    .default_import
                    .as_ref()
                    .is_some_and(|name| needed.contains(name))
                    || import
                        .names
                        .iter()
                        .any(|name| needed.contains(crate::imports::specifier_local_name(name)))
            }) {
                file_index.module_targets.insert(
                    import.module_path.clone(),
                    module_ref_target(
                        &self.root,
                        file,
                        "import",
                        &import.module_path,
                        &lazy.module_resolution_memo,
                        &facts,
                        &|path: &str| lazy.contains_file(path),
                    ),
                );
            }
            Arc::make_mut(&mut self.index.files).insert(file.to_string(), file_index);
        }
        let targets = join::resolve_dispatch_type_names(&self.index, file, &parse, &data);
        if let Some(error) = self.index.take_lazy_load_error() {
            return Err(error.into());
        }
        self.targets.extend(
            targets
                .iter()
                .map(|(name, target)| ((file.to_string(), name.clone()), target.clone())),
        );
        self.parses.insert(file.to_string(), parse);
        for (target_file, _) in targets.values() {
            if target_file != file {
                self.load(target_file)?;
            }
        }
        Ok(())
    }

    pub(super) fn prepare(&mut self, references: &[NameMatchRef]) -> Result<()> {
        let mut profile = crate::stage_timer::StageTimer::new("typescript_objects");
        let callers = references
            .iter()
            .filter(|r| matches!(r.lang.as_str(), "typescript" | "tsx"))
            .map(|r| r.caller_file.clone())
            .collect::<BTreeSet<_>>();
        for file in &callers {
            self.load(file)?;
        }
        profile.finish(
            "load",
            format_args!(
                "callers={} parses={} cached={} source_extracts={}",
                callers.len(),
                self.parses.len(),
                self.cached_files,
                self.source_extracts
            ),
        );
        let mut resolver = join::dispatch::Resolver::new(
            self.parses
                .iter()
                .map(|(path, parse)| (path.clone(), parse.as_ref()))
                .collect(),
        );
        resolver.type_targets = self.targets.clone();
        profile.finish("resolver", format_args!("parses={}", self.parses.len()));
        for file in callers {
            let Some(parse) = self.parses.get(&file) else {
                continue;
            };
            let starts = parse
                .refs
                .iter()
                .filter(|r| r.kind == join::BlobRefKind::Call)
                .map(|r| (r.ordinal, r.byte_start))
                .collect::<HashMap<_, _>>();
            let mut resolutions = HashMap::new();
            for site in &parse.dispatch.sites {
                if !site.receiver.as_ref().is_some_and(|r| {
                    r.starts_with("@object:")
                        || r.starts_with("@return:")
                        || r.starts_with("@field:")
                }) {
                    continue;
                }
                let resolution = resolver.resolve(&file, site);
                if resolution.targets.len() == 1 {
                    if let Some(&start) = starts.get(&site.ordinal) {
                        resolutions.insert(start, resolution.targets.into_iter().next().unwrap());
                    }
                }
            }
            // Only files with an object resolution get an entry, so `resolve`
            // answers every other file without touching the store.
            if !resolutions.is_empty() {
                self.resolutions.insert(file, resolutions);
            }
        }
        profile.finish("resolve", format_args!("files={}", self.resolutions.len()));
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn source_extracts(&self) -> usize {
        self.source_extracts
    }

    pub(super) fn resolve(
        &self,
        connection: &Connection,
        reference: &NameMatchRef,
    ) -> Result<Option<NameMatchCandidate>> {
        // Most unresolved members come from files with no object resolutions;
        // answer those without touching the store. The statement is cached so
        // a build prepares it once, not once per member.
        let Some(sites) = self.resolutions.get(&reference.caller_file) else {
            return Ok(None);
        };
        let start: usize = connection
            .prepare_cached("SELECT byte_start FROM refs WHERE ref_id=?1")?
            .query_row([&reference.ref_id], |r| r.get(0))?;
        let Some(target) = sites.get(&start) else {
            return Ok(None);
        };
        let Some(node_id) = self.index.node_for_symbol(&target.file, &target.symbol) else {
            return Ok(None);
        };
        Ok(Some(NameMatchCandidate {
            node_id,
            file_path: target.file.clone(),
            scoped_name: target.symbol.clone(),
            kind: "method".into(),
            start_line: 0,
        }))
    }
}
