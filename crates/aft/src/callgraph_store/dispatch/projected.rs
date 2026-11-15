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

use super::{dependency, DispatchFacts, MethodHint, ParseBlob, Resolver, SiteHint, TypeHint};

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
    /// How many of `projections` are there only because `complete_names`
    /// is set, for measurement.
    resolvable_name_projections: u32,
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

    /// How many projected answers and resolution orders the record keeps,
    /// how many of those answer a Rust file's resolvable names only, and
    /// the heap they hold.
    pub(crate) fn projection_census(
        &self,
        seen: &mut aft_store::heap::Seen,
    ) -> (usize, usize, usize) {
        use aft_store::heap::HeapBytes as _;
        (
            self.projections.len() + self.mro.len(),
            self.resolvable_name_projections as usize,
            self.projections.heap_bytes(seen) + self.mro.heap_bytes(seen),
        )
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

/// The first byte of every scratch-encoded record (see
/// [`FileRecord::encode_scratch`]). The encoding lives only in one build's
/// scratch folder and is read back by the same process, so it needs no
/// version history; the byte only catches bytes that are not a record.
const SCRATCH_FORMAT: u8 = 1;

/// Appends the scratch encoding's parts: counts and lengths as
/// little-endian `u32`, flags as one byte, text as its length and bytes.
struct ScratchWriter<'o>(&'o mut Vec<u8>);

impl ScratchWriter<'_> {
    fn byte(&mut self, value: u8) {
        self.0.push(value);
    }

    fn flag(&mut self, value: bool) {
        self.0.push(u8::from(value));
    }

    fn number(&mut self, value: u32) {
        self.0.extend(value.to_le_bytes());
    }

    fn count(&mut self, count: usize) -> Result<(), String> {
        let count = u32::try_from(count)
            .map_err(|_| "a pass-4 record part has 4 billion or more items".to_string())?;
        self.number(count);
        Ok(())
    }

    fn text(&mut self, text: &str) -> Result<(), String> {
        self.count(text.len())?;
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }

    fn optional(&mut self, text: Option<&str>) -> Result<(), String> {
        match text {
            None => self.byte(0),
            Some(text) => {
                self.byte(1);
                self.text(text)?;
            }
        }
        Ok(())
    }

    fn texts(&mut self, texts: &[String]) -> Result<(), String> {
        self.count(texts.len())?;
        for text in texts {
            self.text(text)?;
        }
        Ok(())
    }
}

/// Reads what [`ScratchWriter`] wrote, failing on anything it could not
/// have written.
struct ScratchReader<'b> {
    bytes: &'b [u8],
    at: usize,
}

impl<'b> ScratchReader<'b> {
    fn take(&mut self, len: usize) -> Result<&'b [u8], String> {
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| "a truncated pass-4 record".to_string())?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn flag(&mut self) -> Result<bool, String> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(format!("a pass-4 record flag reads {other}")),
        }
    }

    fn number(&mut self) -> Result<u32, String> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().expect("four bytes")))
    }

    /// A count of items, each at least one byte long, so a damaged count
    /// cannot ask for more memory than the record's bytes.
    fn count(&mut self) -> Result<usize, String> {
        let count = self.number()? as usize;
        if count > self.bytes.len() - self.at {
            return Err(format!("a pass-4 record counts {count} items past its end"));
        }
        Ok(count)
    }

    fn text(&mut self) -> Result<String, String> {
        let len = self.number()? as usize;
        String::from_utf8(self.take(len)?.to_vec())
            .map_err(|_| "a pass-4 record holds text that is not UTF-8".to_string())
    }

    fn optional(&mut self) -> Result<Option<String>, String> {
        match self.byte()? {
            0 => Ok(None),
            1 => self.text().map(Some),
            other => Err(format!("a pass-4 record option reads {other}")),
        }
    }

    fn texts(&mut self) -> Result<Vec<String>, String> {
        let count = self.count()?;
        (0..count).map(|_| self.text()).collect()
    }
}

impl ProjectedType {
    fn encode_scratch(&self, out: &mut ScratchWriter<'_>) -> Result<(), String> {
        let Self {
            found,
            dependencies,
        } = self;
        match found {
            None => out.byte(0),
            Some((file, index)) => {
                out.byte(1);
                out.text(file)?;
                out.number(*index);
            }
        }
        out.texts(dependencies)
    }

    fn decode_scratch(input: &mut ScratchReader<'_>) -> Result<Self, String> {
        let found = match input.byte()? {
            0 => None,
            1 => Some((input.text()?, input.number()?)),
            other => return Err(format!("a projected type reads {other}")),
        };
        Ok(Self {
            found,
            dependencies: input.texts()?.into_boxed_slice(),
        })
    }
}

