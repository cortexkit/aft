use std::collections::BTreeSet;

use super::super::{dependency, typescript, Resolution, Resolver, Target};
use super::extract::join;
use super::{Declaration, Scope, StaticFacts};

pub(in crate::callgraph_store) const STATIC_IMPORT_PREFIX: &str = "@static-import:";
// An import can resolve to a project module without resolving to a class.
const MISSING_STATIC_EXPORT: u32 = u32::MAX;

struct Type<'a> {
    file: String,
    facts: &'a StaticFacts,
    declaration: &'a Declaration,
}

impl<'a> Resolver<'a> {
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
        files
            .into_iter()
            .flat_map(|file| {
                let facts = self
                    .lookup(&file)
                    .filter(|parse| parse.language == language)
                    .and_then(|parse| parse.dispatch.static_facts.as_deref());
                facts.into_iter().flat_map(move |facts| {
                    let file = file.clone();
                    facts
                        .types
                        .iter()
                        .filter(move |ty| ty.name == name)
                        .map(move |declaration| Type {
                            file: file.clone(),
                            facts,
                            declaration,
                        })
                })
            })
            .collect()
    }

    fn ts_static_types(&self, file: &str, name: &str) -> Vec<Type<'a>> {
        let Some((path, ty)) = self.project_type(file, name) else {
            return Vec::new();
        };
        self.depend(dependency::file(&path));
        let Some(facts) = self
            .lookup(&path)
            .and_then(|parse| parse.dispatch.static_facts.as_deref())
        else {
            return Vec::new();
        };
        facts
            .types
            .iter()
            .filter(|declaration| declaration.name == ty.name)
            .map(|declaration| Type {
                file: path.clone(),
                facts,
                declaration,
            })
            .collect()
    }

    pub(in super::super) fn static_import_projection(
        &self,
        file: &str,
        key: &str,
    ) -> Option<(String, u32)> {
        let (path, symbol) = self.type_target(file, key)?;
        self.depend(dependency::file(&path));
        let index = self
            .lookup(&path)
            .and_then(|parse| parse.dispatch.static_facts.as_deref())
            .and_then(|facts| {
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
        let key = format!("{STATIC_IMPORT_PREFIX}{name}");
        let bound = match self.projections() {
            Some(projections) => self.replay_static_import(projections, file, &key),
            None => self.static_import_projection(file, &key),
        };
        let Some((path, index)) = bound else {
            // Relative and absolute paths, and package-internal aliases, do
            // not establish that a module lives outside this project.
            let package = !module.starts_with(['.', '/', '#']);
            return package.then(Vec::new);
        };
        let facts = self.lookup(&path)?.dispatch.static_facts.as_deref()?;
        let declaration = facts.types.get(index as usize)?;
        Some(vec![Type {
            file: path,
            facts,
            declaration,
        }])
    }

    fn csharp_type(&self, scope: &Scope, owner: &str, name: &str) -> Vec<Type<'a>> {
        if let Some(name) = name.strip_prefix("global::") {
            return self.static_types("csharp", name);
        }
        if let Some((_, target)) = scope.aliases.iter().find(|(alias, _)| alias == name) {
            return self.static_types("csharp", target.trim_start_matches("global::"));
        }
        let mut prefix = owner;
        loop {
            let found = self.static_types("csharp", &join(prefix, name));
            if !found.is_empty() {
                return found;
            }
            let Some((outer, _)) = prefix.rsplit_once('.') else {
                if !prefix.is_empty() {
                    let found = self.static_types("csharp", name);
                    if !found.is_empty() {
                        return found;
                    }
                }
                break;
            };
            prefix = outer;
        }
        let mut candidates = scope
            .imports
            .iter()
            .map(|namespace| {
                self.static_types(
                    "csharp",
                    &join(namespace.trim_start_matches("global::"), name),
                )
            })
            .filter(|found| !found.is_empty());
        let Some(found) = candidates.next() else {
            return Vec::new();
        };
        if candidates.any(|other| other[0].declaration.name != found[0].declaration.name) {
            // A lookup in two imported namespaces must not pick an arbitrary type.
            return Vec::new();
        }
        found
    }

    fn java_type(&self, scope: &Scope, name: &str) -> (Vec<Type<'a>>, bool) {
        if let Some((_, target)) = scope
            .aliases
            .iter()
            .find(|(alias, target)| alias == name || target == name)
        {
            return (self.static_types("java", target), true);
        }
        let package_prefix = format!("{}.", scope.namespace);
        let found = self.static_types(
            "java",
            &if name.starts_with(&package_prefix) {
                name.to_string()
            } else {
                join(&scope.namespace, name)
            },
        );
        if !found.is_empty() {
            return (found, true);
        }
        let core = name.strip_prefix("java.lang.").unwrap_or(name);
        if java_lang_class(core) {
            return (self.static_types("java", &join("java.lang", core)), true);
        }
        (Vec::new(), false)
    }

    fn csharp_closed(
        &self,
        owner: &Type<'a>,
        name: &str,
        seen: &mut BTreeSet<(String, String)>,
    ) -> bool {
        let ty = owner.declaration;
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
            let bases = self.csharp_type(&owner.facts.scopes[ty.scope as usize], prefix, base);
            !bases.is_empty()
                && bases
                    .iter()
                    .all(|base| self.csharp_closed(base, name, seen))
        });
        seen.remove(&key);
        closed
    }

    fn java_inherited_value(&self, owner: Type<'a>, name: &str) -> bool {
        let mut seen = BTreeSet::new();
        let mut pending = vec![owner];
        while let Some(ty) = pending.pop() {
            if !seen.insert((ty.file, ty.declaration.name.as_str())) {
                continue;
            }
            if ty
                .declaration
                .values
                .binary_search_by(|value| value.as_str().cmp(name))
                .is_ok()
            {
                return true;
            }
            let scope = &ty.facts.scopes[ty.declaration.scope as usize];
            for base in &ty.declaration.bases {
                pending.extend(self.java_type(scope, base).0);
            }
        }
        false
    }

    pub(in super::super) fn static_call(
        &self,
        file: &str,
        language: &str,
        facts: &'a StaticFacts,
        receiver: u32,
        member: &str,
    ) -> Option<Resolution> {
        let receiver = &facts.receivers[receiver as usize];
        let scope = &facts.scopes[receiver.scope as usize];
        let mut pending = match language {
            "java" => {
                let (types, proven) = self.java_type(scope, &receiver.name);
                if !proven {
                    return None;
                }
                if let Some(owner) = receiver.owner {
                    let root = receiver.name.split('.').next()?;
                    if self.java_inherited_value(
                        Type {
                            file: file.to_string(),
                            facts,
                            declaration: &facts.types[owner as usize],
                        },
                        root,
                    ) {
                        return None;
                    }
                }
                types
            }
            "csharp" => {
                let owner = &facts.types[receiver.owner? as usize];
                let types = self.csharp_type(scope, &owner.name, &receiver.name);
                if types.is_empty()
                    && !scope
                        .aliases
                        .iter()
                        .any(|(alias, _)| alias == &receiver.name)
                {
                    return None;
                }
                let name = receiver
                    .name
                    .trim_start_matches("global::")
                    .split(['.', '`'])
                    .next()?;
                // Check all parts of partial types, as well as containing
                // types whose members are visible inside the nested type.
                let mut qualified_owner = owner.name.as_str();
                loop {
                    let parts = self.static_types(language, qualified_owner);
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
                Some(index) => vec![Type {
                    file: file.to_string(),
                    facts,
                    declaration: &facts.types[index as usize],
                }],
                None => self.ts_import_type(file, &receiver.name, receiver.module.as_deref()?)?,
            },
            _ => return None,
        };
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
            let scope = &ty.facts.scopes[ty.declaration.scope as usize];
            for base in &ty.declaration.bases {
                pending.extend(match language {
                    "java" => self.java_type(scope, base).0,
                    "csharp" => self.csharp_type(
                        scope,
                        ty.declaration
                            .name
                            .rsplit_once('.')
                            .map_or("", |(prefix, _)| prefix),
                        base,
                    ),
                    _ => self.ts_static_types(&ty.file, base),
                });
            }
        }
        answer.external = usize::from(answer.targets.is_empty());
        Some(answer)
    }
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
