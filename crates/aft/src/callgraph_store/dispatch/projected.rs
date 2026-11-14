//! Method-call resolution split in two: what each file's sites and
//! declarations mean, worked out in file order, and the matching of sites
//! to declarations, which then reads only the records written here.
//!
//! [`Resolver::project_type`] is the only rule that reads a file's imports,
//! fields, return types, type and import targets, or its directory, and
//! Python's method resolution order is the only rule that follows imports
//! across files. Both are answered here once per file, with the dependency
//! keys they recorded. A resolver over a source with
//! [`DispatchFiles::projections`] replays these answers and reads each file
//! only for its language, Go package, types and methods (the method rows
//! and the type graph), so it may run in any order: every other rule runs
//! the same code on the same facts and records the same keys.
//!
//! What pass 4 asks [`Resolver::project_type`] of each file `F`:
//! - each site's receiver, from the calling file;
//! - each method's owner and trait, from `F`;
//! - each base of every type `F` declares, from `F`;
//! - each base of a method's owner type, again from the method's file `F`
//!   (the unknown-receiver rule projects them from there, even when the
//!   owner type is declared elsewhere);
//! - for a Rust file with typed sites, the last segment of a trait or owner
//!   name, from the calling file. Those names are not known in file order,
//!   so every name such a file can resolve is projected, and any other name
//!   gets [`Recorded::NotProjectable`], whose answer and keys are fixed (see
//!   [`Resolver::replay_projection`]).

use std::collections::{BTreeMap, BTreeSet};

use super::{dependency, DispatchFacts, ParseBlob, Resolver, SiteHint, TypeHint};

/// The answer [`Resolver::project_type`] gave for one name asked of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectedType {
    /// The file declaring the type and the type's position among that
    /// file's types, or `None` when the name names no project type.
    found: Option<(String, u32)>,
    /// The keys the lookup recorded.
    dependencies: Box<[String]>,
}

/// The method resolution order a Python `super()` call reads for its owner
/// type, with the keys computing it recorded. The owner's own projection is
/// recorded separately, as a [`ProjectedType`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectedMro {
    mro: Option<Vec<(String, String)>>,
    dependencies: Box<[String]>,
}

/// One method-call site in `bound_dispatch_sites()` order, with what the
/// pass-4 driver reads about it from its file besides the site itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SiteRecord {
    pub(crate) site: SiteHint,
    /// Whether the file's reference with the site's ordinal is a Rust
    /// associated-function call. The resolver applies it to Rust files only.
    pub(crate) associated: bool,
    /// Whether pass 2 already linked the site's reference with an ordinary
    /// edge that Rust spells as an associated or `self.` call: such a Rust
    /// site with no project receiver is left out of the file's sites.
    pub(crate) keeps_ordinary_edge: bool,
}

/// Everything pass 4 reads about one file once receivers are projected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FileRecord {
    /// The file's language, Go package, types and methods; everything else
    /// is left empty.
    facts: ParseBlob,
    /// Every name pass 4 asks [`Resolver::project_type`] of this file, with
    /// its answer.
    projections: BTreeMap<String, ProjectedType>,
    /// For each owner of a Python `super()` site of this file, the owner
    /// type's resolution order.
    mro: BTreeMap<String, ProjectedMro>,
    /// Set for a Rust file whose every resolvable name is in
    /// `projections`, so any other name has the fixed miss answer.
    complete_names: bool,
    sites: Vec<SiteRecord>,
    /// Sorted ordinals of the sites whose `associated` is set.
    associated: Vec<u32>,
}

/// A record looked up by name.
pub(crate) enum Recorded<'r, T> {
    Found(&'r T),
    /// A name of a Rust file the file cannot resolve to a type.
    NotProjectable,
    /// No record: the records are incomplete, which fails the build.
    Missing,
}

/// Recorded projections, as a source serving them hands them to the
/// resolver.
pub(crate) trait Projections {
    fn projected_type(&self, file: &str, name: &str) -> Recorded<'_, ProjectedType>;
    fn projected_mro(&self, file: &str, owner: &str) -> Recorded<'_, ProjectedMro>;
    /// Notes a record the resolver needed and did not find; the pass fails.
    fn missing(&self, what: String);
}

impl FileRecord {
    pub(crate) fn facts(&self) -> &ParseBlob {
        &self.facts
    }

    pub(crate) fn sites(&self) -> &[SiteRecord] {
        &self.sites
    }

    /// How many projected answers and resolution orders the record keeps.
    pub(crate) fn projection_count(&self) -> usize {
        self.projections.len() + self.mro.len()
    }

    pub(crate) fn associated(&self, ordinal: u32) -> bool {
        self.associated.binary_search(&ordinal).is_ok()
    }