impl ProjectedMro {
    fn encode_scratch(&self, out: &mut ScratchWriter<'_>) -> Result<(), String> {
        let Self { mro, dependencies } = self;
        match mro {
            None => out.byte(0),
            Some(order) => {
                out.byte(1);
                out.count(order.len())?;
                for (file, name) in order {
                    out.text(file)?;
                    out.text(name)?;
                }
            }
        }
        out.texts(dependencies)
    }

    fn decode_scratch(input: &mut ScratchReader<'_>) -> Result<Self, String> {
        let mro = match input.byte()? {
            0 => None,
            1 => {
                let count = input.count()?;
                Some(
                    (0..count)
                        .map(|_| Ok((input.text()?, input.text()?)))
                        .collect::<Result<Vec<_>, String>>()?,
                )
            }
            other => return Err(format!("a resolution order reads {other}")),
        };
        Ok(Self {
            mro,
            dependencies: input.texts()?.into_boxed_slice(),
        })
    }
}

impl FileRecord {
    /// Appends the record's scratch encoding to `out`: what a cold build
    /// writes to its scratch folder, and [`Self::decode_scratch`] reads back
    /// equal. Never published.
    ///
    /// # Errors
    ///
    /// When a part has 4 billion or more items or bytes.
    pub(crate) fn encode_scratch(&self, out: &mut Vec<u8>) -> Result<(), String> {
        // Named in full, so a field added to the record must be added here.
        let Self {
            facts,
            projections,
            mro,
            complete_names,
            resolvable_name_projections,
            sites,
            associated,
        } = self;
        debug_assert!(
            *facts == declarations(facts),
            "a record keeps only the facts `declarations` keeps"
        );
        let mut out = ScratchWriter(out);
        out.byte(SCRATCH_FORMAT);
        out.text(&facts.language)?;
        out.optional(facts.dispatch.go_package.as_deref())?;
        out.count(facts.dispatch.types.len())?;
        for TypeHint {
            name,
            bases,
            interface,
            closed,
        } in &facts.dispatch.types
        {
            out.text(name)?;
            out.texts(bases)?;
            out.flag(*interface);
            out.flag(*closed);
        }
        out.count(facts.dispatch.methods.len())?;
        for MethodHint {
            owner,
            trait_name,
            name,
            symbol,
            has_body,
            shape,
            private,
        } in &facts.dispatch.methods
        {
            out.text(owner)?;
            out.optional(trait_name.as_deref())?;
            out.text(name)?;
            out.text(symbol)?;
            out.flag(*has_body);
            out.text(shape)?;
            out.flag(*private);
        }
        out.count(projections.len())?;
        for (name, projected) in projections {
            out.text(name)?;
            projected.encode_scratch(&mut out)?;
        }
        out.count(mro.len())?;
        for (owner, order) in mro {
            out.text(owner)?;
            order.encode_scratch(&mut out)?;
        }
        out.flag(*complete_names);
        out.number(*resolvable_name_projections);
        out.count(sites.len())?;
        for SiteRecord {
            site,
            associated,
            keeps_ordinary_edge,
        } in sites
        {
            let SiteHint {
                ordinal,
                caller,
                line,
                member,
                receiver,
                dynamic,
            } = site;
            out.number(*ordinal);
            out.optional(caller.as_deref())?;
            out.number(*line);
            out.optional(member.as_deref())?;
            out.optional(receiver.as_deref())?;
            out.flag(*dynamic);
            out.flag(*associated);
            out.flag(*keeps_ordinary_edge);
        }
        out.count(associated.len())?;
        for ordinal in associated {
            out.number(*ordinal);
        }
        Ok(())
    }

