use std::borrow::Cow;
use std::collections::BTreeSet;
use std::path::Path;

use super::super::{dependency, typescript, DispatchParse, Resolution, Resolver, Target};
use super::extract::join;
use super::{csharp_projects::ProjectId, relative_module, Declaration, Scope, StaticFacts};

pub(in crate::callgraph_store) const STATIC_IMPORT_PREFIX: &str = "@static-import:";
// An import can resolve to a project module without resolving to a class.
const MISSING_STATIC_EXPORT: u32 = u32::MAX;

struct Type<'a> {
    file: String,
    declaration: Cow<'a, Declaration>,
    scope: Cow<'a, Scope>,
}

impl<'a> Type<'a> {
    fn from_parse(file: &str, parse: &DispatchParse<'a>, index: usize) -> Option<Self> {
        // A pending base walk needs only the selected declaration and its
        // scope, not the file's other declarations or its cache handle.
        let declaration = parse.select(|p| p.dispatch.static_facts.as_ref()?.types.get(index))?;
        let scope = parse.select(|p| {
            p.dispatch
                .static_facts
                .as_ref()?
                .scopes
                .get(declaration.scope as usize)
        })?;
        Some(Self {
            file: file.to_string(),
            declaration,
            scope,
        })
    }
}

impl<'a> Resolver<'a> {
    fn declares_namespace(&self, language: &str, namespace: &str) -> bool {
        let key = StaticFacts::namespace_key(language, namespace);
        self.depend(key.clone());
        let declares = |parse: &super::super::ParseBlob| {
            parse.language == language
                && parse.dispatch.static_facts.as_ref().is_some_and(|facts| {
                    facts
                        .namespaces
                        .binary_search_by(|name| name.as_str().cmp(namespace))
                        .is_ok()
                })
        };
        match self.source {
            Some(source) => source.files_with_static_type(&key).iter().any(|path| {
                self.lookup_handle(path)
                    .is_some_and(|parse| declares(&parse))
            }),
            None => self.files.values().copied().any(declares),
        }
    }

    fn absent_namespace(&self, language: &str, type_name: &str) -> bool {
        let Some((namespace, _)) = type_name.rsplit_once('.') else {
            return false;
        };
        !self.declares_namespace(language, namespace)
    }
    fn csharp_project(&self, file: &str) -> Option<ProjectId> {
        self.csharp_projects?.owner(file, |key| self.depend(key))
    }

