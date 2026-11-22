//! Capabilities for cross-file reads. No view exposes its underlying parse blob.

use super::{dependency, DispatchParse, MethodHint, ParseBlob, Resolver, TypeHint};
use std::borrow::Cow;

pub(super) struct File<'r, 'a> {
    resolver: &'r Resolver<'a>,
    path: &'r str,
    parse: DispatchParse<'a>,
}

pub(super) struct NamedImport<'a> {
    pub module: &'a str,
    pub imported: Option<&'a str>,
    pub default: bool,
}

impl<'r, 'a> File<'r, 'a> {
    pub(super) fn new(resolver: &'r Resolver<'a>, path: &'r str, parse: DispatchParse<'a>) -> Self {
        Self {
            resolver,
            path,
            parse,
        }
    }

    pub(super) fn language(&self) -> &str {
        &self.parse.language
    }
    pub(super) fn go_package(&self) -> Option<&str> {
        self.parse.dispatch.go_package.as_deref()
    }

    pub(super) fn type_named(&self, name: &str) -> Option<Cow<'a, TypeHint>> {
        self.resolver.depend(dependency::type_name(self.path, name));
        self.parse
            .select(|parse| parse.dispatch.types.iter().find(|ty| ty.name == name))
    }

    pub(super) fn field_type(&self, owner: &str, name: &str) -> Option<Cow<'a, String>> {
        self.resolver
            .depend(dependency::field(self.path, owner, name));
        self.parse.select(|parse| {
            parse
                .dispatch
                .fields
                .iter()
                .find(|field| field.owner == owner && field.name == name)
                .map(|field| &field.ty)
        })
    }

    pub(super) fn return_type(&self, symbol: &str) -> Option<Cow<'a, String>> {
        self.resolver
            .depend(dependency::return_hint(self.path, symbol));
        self.parse.select(|parse| {
            parse
                .dispatch
                .returns
                .iter()
                .find(|returned| returned.symbol == symbol)
                .map(|returned| &returned.ty)
        })
    }

    pub(super) fn default_export(&self) -> Option<&str> {
        self.resolver
            .depend(dependency::type_name(self.path, "@default"));
        self.parse.default_export_symbol.as_deref()
    }

    pub(super) fn imports_naming(&self, name: &str) -> Vec<NamedImport<'_>> {
        self.resolver.depend(dependency::type_name(self.path, name));
        self.parse
            .imports
            .iter()
            .filter_map(|import| {
                let imported = import
                    .names
                    .iter()
                    .find(|n| n.split_whitespace().last() == Some(name));
                let default = import.default_import.as_deref() == Some(name);
                (imported.is_some() || default).then(|| NamedImport {
                    module: &import.module_path,
                    imported: imported.and_then(|n| n.split_whitespace().next()),
                    default,
                })
            })
            .collect()
    }

    pub(super) fn import_target(&self, name: &str, module: &str) -> Option<String> {
        self.resolver.depend(dependency::type_name(self.path, name));
        self.resolver.import_target(self.path, module)
    }

    pub(super) fn python_imports_naming(&self, name: &str) -> Vec<(String, Vec<String>)> {
        self.resolver.depend(dependency::type_name(self.path, name));
        let imports = if self.parse.refs.is_empty() {
            self.parse
                .imports
                .iter()
                .map(|i| (i.module_path.clone(), i.names.clone()))
                .collect()
        } else {
            super::python_reexports(&self.parse)
        };
        imports
            .into_iter()
            .filter_map(|(module, names)| {
                let names: Vec<_> = names
                    .into_iter()
                    .filter(|s| crate::imports::specifier_local_name(s) == name)
                    .collect();
                (!names.is_empty()).then_some((module, names))
            })
            .collect()
    }

    pub(super) fn methods_named_here(&self, name: &str) -> Vec<&MethodHint> {
        self.resolver.depend(dependency::method(name));
        self.parse
            .dispatch
            .methods
            .iter()
            .filter(|method| method.name == name)
            .collect()
    }

    pub(super) fn go_methods(&self) -> &[MethodHint] {
        assert_eq!(self.language(), "go");
        &self.parse.dispatch.methods
    }
}

test_support_visibility! {
/// A method-group capability carries only facts covered by its `m:` key.
#[derive(Clone)]
pub(super) enum MethodGroup<'a> {
    Borrowed(&'a ParseBlob),
    Projected { language: String, go_package: Option<String> },
}
}

impl<'a> MethodGroup<'a> {
    test_support_visibility! {
    pub(super) fn new(parse: &'a ParseBlob) -> Self {
        Self::Borrowed(parse)
    }
    }