    /// The record [`Self::encode_scratch`] encoded as `bytes`.
    ///
    /// # Errors
    ///
    /// When `bytes` is not exactly one record's scratch encoding.
    pub(crate) fn decode_scratch(bytes: &[u8]) -> Result<Self, String> {
        let mut input = ScratchReader { bytes, at: 0 };
        let format = input.byte()?;
        if format != SCRATCH_FORMAT {
            return Err(format!("not a pass-4 record (it starts with {format})"));
        }
        let language = input.text()?;
        let go_package = input.optional()?;
        let count = input.count()?;
        let mut types = Vec::with_capacity(count);
        for _ in 0..count {
            types.push(TypeHint {
                name: input.text()?,
                bases: input.texts()?,
                interface: input.flag()?,
                closed: input.flag()?,
            });
        }
        let count = input.count()?;
        let mut methods = Vec::with_capacity(count);
        for _ in 0..count {
            methods.push(MethodHint {
                owner: input.text()?,
                trait_name: input.optional()?,
                name: input.text()?,
                symbol: input.text()?,
                has_body: input.flag()?,
                shape: input.text()?,
                private: input.flag()?,
            });
        }
        let mut projections = BTreeMap::new();
        for _ in 0..input.count()? {
            let name = input.text()?;
            projections.insert(name, ProjectedType::decode_scratch(&mut input)?);
        }
        let mut mro = BTreeMap::new();
        for _ in 0..input.count()? {
            let owner = input.text()?;
            mro.insert(owner, ProjectedMro::decode_scratch(&mut input)?);
        }
        let complete_names = input.flag()?;
        let resolvable_name_projections = input.number()?;
        let count = input.count()?;
        let mut sites = Vec::with_capacity(count);
        for _ in 0..count {
            sites.push(SiteRecord {
                site: SiteHint {
                    ordinal: input.number()?,
                    caller: input.optional()?,
                    line: input.number()?,
                    member: input.optional()?,
                    receiver: input.optional()?,
                    dynamic: input.flag()?,
                },
                associated: input.flag()?,
                keeps_ordinary_edge: input.flag()?,
            });
        }
        let count = input.count()?;
        let associated = (0..count)
            .map(|_| input.number())
            .collect::<Result<Vec<_>, _>>()?;
        if input.at != bytes.len() {
            return Err(format!(
                "a pass-4 record has {} bytes past its end",
                bytes.len() - input.at
            ));
        }
        Ok(Self {
            facts: declarations_of(language, go_package, types, methods),
            projections,
            mro,
            complete_names,
            resolvable_name_projections,
            sites,
            associated,
        })
    }
}

/// `parse` reduced to what the resolver reads once projections are
/// recorded: language, Go package, types and methods.
fn declarations(parse: &ParseBlob) -> ParseBlob {
    declarations_of(
        parse.language.clone(),
        parse.dispatch.go_package.clone(),
        parse.dispatch.types.clone(),
        parse.dispatch.methods.clone(),
    )
}