    fn csharp_project_types(&self, project: ProjectId, name: &str) -> Vec<Type<'a>> {
        let Some(visible) = self
            .csharp_projects
            .and_then(|projects| projects.reachable(project, |key| self.depend(key)))
        else {
            return Vec::new();
        };
        self.static_types("csharp", name)
            .into_iter()
            .filter(|ty| {
                self.csharp_project(&ty.file)
                    .is_some_and(|owner| visible.contains(&owner))
            })
            .collect()
    }
    fn static_types(&self, language: &str, name: &str) -> Vec<Type<'a>> {
        let key = StaticFacts::type_key(language, name);
        // A miss matters too: adding this declaration can turn a formerly
        // unknown receiver into a static call without changing its source file.
        self.depend(StaticFacts::dependency(language, name));
        let files: Vec<_> = match self.source {
            Some(source) => source.files_with_static_type(&key),
            None => self
                .files
                .iter()
                .filter(|(_, parse)| {
                    parse.language == language
                        && parse
                            .dispatch
                            .static_facts
                            .as_ref()
                            .is_some_and(|facts| facts.types.iter().any(|ty| ty.name == name))
                })
                .map(|(path, _)| path.clone())
                .collect(),
        };
        let mut types = Vec::new();
        for file in files {
            let Some(parse) = self.lookup_handle(&file).filter(|p| p.language == language) else {
                continue;
            };
            types.extend(Self::static_types_in(&file, &parse, name));
        }
        types
    }

    fn static_types_in<'p>(
        file: &'p str,
        parse: &'p DispatchParse<'a>,
        name: &'p str,
    ) -> impl Iterator<Item = Type<'a>> + 'p {
        parse.dispatch.static_facts.iter().flat_map(move |facts| {
            facts
                .types
                .iter()
                .enumerate()
                .filter(move |(_, ty)| ty.name == name)
                .filter_map(move |(index, _)| Type::from_parse(file, parse, index))
        })
    }

    fn ts_static_types(&self, file: &str, name: &str) -> Vec<Type<'a>> {
        let Some((path, ty)) = self.project_type_hint(file, name) else {
            return Vec::new();
        };
        self.depend(dependency::file(&path));
        let Some(parse) = self.lookup_handle(&path) else {
            return Vec::new();
        };
        Self::static_types_in(&path, &parse, &ty.name).collect()
    }

    pub(in super::super) fn static_import_projection(
        &self,
        file: &str,
        key: &str,
    ) -> Option<(String, u32)> {
        let (path, symbol) = self.type_target(file, key)?;
        self.depend(dependency::file(&path));
        let index = self
            .lookup_handle(&path)
            .and_then(|parse| {
                let facts = parse.dispatch.static_facts.as_deref()?;
                let mut types = facts
                    .types
                    .iter()
                    .enumerate()
                    .filter(|(_, ty)| ty.name == symbol);
                let (index, _) = types.next()?;
                types.next().is_none().then_some(index as u32)
            })
            .unwrap_or(MISSING_STATIC_EXPORT);
        Some((path, index))
    }

    fn ts_import_type(&self, file: &str, name: &str, module: &str) -> Option<Vec<Type<'a>>> {
        // A host can supply a bare module at runtime without any file for
        // import resolution to find. Only relative imports can prove a class.
        if !relative_module(module) {
            return None;
        }
        let key = format!("{STATIC_IMPORT_PREFIX}{name}");
        let (path, index) = match self.projections() {
            Some(projections) => self.replay_static_import(projections, file, &key),
            None => self.static_import_projection(file, &key),
        }?;
        let parse = self.lookup_handle(&path)?;
        Some(vec![Type::from_parse(&path, &parse, index as usize)?])
    }

    fn csharp_type(&self, file: &str, scope: &Scope, owner: &str, name: &str) -> Vec<Type<'a>> {
        let Some(project) = self.csharp_project(file) else {
            return Vec::new();
        };
        if let Some(name) = name.strip_prefix("global::") {
            return self.csharp_project_types(project, name);
        }
        if let Some((_, target)) = scope.aliases.iter().find(|(alias, _)| alias == name) {
            if target.is_empty() {
                return Vec::new();
            }
            return self.csharp_project_types(project, target.trim_start_matches("global::"));
        }
        let mut prefix = owner;
        while !prefix.is_empty() {
            let found = self.csharp_project_types(project, &join(prefix, name));
            if !found.is_empty() {
                return found;
            }
            prefix = prefix.rsplit_once('.').map_or("", |(outer, _)| outer);
        }
        // A global name in another C# compilation is not visible just because
        // its source is present. A written qualifier can name it explicitly.
        let mut found = self.csharp_project_types(project, name);
        if !name.contains('.') {
            found.retain(|candidate| candidate.file == file);
        }
        if !found.is_empty() {
            return found;
        }
        let mut seen = BTreeSet::new();
        for namespace in &scope.imports {
            // A static using lets a caller name nested types of the imported type.
            let prefix = namespace
                .trim_start_matches("static:")
                .trim_start_matches("global::");
            for candidate in self.csharp_project_types(project, &join(prefix, name)) {
                if seen.insert((candidate.file.clone(), candidate.declaration.name.clone())) {
                    found.push(candidate);
                }
            }
        }
        found
    }

    fn java_type(&self, file: &str, scope: &Scope, name: &str) -> Option<Vec<Type<'a>>> {
        if let Some((_, target)) = scope
            .aliases
            .iter()
            .find(|(alias, target)| alias == name || target == name)
        {
            let found = unambiguous(self.static_types("java", target))?;
            return (!found.is_empty() || self.absent_namespace("java", target)).then_some(found);
        }
        let package_prefix = format!("{}.", scope.namespace);
        let mut found = self.static_types(
            "java",
            &if name.starts_with(&package_prefix) {
                name.to_string()
            } else {
                join(&scope.namespace, name)
            },
        );
        found.retain(|candidate| java_package_visible(file, &candidate.file, &scope.namespace));
        if !found.is_empty() {
            return unambiguous(found);
        }
        let core = name.strip_prefix("java.lang.").unwrap_or(name);
        if java_lang_class(core) {
            let found = unambiguous(self.static_types("java", &join("java.lang", core)))?;
            return (!found.is_empty() || !self.declares_namespace("java", "java.lang"))
                .then_some(found);
        }
        None
    }

    fn csharp_closed(
        &self,
        owner: &Type<'a>,
        name: &str,
        seen: &mut BTreeSet<(String, String)>,
    ) -> bool {
        let ty = &owner.declaration;
        if ty
            .values
            .binary_search_by(|value| value.as_str().cmp(name))
            .is_ok()
        {
            return false;
        }
        let key = (owner.file.clone(), ty.name.clone());
        if !seen.insert(key.clone()) {
            return false;
        }
        let closed = ty.bases.iter().all(|base| {
            // A base expression is outside the declaring type's body, so it
            // can see the enclosing type and namespace, not its own members.
            let prefix = ty.name.rsplit_once('.').map_or("", |(prefix, _)| prefix);
            let bases = self.csharp_type(&owner.file, &owner.scope, prefix, base);
            bases.len() == 1
                && bases
                    .iter()
                    .all(|base| self.csharp_closed(base, name, seen))
        });
        seen.remove(&key);
        closed
    }

    fn java_clear_receiver_scope(&self, owner: Type<'a>, name: &str) -> bool {
        let mut seen = BTreeSet::new();
        let mut pending = vec![owner];
        while let Some(ty) = pending.pop() {
            if !seen.insert((ty.file.clone(), ty.declaration.name.clone())) {
                continue;
            }
            if ty
                .declaration
                .values
                .binary_search_by(|value| value.as_str().cmp(name))
                .is_ok()
            {
                return false;
            }
            let scope = &ty.scope;
            for base in &ty.declaration.bases {
                // An unreadable superclass could contribute a field with
                // this name. It cannot establish absence of shadowing.
                let Some(bases) = self.java_type(&ty.file, scope, base) else {
                    return false;
                };
                if bases.is_empty() {
                    return false;
                }
                pending.extend(bases);
            }
        }
        true
    }

    pub(in super::super) fn static_call(
        &self,
        file: &str,
        parse: &DispatchParse<'a>,
        receiver: u32,
        member: &str,
    ) -> Option<Resolution> {
        let language = parse.language.as_str();
        let facts = parse.dispatch.static_facts.as_deref()?;
        let receiver = &facts.receivers[receiver as usize];
        let scope = &facts.scopes[receiver.scope as usize];
        let mut pending = match language {
            "java" => {
                let types = self.java_type(file, scope, &receiver.name)?;
                if let Some(owner) = receiver.owner {
                    let root = receiver.name.split('.').next()?;
                    if !self.java_clear_receiver_scope(
                        Type::from_parse(file, parse, owner as usize)?,
                        root,
                    ) {
                        return None;
                    }
                }
                types
            }
            "csharp" => {
                let owner = &facts.types[receiver.owner? as usize];
                let project = self.csharp_project(file)?;
                self.csharp_projects?
                    .reachable(project, |key| self.depend(key))?;
                // An empty alias target means its declaration is conditional,
                // ambiguous, or unreadable. It can still hide a visible type.
                if scope.aliases.iter().any(|(alias, target)| {
                    target.is_empty()
                        && (alias == &receiver.name
                            || receiver
                                .name
                                .strip_prefix(alias)
                                .is_some_and(|rest| rest.starts_with('.')))
                }) {
                    return None;
                }
                let types =
                    unambiguous(self.csharp_type(file, scope, &owner.name, &receiver.name))?;
                if types.is_empty() {
                    let (_, target) = scope
                        .aliases
                        .iter()
                        .find(|(alias, _)| alias == &receiver.name)?;
                    // An alias referring to a declaration in another project
                    // is not proof that the caller references that project.
                    if !self
                        .static_types("csharp", target.trim_start_matches("global::"))
                        .is_empty()
                        || !self.absent_namespace("csharp", target.trim_start_matches("global::"))
                    {
                        return None;
                    }
                }
                let name = receiver
                    .name
                    .trim_start_matches("global::")
                    .split(['.', '`'])
                    .next()?;
                if !receiver.name.starts_with("global::")
                    && scope
                        .imports
                        .iter()
                        .filter_map(|import| import.strip_prefix("static:"))
                        .any(|import| {
                            let providers = self.csharp_type(file, scope, &owner.name, import);
                            providers.len() != 1
                                || providers
                                    .iter()
                                    .any(|ty| !self.csharp_closed(ty, name, &mut BTreeSet::new()))
                        })
                {
                    return None;
                }
                // Check all parts of partial types, as well as containing
                // types whose members are visible inside the nested type.
                let mut qualified_owner = owner.name.as_str();
                loop {
                    let parts = self.csharp_project_types(project, qualified_owner);
                    if parts.is_empty()
                        || parts
                            .iter()
                            .any(|part| !self.csharp_closed(part, name, &mut BTreeSet::new()))
                    {
                        return None;
                    }
                    let Some((outer, _)) = qualified_owner.rsplit_once('.') else {
                        break;
                    };
                    if outer == scope.namespace || outer.is_empty() {
                        break;
                    }
                    qualified_owner = outer;
                }
                types
            }
            _ if typescript::supported(language) => match receiver.owner {
                Some(index) => vec![Type::from_parse(file, parse, index as usize)?],
                None => self.ts_import_type(file, &receiver.name, receiver.module.as_deref()?)?,
            },
            _ => return None,
        };
        if pending.is_empty() {
            return Some(Resolution {
                external: 1,
                ..Resolution::default()
            });
        }
        let mut answer = Resolution::default();
        let mut seen = BTreeSet::new();
        while let Some(ty) = pending.pop() {
            if !seen.insert((ty.file.clone(), ty.declaration.name.clone())) {
                continue;
            }
            for (name, symbol) in &ty.declaration.methods {
                if name == member {
                    answer.protected.insert((ty.file.clone(), symbol.clone()));
                    answer.targets.insert(Target {
                        file: ty.file.clone(),
                        symbol: symbol.clone(),
                        provenance: "exact",
                    });
                }
            }
            let scope = &ty.scope;
            for base in &ty.declaration.bases {
                pending.extend(match language {
                    "java" => self.java_type(&ty.file, scope, base).unwrap_or_default(),
                    "csharp" => unambiguous(
                        self.csharp_type(
                            &ty.file,
                            scope,
                            ty.declaration
                                .name
                                .rsplit_once('.')
                                .map_or("", |(prefix, _)| prefix),
                            base,
                        ),
                    )
                    .unwrap_or_default(),
                    _ => self.ts_static_types(&ty.file, base),
                });
            }
        }
        (!answer.targets.is_empty()).then_some(answer)
    }
}