    pub(super) fn projected(parse: &ParseBlob) -> Self {
        Self::Projected {
            language: parse.language.clone(),
            go_package: parse.dispatch.go_package.clone(),
        }
    }

    pub(super) fn language(&self) -> &str {
        match self {
            Self::Borrowed(parse) => &parse.language,
            Self::Projected { language, .. } => language,
        }
    }

    pub(super) fn go_package(&self) -> Option<&str> {
        match self {
            Self::Borrowed(parse) => parse.dispatch.go_package.as_deref(),
            Self::Projected { go_package, .. } => go_package.as_deref(),
        }
    }

    fn parse(&self, resolver: &Resolver<'a>, path: &str) -> Option<DispatchParse<'a>> {
        match self {
            Self::Borrowed(parse) => Some(DispatchParse::Borrowed(parse)),
            Self::Projected { .. } => resolver.lookup_handle(path),
        }
    }

    test_support_visibility! {
    pub(super) fn owner_type(
        &self,
        resolver: &Resolver<'a>,
        path: &str,
        owner: &str,
    ) -> Option<Cow<'a, TypeHint>> {
        // The method group's m: key covers coarse events; the local owner read
        // additionally needs its t: key for precise changes to the type.
        File::new(resolver, path, self.parse(resolver, path)?).type_named(owner)
    }
    }

    pub(super) fn implements_at(
        &self,
        resolver: &Resolver<'a>,
        path: &str,
        owner: &str,
        required: &std::collections::BTreeSet<(&str, &str)>,
    ) -> bool {
        self.parse(resolver, path)
            .is_some_and(|parse| Self::implements_in(&parse, owner, required))
    }

    #[cfg(feature = "test-support")]
    pub fn implements(&self, owner: &str, required: &std::collections::BTreeSet<(&str, &str)>) -> bool {
        let Self::Borrowed(parse) = self else {
            unreachable!("this test helper requires a borrowed parse")
        };
        Self::implements_in(parse, owner, required)
    }

    fn implements_in(parse: &ParseBlob, owner: &str, required: &std::collections::BTreeSet<(&str, &str)>) -> bool {
        // Every Go fact change emits the method group's key, including changes
        // to other method names. Non-Go files do not have that coarse coverage.
        assert_eq!(parse.language, "go", "whole-method-set reads require Go's coarse invalidation");
        required.iter().all(|(name, shape)| {
            parse.dispatch.methods.iter().any(|method| {
                method.owner == owner && method.name == *name && method.shape == *shape
            })
        })
    }
}

/// Directory membership and a single type name cover this scan's reads.
pub(super) struct Directory<'r, 'a> {
    parse: DispatchParse<'a>,
    name: &'r str,
}

impl<'r, 'a> Directory<'r, 'a> {
    pub(super) fn new(parse: DispatchParse<'a>, name: &'r str) -> Self {
        Self { parse, name }
    }
    pub(super) fn language(&self) -> &str {
        &self.parse.language
    }
    pub(super) fn go_package(&self) -> Option<&str> {
        self.parse.dispatch.go_package.as_deref()
    }
    pub(super) fn types_named(&self) -> impl Iterator<Item = Cow<'a, TypeHint>> + '_ {
        let name = self.name;
        let (pinned, evictable): (&[TypeHint], &[TypeHint]) = match &self.parse {
            DispatchParse::Borrowed(parse) => (&parse.dispatch.types, &[]),
            DispatchParse::Shared(_) => (&[], &self.parse.dispatch.types),
        };
        pinned
            .iter()
            .filter(move |ty| ty.name == name)
            .map(Cow::Borrowed)
            .chain(
                evictable
                    .iter()
                    .filter(move |ty| ty.name == name)
                    .cloned()
                    .map(Cow::Owned),
            )
    }
}

pub(super) fn methods(parse: &ParseBlob) -> impl Iterator<Item = (MethodGroup<'_>, &MethodHint)> {
    parse
        .dispatch
        .methods
        .iter()
        .map(|method| (MethodGroup::new(parse), method))
}

pub(super) fn go_interface_requires(
    resolver: &Resolver<'_>,
    parse: &ParseBlob,
    name: &str,
    shape: &str,
) -> bool {
    resolver.depend(dependency::GO_INTERFACES.to_string());
    parse.language == "go"
        && parse.dispatch.types.iter().any(|ty| {
            ty.interface
                && parse
                    .dispatch
                    .methods
                    .iter()
                    .any(|m| m.owner == ty.name && m.name == name && m.shape == shape)
        })
}