    pub(crate) fn projected_type(&self, name: &str) -> Recorded<'_, ProjectedType> {
        match self.projections.get(name) {
            Some(found) => Recorded::Found(found),
            None if self.complete_names && !name.starts_with('@') => Recorded::NotProjectable,
            None => Recorded::Missing,
        }
    }

    pub(crate) fn projected_mro(&self, owner: &str) -> Recorded<'_, ProjectedMro> {
        self.mro
            .get(owner)
            .map_or(Recorded::Missing, Recorded::Found)
    }

    /// The names of the methods the file declares, in name order.
    pub(crate) fn method_names(&self) -> BTreeSet<&str> {
        self.facts
            .dispatch
            .methods
            .iter()
            .map(|method| method.name.as_str())
            .collect()
    }

    /// For a Go file, the names of the methods its interfaces require.
    pub(crate) fn go_required(&self) -> BTreeSet<&str> {
        if self.facts.language != "go" {
            return BTreeSet::new();
        }
        let dispatch = &self.facts.dispatch;
        dispatch
            .methods
            .iter()
            .filter(|method| {
                dispatch
                    .types
                    .iter()
                    .any(|ty| ty.interface && ty.name == method.owner)
            })
            .map(|method| method.name.as_str())
            .collect()
    }
}

/// `parse` reduced to what the resolver reads once projections are
/// recorded: language, Go package, types and methods.
fn declarations(parse: &ParseBlob) -> ParseBlob {
    ParseBlob {
        entry_point_symbols: Vec::new(),
        dispatch: DispatchFacts {
            go_package: parse.dispatch.go_package.clone(),
            types: parse.dispatch.types.clone(),
            methods: parse.dispatch.methods.clone(),
            ..DispatchFacts::default()
        },
        extractor_version: String::new(),
        language: parse.language.clone(),
        ast_nodes: Vec::new(),
        symbols: Vec::new(),
        default_export_symbol: None,
        exported_symbols: Vec::new(),
        callable_symbols: Vec::new(),
        imports: Vec::new(),
        python_bindings: Vec::new(),
        refs: Vec::new(),
    }
}

/// Every name a Rust file's [`Resolver::project_type`] can answer other than
/// with the fixed miss: the types it declares, its receiver type targets,
/// and the names its imports bind (read as `File::imports_naming` reads
/// them).
fn rust_resolvable_names(
    parse: &ParseBlob,
    type_target_names: &mut dyn Iterator<Item = &str>,
) -> BTreeSet<String> {
    let mut names = parse
        .dispatch
        .types
        .iter()
        .map(|ty| ty.name.clone())
        .collect::<BTreeSet<_>>();
    names.extend(type_target_names.map(str::to_string));
    for import in &parse.imports {
        names.extend(
            import
                .names
                .iter()
                .filter_map(|name| name.split_whitespace().last())
                .map(str::to_string),
        );
        names.extend(import.default_import.clone());
    }
    names
}