/// The facts a [`FileRecord`] keeps, from their parts: every other field
/// of the parse is left empty.
fn declarations_of(
    language: String,
    go_package: Option<String>,
    types: Vec<TypeHint>,
    methods: Vec<MethodHint>,
) -> ParseBlob {
    ParseBlob {
        entry_point_symbols: Vec::new(),
        dispatch: DispatchFacts {
            go_package,
            types,
            methods,
            ..DispatchFacts::default()
        },
        extractor_version: String::new(),
        language,
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
        let inserted = std::cell::Cell::new(0u32);
        let mut project = |name: &str| -> Option<(String, u32)> {
            if let Some(found) = projections.get(name) {
                let found: &ProjectedType = found;
                return found.found.clone();
            }
            let projected = self.record_projection(path, name);
            let found = projected.found.clone();
            projections.insert(name.to_string(), projected);
            inserted.set(inserted.get() + 1);
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
        let before = inserted.get();
        if typed_rust {
            for name in rust_resolvable_names(parse, type_target_names) {
                project(&name);
            }
            complete_names = true;
        }
        let resolvable_name_projections = inserted.get() - before;
        associated_ordinals.sort_unstable();
        associated_ordinals.dedup();
        Some(FileRecord {
            facts: declarations(parse),
            projections,
            mro,
            complete_names,
            resolvable_name_projections,
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
    resolvable_name_projections,
    sites,
    associated,
});

#[cfg(test)]
impl FileRecord {
    /// A record whose parts all depend on `index`, declaring methods named
    /// `m{index % 3}` and `shared`, for tests of where records are kept.
    pub(crate) fn sample(index: u32) -> Self {
        let method = |name: String| MethodHint {
            owner: format!("Type{index}"),
            trait_name: None,
            name,
            symbol: format!("Type{index}::method"),
            has_body: index % 2 == 0,
            shape: "x".repeat(index as usize % 40),
            private: false,
        };
        FileRecord {
            facts: declarations_of(
                "typescript".to_string(),
                None,
                vec![TypeHint {
                    name: format!("Type{index}"),
                    bases: vec![format!("Base{}", index / 2)],
                    interface: false,
                    closed: false,
                }],
                vec![
                    method(format!("m{}", index % 3)),
                    method("shared".to_string()),
                ],
            ),
            projections: BTreeMap::from([(
                format!("Base{}", index / 2),
                ProjectedType {
                    found: (index % 2 == 0).then(|| (format!("src/{index}.ts"), index)),
                    dependencies: vec![format!("t:src/{index}.ts\tBase")].into_boxed_slice(),
                },
            )]),
            mro: BTreeMap::new(),
            complete_names: false,
            resolvable_name_projections: 0,
            sites: (0..index % 5)
                .map(|ordinal| SiteRecord {
                    site: SiteHint {
                        ordinal,
                        caller: Some(format!("caller{index}")),
                        line: ordinal + index,
                        member: Some("shared".to_string()),
                        receiver: None,
                        dynamic: false,
                    },
                    associated: false,
                    keeps_ordinary_edge: false,
                })
                .collect(),
            associated: Vec::new(),
        }
    }
}

#[cfg(test)]
mod scratch_tests {
    use super::*;

    fn hint(name: &str) -> MethodHint {
        MethodHint {
            owner: "Owner".to_string(),
            trait_name: Some(format!("Trait\u{0}{name}")),
            name: name.to_string(),
            symbol: format!("Owner::{name}"),
            has_body: true,
            shape: "(&self)".to_string(),
            private: false,
        }
    }

    /// A record with every part set, including empty and missing text, a
    /// zero byte and text that is not ASCII.
    fn record() -> FileRecord {
        let facts = declarations_of(
            "python".to_string(),
            Some("pkg".to_string()),
            vec![
                TypeHint {
                    name: "Owner".to_string(),
                    bases: vec!["Base".to_string(), String::new()],
                    interface: false,
                    closed: true,
                },
                TypeHint {
                    name: "Ünïcode".to_string(),
                    bases: Vec::new(),
                    interface: true,
                    closed: false,
                },
            ],
            vec![hint("run"), hint("")],
        );
        let projections = BTreeMap::from([
            (
                "Base".to_string(),
                ProjectedType {
                    found: Some(("pkg/base.py".to_string(), 3)),
                    dependencies: vec!["r:pkg/base.py".to_string(), "t:x\ty".to_string()]
                        .into_boxed_slice(),
                },
            ),
            (
                "@field:x".to_string(),
                ProjectedType {
                    found: None,
                    dependencies: Box::default(),
                },
            ),
        ]);
        let mro = BTreeMap::from([
            (
                "Owner".to_string(),
                ProjectedMro {
                    mro: Some(vec![
                        ("pkg/a.py".to_string(), "Owner".to_string()),
                        ("pkg/base.py".to_string(), "Base".to_string()),
                    ]),
                    dependencies: vec!["m:run".to_string()].into_boxed_slice(),
                },
            ),
            (
                "Other".to_string(),
                ProjectedMro {
                    mro: None,
                    dependencies: Box::default(),
                },
            ),
        ]);
        let sites = vec![
            SiteRecord {
                site: SiteHint {
                    ordinal: 7,
                    caller: Some("Owner::run".to_string()),
                    line: 12,
                    member: Some("run".to_string()),
                    receiver: Some("@super:Owner".to_string()),
                    dynamic: false,
                },
                associated: true,
                keeps_ordinary_edge: false,
            },
            SiteRecord {
                site: SiteHint {
                    ordinal: u32::MAX,
                    caller: None,
                    line: 0,
                    member: None,
                    receiver: None,
                    dynamic: true,
                },
                associated: false,
                keeps_ordinary_edge: true,
            },
        ];
        FileRecord {
            facts,
            projections,
            mro,
            complete_names: true,
            resolvable_name_projections: 5,
            sites,
            associated: vec![7],
        }
    }

    #[test]
    fn a_scratch_encoded_record_decodes_equal() {
        let record = record();
        let mut bytes = Vec::new();
        record.encode_scratch(&mut bytes).unwrap();
        assert_eq!(FileRecord::decode_scratch(&bytes).unwrap(), record);

        // An empty record too.
        let empty = FileRecord {
            facts: declarations_of(String::new(), None, Vec::new(), Vec::new()),
            projections: BTreeMap::new(),
            mro: BTreeMap::new(),
            complete_names: false,
            resolvable_name_projections: 0,
            sites: Vec::new(),
            associated: Vec::new(),
        };
        let mut empty_bytes = Vec::new();
        empty.encode_scratch(&mut empty_bytes).unwrap();
        assert_eq!(FileRecord::decode_scratch(&empty_bytes).unwrap(), empty);
    }

    #[test]
    fn bytes_that_are_not_one_scratch_record_fail_to_decode() {
        let mut bytes = Vec::new();
        record().encode_scratch(&mut bytes).unwrap();
        for len in 0..bytes.len() {
            assert!(
                FileRecord::decode_scratch(&bytes[..len]).is_err(),
                "a record cut to {len} of {} bytes decoded",
                bytes.len()
            );
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(FileRecord::decode_scratch(&longer).is_err());
        let mut other = bytes;
        other[0] = SCRATCH_FORMAT + 1;
        assert!(FileRecord::decode_scratch(&other).is_err());
    }
}
