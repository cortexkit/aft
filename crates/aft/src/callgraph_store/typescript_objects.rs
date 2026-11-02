//! Resolve checkout-backed calls using object declarations and explicit factory
//! returns. The immutable-blob graph resolver supplies the same binding rules;
//! its object types and return information remain temporary, not new SQL tables.
use super::*;

pub(super) struct ObjectDispatch<'a> {
    root: PathBuf,
    index: ProjectIndex<'a>,
    parses: BTreeMap<String, join::ParseBlob>,
    targets: BTreeMap<(String, String), (String, String)>,
}

impl<'a> ObjectDispatch<'a> {
    pub(super) fn new(connection: &'a Connection, root: &Path) -> Self {
        let mut index = ProjectIndex::from_parts(
            root,
            HashMap::new(),
            HashMap::new(),
            WorkspaceCratePrefixCache::default(),
            Rc::new(DiskFacts::new(root)),
        );
        index.lazy = Some(Rc::new(LazyDbFileIndexes::new(
            connection,
            root,
            HashSet::new(),
        )));
        Self {
            root: root.to_path_buf(),
            index,
            parses: BTreeMap::new(),
            targets: BTreeMap::new(),
        }
    }

    fn load(&mut self, file: &str) -> Result<()> {
        if self.parses.contains_key(file) {
            return Ok(());
        }
        let Some(language @ (LangId::TypeScript | LangId::Tsx)) = self.index.lang_for(file) else {
            return Ok(());
        };
        let source = crate::project_content::read_to_string(self.root.join(file))?;
        let join::CallgraphBlob::Parse(parse) =
            join::CallgraphBlob::extract(&source, lang_label(language), "object-dispatch")
                .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?
        else {
            return Ok(());
        };
        let data = parse
            .file_data(file)
            .map_err(|error| CallGraphStoreError::Unavailable(error.to_string()))?;
        // The lazy index normally loads imports only for re-extracted callers.
        // This pass reparses callers independently, so install their import
        // module targets before asking the ordinary export resolver to bind a
        // factory name.
        if let Some(mut file_index) = self.index.file_index(file).map(|index| (*index).clone()) {
            let lazy = self.index.lazy.as_ref().unwrap();
            let facts = lazy.fact_paths();
            for import in &parse.imports {
                file_index.module_targets.insert(
                    import.module_path.clone(),
                    module_ref_target(
                        &self.root,
                        file,
                        "import",
                        &import.module_path,
                        &lazy.module_resolution_memo,
                        &facts,
                        &|path| lazy.contains_file(path),
                    ),
                );
            }
            self.index.files.insert(file.to_string(), file_index);
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

    pub(super) fn resolve(
        &mut self,
        connection: &Connection,
        reference: &NameMatchRef,
    ) -> Result<Option<NameMatchCandidate>> {
        self.load(&reference.caller_file)?;
        let Some(parse) = self.parses.get(&reference.caller_file) else {
            return Ok(None);
        };
        let start: usize = connection.query_row(
            "SELECT byte_start FROM refs WHERE ref_id=?1",
            [&reference.ref_id],
            |r| r.get(0),
        )?;
        let Some(raw) = parse
            .refs
            .iter()
            .find(|r| r.kind == join::BlobRefKind::Call && r.byte_start == start)
        else {
            return Ok(None);
        };
        let Some(site) = parse
            .dispatch
            .sites
            .iter()
            .find(|s| s.ordinal == raw.ordinal)
        else {
            return Ok(None);
        };
        // Unknown receivers must not acquire an edge from a global name match.
        if !site.receiver.as_ref().is_some_and(|r| {
            r.starts_with("@object:") || r.starts_with("@return:") || r.starts_with("@field:")
        }) {
            return Ok(None);
        }
        let mut resolver = join::dispatch::Resolver::new(
            self.parses
                .iter()
                .map(|(path, parse)| (path.clone(), parse))
                .collect(),
        );
        resolver.type_targets = self.targets.clone();
        let resolution = resolver.resolve(&reference.caller_file, site);
        if resolution.targets.len() != 1 {
            return Ok(None);
        }
        let target = resolution.targets.into_iter().next().unwrap();
        let Some(node_id) = self.index.node_for_symbol(&target.file, &target.symbol) else {
            return Ok(None);
        };
        Ok(Some(NameMatchCandidate {
            node_id,
            file_path: target.file,
            scoped_name: target.symbol,
            kind: "method".into(),
            start_line: 0,
        }))
    }
}