impl<'a> Resolver<'a> {
    /// The source's recorded projections, when it serves them.
    pub(super) fn projections(&self) -> Option<&'a (dyn Projections + 'a)> {
        self.source.and_then(|source| source.projections())
    }

    /// [`Resolver::project_type`] answered from `projections`, recording the
    /// keys the recorded lookup recorded.
    pub(super) fn replay_projection(
        &self,
        projections: &'a (dyn Projections + 'a),
        file: &str,
        name: &str,
    ) -> Option<(String, &'a TypeHint)> {
        match projections.projected_type(file, name) {
            Recorded::Found(projected) => {
                for key in projected.dependencies.iter() {
                    self.depend(key.clone());
                }
                let (type_file, index) = projected.found.as_ref()?;
                let ty = self
                    .lookup(type_file)
                    .and_then(|parse| parse.dispatch.types.get(usize::try_from(*index).ok()?));
                if ty.is_none() {
                    projections.missing(format!(
                        "type {index} of {type_file}, which {name} names in {file}"
                    ));
                }
                Some((type_file.clone(), ty?))
            }
            // A Rust file's lookup of a name it neither declares, targets
            // nor imports reads its facts, the name's target and the
            // name's local declaration and imports, and finds nothing.
            Recorded::NotProjectable => {
                self.depend(dependency::file(file));
                self.depend(dependency::type_name(file, name));
                None
            }
            Recorded::Missing => {
                projections.missing(format!("the projection of {name} in {file}"));
                None
            }
        }
    }

    /// `owner`'s resolution order as `file`'s Python `super()` call reads
    /// it, from `projections`, recording the keys computing it recorded.
    pub(super) fn replay_mro(
        &self,
        projections: &'a (dyn Projections + 'a),
        file: &str,
        owner: &str,
    ) -> Option<Vec<(String, String)>> {
        match projections.projected_mro(file, owner) {
            Recorded::Found(projected) => {
                for key in projected.dependencies.iter() {
                    self.depend(key.clone());
                }
                projected.mro.clone()
            }
            Recorded::NotProjectable | Recorded::Missing => {
                projections.missing(format!("the resolution order of {owner} in {file}"));
                None
            }
        }
    }

    /// [`Resolver::project_type`] for `name` in `file` with the keys it
    /// recorded, leaving the keys recorded so far as they were.
    fn record_projection(&self, file: &str, name: &str) -> ProjectedType {
        let outer = self.take_dependencies();
        let found = self.project_type(file, name).and_then(|(type_file, ty)| {
            let index = self
                .lookup(&type_file)?
                .dispatch
                .types
                .iter()
                .position(|candidate| std::ptr::eq(candidate, ty))?;
            Some((type_file, u32::try_from(index).ok()?))
        });
        let dependencies = self.take_dependencies();
        *self.dependencies.borrow_mut() = outer;
        ProjectedType {
            found,
            dependencies: dependencies.into_iter().collect(),
        }
    }

    /// The resolution order `python_super` reads for `owner` in `file`,
    /// with the keys computing it recorded. `None` when `owner` names no
    /// project type, so `python_super` reads no order.
    fn record_mro(&self, file: &str, owner: &str) -> Option<ProjectedMro> {
        let outer = self.take_dependencies();
        let projected = self.project_type(file, owner);
        // The owner's projection is recorded on its own.
        self.take_dependencies();
        let mro = projected.map(|(owner_file, ty)| {
            let mro = self.python_mro(&owner_file, ty, &mut BTreeSet::new(), &mut BTreeMap::new());
            ProjectedMro {
                mro,
                dependencies: self.take_dependencies().into_iter().collect(),
            }
        });
        *self.dependencies.borrow_mut() = outer;
        mro
    }

    /// The pass-4 record of `path`, a file this resolver reads from files:
    /// its declarations, every projection pass 4 asks of it, and `sites`
    /// (its `bound_dispatch_sites()`), each with whether its ordinal is an
    /// associated-function call and whether it keeps an ordinary edge.
    /// `None` when `path` has no facts.
    pub(crate) fn project_file(
        &self,
        path: &str,
        sites: Vec<SiteHint>,
        type_target_names: &mut dyn Iterator<Item = &str>,
        associated: &dyn Fn(u32) -> bool,
        keeps_ordinary_edge: &dyn Fn(u32) -> bool,
    ) -> Option<FileRecord> {
        let parse = self.lookup(path)?;
        let mut projections = BTreeMap::new();
        let mut project = |name: &str| -> Option<(String, u32)> {
            if let Some(found) = projections.get(name) {
                let found: &ProjectedType = found;
                return found.found.clone();
            }
            let projected = self.record_projection(path, name);
            let found = projected.found.clone();
            projections.insert(name.to_string(), projected);
            found
        };
        for ty in &parse.dispatch.types {
            for base in &ty.bases {
                project(base);
            }
        }
        for method in &parse.dispatch.methods {
            if let Some((owner_file, index)) = project(&method.owner) {
                let owner = self
                    .lookup(&owner_file)
                    .and_then(|owner| owner.dispatch.types.get(index as usize));
                for base in owner.into_iter().flat_map(|owner| &owner.bases) {
                    project(base);
                }
            }
            if let Some(trait_name) = &method.trait_name {
                project(trait_name);
            }
        }
        let language = parse.language.as_str();
        let mut mro = BTreeMap::new();
        let mut typed_rust = false;
        let mut records = Vec::with_capacity(sites.len());
        let mut associated_ordinals = Vec::new();
        for site in sites {
            let record = SiteRecord {
                associated: associated(site.ordinal),
                keeps_ordinary_edge: keeps_ordinary_edge(site.ordinal),
                site,
            };
            if record.associated {
                associated_ordinals.push(record.site.ordinal);
            }
            let site = &record.site;
            // The projections `Resolver::resolve` makes from the calling
            // file, on the paths that reach one.
            let typed = !site.dynamic
                && site.member.is_some()
                && !(language == "go" && parse.dispatch.go_package.is_none())
                && language != "cpp";
            match site.receiver.as_deref() {
                Some(receiver)
                    if typed
                        && !(language == "rust"
                            && receiver.starts_with(super::TEMPLATE_SELF_RECEIVER)) =>
                {
                    match receiver.strip_prefix("@super:") {
                        Some(owner) if language == "python" => {
                            project(owner);
                            if !mro.contains_key(owner) {
                                if let Some(order) = self.record_mro(path, owner) {
                                    mro.insert(owner.to_string(), order);
                                }
                            }
                        }
                        _ => {
                            project(receiver);
                            typed_rust |= language == "rust";
                        }
                    }
                }
                _ => {}
            }
            records.push(record);
        }
        let mut complete_names = false;
        if typed_rust {
            for name in rust_resolvable_names(parse, type_target_names) {
                project(&name);
            }
            complete_names = true;
        }
        associated_ordinals.sort_unstable();
        associated_ordinals.dedup();
        Some(FileRecord {
            facts: declarations(parse),
            projections,
            mro,
            complete_names,
            sites: records,
            associated: associated_ordinals,
        })
    }
}

aft_store::heap_bytes_of_fields!(ProjectedType {
    found,
    dependencies
});
aft_store::heap_bytes_of_fields!(ProjectedMro { mro, dependencies });
aft_store::heap_bytes_of_fields!(SiteRecord {
    site,
    associated,
    keeps_ordinary_edge,
});
aft_store::heap_bytes_of_fields!(FileRecord {
    facts,
    projections,
    mro,
    complete_names,
    sites,
    associated,
});
