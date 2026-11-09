//! Capabilities for cross-file reads. No view exposes its underlying parse blob.

use super::{dependency, FieldHint, MethodHint, ParseBlob, Resolver, ReturnHint, TypeHint};

pub(super) struct File<'r, 'a> {
    resolver: &'r Resolver<'a>,
    path: &'r str,
    parse: &'a ParseBlob,
}

pub(super) struct NamedImport<'a> {
    pub module: &'a str,
    pub imported: Option<&'a str>,
    pub default: bool,
}

impl<'r, 'a> File<'r, 'a> {
    pub(super) fn new(resolver: &'r Resolver<'a>, path: &'r str, parse: &'a ParseBlob) -> Self {
        Self {
            resolver,
            path,
            parse,
        }
    }

    pub(super) fn language(&self) -> &'a str {
        &self.parse.language
    }
    pub(super) fn go_package(&self) -> Option<&'a str> {
        self.parse.dispatch.go_package.as_deref()
    }

    pub(super) fn type_named(&self, name: &str) -> Option<&'a TypeHint> {
        self.resolver.depend(dependency::type_name(self.path, name));
        self.parse.dispatch.types.iter().find(|ty| ty.name == name)
    }

    pub(super) fn field(&self, owner: &str, name: &str) -> Option<&'a FieldHint> {
        self.resolver
            .depend(dependency::field(self.path, owner, name));
        self.parse
            .dispatch
            .fields
            .iter()
            .find(|field| field.owner == owner && field.name == name)
    }

    pub(super) fn return_hint(&self, symbol: &str) -> Option<&'a ReturnHint> {
        self.resolver
            .depend(dependency::return_hint(self.path, symbol));
        self.parse
            .dispatch
            .returns
            .iter()
            .find(|returned| returned.symbol == symbol)
    }

    pub(super) fn default_export(&self) -> Option<&'a str> {
        self.resolver
            .depend(dependency::type_name(self.path, "@default"));
        self.parse.default_export_symbol.as_deref()
    }

    pub(super) fn imports_naming(&self, name: &str) -> Vec<NamedImport<'a>> {
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
            super::python_reexports(self.parse)
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

    pub(super) fn methods_named_here(&self, name: &str) -> Vec<&'a MethodHint> {
        self.resolver.depend(dependency::method(name));
        self.parse
            .dispatch
            .methods
            .iter()
            .filter(|method| method.name == name)
            .collect()
    }

    pub(super) fn go_methods(&self) -> &'a [MethodHint] {
        assert_eq!(self.language(), "go");
        &self.parse.dispatch.methods
    }
}

/// A method-group capability carries only facts covered by its `m:` key.
#[derive(Clone, Copy)]
pub(super) struct MethodGroup<'a> {
    parse: &'a ParseBlob,
}

impl<'a> MethodGroup<'a> {
    pub(super) fn new(parse: &'a ParseBlob) -> Self {
        Self { parse }
    }
    pub(super) fn language(&self) -> &'a str {
        &self.parse.language
    }
    pub(super) fn go_package(&self) -> Option<&'a str> {
        self.parse.dispatch.go_package.as_deref()
    }
    pub(super) fn owner_type(
        &self,
        resolver: &Resolver<'a>,
        path: &str,
        owner: &str,
    ) -> Option<&'a TypeHint> {
        // The method group's m: key covers coarse events; the local owner read
        // additionally needs its t: key for precise changes to the type.
        File::new(resolver, path, self.parse).type_named(owner)
    }

    pub(super) fn implements(
        &self,
        owner: &str,
        required: &std::collections::BTreeSet<(&str, &str)>,
    ) -> bool {
        // Every Go fact change emits the method group's key, including changes
        // to other method names. Non-Go files do not have that coarse coverage.
        assert_eq!(
            self.language(),
            "go",
            "whole-method-set reads require Go's coarse invalidation"
        );
        required.iter().all(|(name, shape)| {
            self.parse.dispatch.methods.iter().any(|method| {
                method.owner == owner && method.name == *name && method.shape == *shape
            })
        })
    }
}

/// Directory membership and a single type name cover this scan's reads.
pub(super) struct Directory<'r, 'a> {
    parse: &'a ParseBlob,
    name: &'r str,
}

impl<'r, 'a> Directory<'r, 'a> {
    pub(super) fn new(parse: &'a ParseBlob, name: &'r str) -> Self {
        Self { parse, name }
    }
    pub(super) fn language(&self) -> &'a str {
        &self.parse.language
    }
    pub(super) fn go_package(&self) -> Option<&'a str> {
        self.parse.dispatch.go_package.as_deref()
    }
    pub(super) fn types_named(&self) -> impl Iterator<Item = &'a TypeHint> + '_ {
        let name = self.name;
        self.parse
            .dispatch
            .types
            .iter()
            .filter(move |ty| ty.name == name)
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