fn unambiguous(types: Vec<Type<'_>>) -> Option<Vec<Type<'_>>> {
    (types.len() <= 1).then_some(types)
}

fn java_package_visible(caller: &str, declaration: &str, package: &str) -> bool {
    let caller = Path::new(caller).parent();
    let declared = Path::new(declaration).parent();
    if caller == declared {
        return true;
    }
    fn module<'a>(mut directory: &'a Path, package: &str) -> Option<&'a Path> {
        for part in package.rsplit('.').filter(|part| !part.is_empty()) {
            if directory.file_name()? != part {
                return None;
            }
            directory = directory.parent()?;
        }
        if directory.file_name()? != "java" {
            return None;
        }
        let src = directory.parent()?.parent()?;
        (src.file_name()? == "src").then(|| src.parent()).flatten()
    }
    caller
        .and_then(|path| module(path, package))
        .is_some_and(|root| declared.and_then(|path| module(path, package)) == Some(root))
}

fn java_lang_class(name: &str) -> bool {
    matches!(
        name,
        "Boolean"
            | "Byte"
            | "Character"
            | "Class"
            | "ClassLoader"
            | "ClassValue"
            | "Compiler"
            | "Double"
            | "Enum"
            | "Float"
            | "InheritableThreadLocal"
            | "Integer"
            | "Long"
            | "Math"
            | "Module"
            | "ModuleLayer"
            | "Number"
            | "Object"
            | "Package"
            | "Process"
            | "ProcessBuilder"
            | "ProcessHandle"
            | "Record"
            | "Runtime"
            | "RuntimePermission"
            | "SecurityManager"
            | "Short"
            | "StackTraceElement"
            | "StackWalker"
            | "StrictMath"
            | "String"
            | "StringBuffer"
            | "StringBuilder"
            | "System"
            | "Thread"
            | "ThreadGroup"
            | "ThreadLocal"
            | "Throwable"
            | "Void"
    )
}
