//! Resolve compiler inputs only when the source proves every concrete path.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use super::{canonical, expired};
use serde::{Deserialize, Serialize};

const INCLUDE_PROVISIONAL: &str = "cargo check completed, but {file} includes a file whose path can't be resolved; diagnostics are provisional";
const ENV_PROVISIONAL: &str = "cargo check completed, but {file} reads an environment variable whose name can't be resolved; diagnostics are provisional";

pub(super) struct UnresolvedInput {
    path: PathBuf,
    environment: bool,
}

impl UnresolvedInput {
    pub(super) fn note(&self, root: &Path) -> String {
        let file = self.path.strip_prefix(root).unwrap_or(&self.path);
        let template = if self.environment {
            ENV_PROVISIONAL
        } else {
            INCLUDE_PROVISIONAL
        };
        template.replace("{file}", &file.to_string_lossy().replace('\\', "/"))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
enum Token {
    Literal(String),
    Text(String),
    Group(Vec<Token>),
}

impl Token {
    fn text(&self, expected: &str) -> bool {
        matches!(self, Self::Text(text) if text == expected)
    }
}

fn tokens(node: tree_sitter::Node<'_>, bytes: &[u8], deadline: Instant) -> Option<Vec<Token>> {
    let mut result = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        expired(deadline)?;
        let text = child.utf8_text(bytes).ok()?;
        match child.kind() {
            "line_comment" | "block_comment" | "(" | ")" | "[" | "]" | "{" | "}" => {}
            "string_literal" => result.push(Token::Literal(string_literal(text, deadline)?)),
            "raw_string_literal" => result.push(Token::Literal(
                text.get(text.find('"')? + 1..text.rfind('"')?)?
                    .replace("\r\n", "\n"),
            )),
            "token_tree" | "token_tree_pattern" => {
                result.push(Token::Group(tokens(child, bytes, deadline)?))
            }
            "token_binding_pattern" => result.extend(tokens(child, bytes, deadline)?),
            _ if child.named_child_count() > 0 && child.kind() != "scoped_identifier" => {
                result.push(Token::Group(tokens(child, bytes, deadline)?));
            }
            _ => result.push(Token::Text(text.into())),
        }
    }
    Some(result)
}

fn arguments(tokens: &[Token], deadline: Instant) -> Option<Vec<&[Token]>> {
    let mut args = Vec::new();
    let mut start = 0;
    for (index, token) in tokens.iter().enumerate() {
        expired(deadline)?;
        if token.text(",") {
            if start == index {
                return None;
            }
            args.push(&tokens[start..index]);
            start = index + 1;
        }
    }
    if start < tokens.len() {
        args.push(&tokens[start..]);
    }
    Some(args)
}

fn manifest_dir(path: &Path, deadline: Instant) -> Option<String> {
    for parent in path.parent()?.ancestors() {
        expired(deadline)?;
        match fs::symlink_metadata(parent.join("Cargo.toml")) {
            Ok(meta) if meta.is_file() => return Some(parent.to_str()?.into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return None,
        }
    }
    None
}

fn expression(
    tokens: &[Token],
    path: &Path,
    bindings: &BTreeMap<String, String>,
    deadline: Instant,
) -> Option<String> {
    expired(deadline)?;
    match tokens {
        [Token::Literal(value)] => Some(value.clone()),
        [Token::Text(name)] => bindings.get(name).cloned(),
        [name, bang, Token::Group(args)] if name.text("concat") && bang.text("!") => {
            let mut value = String::new();
            for arg in arguments(args, deadline)? {
                value.push_str(&expression(arg, path, bindings, deadline)?);
            }
            Some(value)
        }
        [name, bang, Token::Group(args)] if name.text("env") && bang.text("!") => {
            match arguments(args, deadline)?.as_slice() {
                [[Token::Literal(key)]] if key == "CARGO_MANIFEST_DIR" => {
                    manifest_dir(path, deadline)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

// Canonicalization alone would hide a symlink in an include's parent directory.
// Check every component first, including components cancelled by `..`.
fn include_path(root: &Path, source: &Path, relative: &str, deadline: Instant) -> Option<PathBuf> {
    let joined = source.parent()?.join(relative);
    let mut current = PathBuf::new();
    for component in joined.components() {
        expired(deadline)?;
        current.push(component);
        if fs::symlink_metadata(&current).ok()?.is_symlink() {
            return None;
        }
    }
    let path = canonical(&joined)?;
    (path.starts_with(root) && path.is_file()).then_some(path)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Definition {
    name: String,
    path: PathBuf,
    parameters: Option<Vec<String>>,
    includes: Vec<(Vec<Token>, bool)>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Invocation {
    name: String,
    path: PathBuf,
    args: Vec<Token>,
    nested: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct FileFacts {
    definitions: Vec<Definition>,
    invocations: Vec<Invocation>,
    references: BTreeSet<String>,
    direct: Vec<(Vec<Token>, bool)>,
    env_keys: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct CachedFacts(Arc<FileFacts>);

impl Serialize for CachedFacts {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CachedFacts {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        FileFacts::deserialize(deserializer).map(|facts| Self(Arc::new(facts)))
    }
}

impl CachedFacts {
    pub(super) fn map_paths(
        &mut self,
        map: &mut impl FnMut(PathBuf) -> Result<PathBuf, &'static str>,
    ) -> Result<(), &'static str> {
        let facts = Arc::make_mut(&mut self.0);
        for definition in &mut facts.definitions {
            definition.path = map(definition.path.clone())?;
        }
        for invocation in &mut facts.invocations {
            invocation.path = map(invocation.path.clone())?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct SourceInputs {
    pub(super) facts: BTreeMap<PathBuf, CachedFacts>,
    rust_sources: BTreeSet<PathBuf>,
    parsed: BTreeSet<PathBuf>,
    pending: BTreeMap<PathBuf, Option<Vec<u8>>>,
    searched_names: BTreeSet<String>,
}

impl FileFacts {
    fn refuse<T>(
        path: &Path,
        environment: bool,
        unresolved: &mut Option<UnresolvedInput>,
    ) -> Option<T> {
        *unresolved = Some(UnresolvedInput {
            path: path.into(),
            environment,
        });
        None
    }
}

impl SourceInputs {
    pub(super) fn new(searched_names: BTreeSet<String>) -> Self {
        Self {
            searched_names,
            ..Self::default()
        }
    }
    pub(super) fn needs_parse(&self, path: &Path) -> bool {
        path.extension().is_some_and(|extension| extension == "rs")
            || (!self.parsed.contains(path) && self.rust_sources.contains(path))
    }
}

#[cfg(test)]
thread_local! { static PARSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
pub(super) fn take_parses() -> usize {
    PARSES.with(|count| count.replace(0))
}

impl FileFacts {
    fn parse(
        &mut self,
        root: &Path,
        path: &Path,
        bytes: &[u8],
        deadline: Instant,
        unresolved: &mut Option<UnresolvedInput>,
    ) -> Option<(Vec<PathBuf>, Vec<String>)> {
        std::str::from_utf8(bytes).ok()?;
        #[cfg(test)]
        PARSES.with(|count| count.set(count.get() + 1));
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .ok()?;
        let mut progress = |_: &tree_sitter::ParseState| {
            if Instant::now() >= deadline {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        };
        let tree = parser.parse_with_options(
            &mut |offset, _| bytes.get(offset..).unwrap_or_default(),
            None,
            Some(tree_sitter::ParseOptions::new().progress_callback(&mut progress)),
        )?;
        let mut nodes = vec![tree.root_node()];
        let mut included = Vec::new();
        let mut env_keys = Vec::new();
        while let Some(node) = nodes.pop() {
            expired(deadline)?;
            if node.kind() == "macro_definition" {
                let name = node
                    .child_by_field_name("name")?
                    .utf8_text(bytes)
                    .ok()?
                    .into();
                let mut cursor = node.walk();
                let mut rule = None;
                let mut single = true;
                for child in node.named_children(&mut cursor) {
                    expired(deadline)?;
                    if child.kind() == "macro_rule" {
                        if rule.is_some() {
                            single = false;
                        }
                        rule = Some(child);
                    }
                }
                let mut definition = Definition {
                    name,
                    path: path.into(),
                    parameters: None,
                    includes: Vec::new(),
                };
                if single {
                    if let Some(left) = rule.and_then(|rule| rule.child_by_field_name("left")) {
                        definition.parameters =
                            parameters(&tokens(left, bytes, deadline)?, deadline);
                    }
                }
                let index = self.definitions.len();
                self.definitions.push(definition);
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    expired(deadline)?;
                    if child.kind() == "macro_rule" {
                        let right = child.child_by_field_name("right")?;
                        self.scan(
                            root,
                            path,
                            &tokens(right, bytes, deadline)?,
                            Some(index),
                            false,
                            deadline,
                            &mut included,
                            &mut env_keys,
                            unresolved,
                        )?;
                    }
                }
                continue;
            }
            if node.kind() == "macro_invocation" {
                self.scan(
                    root,
                    path,
                    &tokens(node, bytes, deadline)?,
                    None,
                    false,
                    deadline,
                    &mut included,
                    &mut env_keys,
                    unresolved,
                )?;
                continue;
            }
            if node.kind() == "identifier" {
                self.references.insert(node.utf8_text(bytes).ok()?.into());
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                expired(deadline)?;
                nodes.push(child);
            }
        }
        self.env_keys = env_keys.clone();
        // An include-bearing macro invoked in this file with an unsupported
        // matcher or a nested call is already uncertain. More files cannot
        // resolve that call, so refuse before hashing the rest of the tree.
        for definition in &self.definitions {
            expired(deadline)?;
            if definition.includes.is_empty() {
                continue;
            }
            for invocation in &self.invocations {
                expired(deadline)?;
                if invocation.name == definition.name
                    && (definition.parameters.is_none() || invocation.nested)
                {
                    return Self::refuse(&definition.path, false, unresolved);
                }
            }
        }
        Some((included, env_keys))
    }

    #[allow(clippy::too_many_arguments)]
    fn scan(
        &mut self,
        root: &Path,
        path: &Path,
        tokens: &[Token],
        definition: Option<usize>,
        nested: bool,
        deadline: Instant,
        included: &mut Vec<PathBuf>,
        env_keys: &mut Vec<String>,
        unresolved: &mut Option<UnresolvedInput>,
    ) -> Option<()> {
        let mut index = 0;
        while index < tokens.len() {
            expired(deadline)?;
            if tokens[index].text("macro_rules") {
                if let Some(definition) = definition {
                    // A generated macro has its own bindings and expansion
                    // sites; substituting the outer bindings would be a guess.
                    self.definitions[definition].parameters = None;
                }
            }
            if let [Token::Text(name), bang, Token::Group(args), ..] = &tokens[index..] {
                if bang.text("!") {
                    let name = name.rsplit("::").next()?;
                    match name {
                        "include" | "include_str" | "include_bytes" => {
                            if !environment_inputs(args, env_keys, deadline)? {
                                return Self::refuse(path, true, unresolved);
                            }
                            let arguments = arguments(args, deadline)?;
                            let [args] = arguments.as_slice() else {
                                return Self::refuse(path, false, unresolved);
                            };
                            // A prefix is not containment: an unknown tail can
                            // contain `..`. Expand proven literal calls instead.
                            if let Some(definition) = definition {
                                if nested {
                                    self.definitions[definition].parameters = None;
                                }
                                self.definitions[definition]
                                    .includes
                                    .push((args.to_vec(), name == "include"));
                            } else {
                                self.direct.push((args.to_vec(), name == "include"));
                                let empty = BTreeMap::new();
                                let target =
                                    expression(args, path, &empty, deadline).and_then(|relative| {
                                        include_path(root, path, &relative, deadline)
                                    });
                                match target {
                                    Some(target) => {
                                        included.push(target);
                                    }
                                    None => {
                                        expired(deadline)?;
                                        return Self::refuse(path, false, unresolved);
                                    }
                                }
                            }
                        }
                        "env" | "option_env" => {
                            let args = arguments(args, deadline)?;
                            match args.first().copied() {
                                Some([Token::Literal(key)]) => env_keys.push(key.clone()),
                                _ => return Self::refuse(path, true, unresolved),
                            }
                        }
                        _ => {
                            self.invocations.push(Invocation {
                                name: name.into(),
                                path: path.into(),
                                args: args.clone(),
                                nested: nested || definition.is_some(),
                            });
                            self.scan(
                                root, path, args, definition, true, deadline, included, env_keys,
                                unresolved,
                            )?;
                        }
                    }
                    index += 3;
                    continue;
                }
            }
            if let Token::Group(group) = &tokens[index] {
                self.scan(
                    root, path, group, definition, nested, deadline, included, env_keys, unresolved,
                )?;
            } else if let Token::Text(name) = &tokens[index] {
                self.references.insert(name.rsplit("::").next()?.into());
            }
            index += 1;
        }
        Some(())
    }
}

impl SourceInputs {
    pub(super) fn macro_names(&self) -> BTreeSet<String> {
        self.facts
            .values()
            .flat_map(|facts| &facts.0.definitions)
            .filter(|definition| !definition.includes.is_empty())
            .map(|definition| definition.name.clone())
            .collect()
    }

    pub(super) fn add(
        &mut self,
        root: &Path,
        path: &Path,
        bytes: Option<Vec<u8>>,
        cached: Option<CachedFacts>,
        deadline: Instant,
        unresolved: &mut Option<UnresolvedInput>,
    ) -> Option<(Vec<PathBuf>, Vec<String>)> {
        if !path.extension().is_some_and(|extension| extension == "rs") {
            self.parsed.insert(path.into());
        }
        if let Some(cached) = cached {
            return self.load(root, path, cached, deadline, unresolved);
        }
        if let Some(bytes) = bytes {
            let text = std::str::from_utf8(&bytes).ok()?;
            if text.contains("include") || text.contains("env") {
                let mut facts = FileFacts::default();
                facts.parse(root, path, &bytes, deadline, unresolved)?;
                return self.load(
                    root,
                    path,
                    CachedFacts(Arc::new(facts)),
                    deadline,
                    unresolved,
                );
            }
            self.pending.insert(path.into(), Some(bytes));
        }
        Some((Vec::new(), Vec::new()))
    }

    fn load(
        &mut self,
        root: &Path,
        path: &Path,
        facts: CachedFacts,
        deadline: Instant,
        unresolved: &mut Option<UnresolvedInput>,
    ) -> Option<(Vec<PathBuf>, Vec<String>)> {
        let mut included = Vec::new();
        for (input, rust) in &facts.0.direct {
            expired(deadline)?;
            let target = expression(input, path, &BTreeMap::new(), deadline)
                .and_then(|relative| include_path(root, path, &relative, deadline));
            let Some(target) = target else {
                expired(deadline)?;
                return FileFacts::refuse(path, false, unresolved);
            };
            if *rust {
                self.rust_sources.insert(target.clone());
            }
            included.push(target);
        }
        let env_keys = facts.0.env_keys.clone();
        self.facts.insert(path.into(), facts);
        Some((included, env_keys))
    }

    pub(super) fn scan_pending(
        &mut self,
        root: &Path,
        files: &BTreeMap<PathBuf, super::Stamp>,
        deadline: Instant,
        unresolved: &mut Option<UnresolvedInput>,
    ) -> Option<()> {
        let names = self.macro_names();
        let added: BTreeSet<_> = names.difference(&self.searched_names).cloned().collect();
        if !added.is_empty() {
            for path in files.keys() {
                expired(deadline)?;
                if (path.extension().is_some_and(|ext| ext == "rs")
                    || self.rust_sources.contains(path))
                    && !self.facts.contains_key(path)
                {
                    self.pending.entry(path.clone()).or_insert(None);
                }
            }
        }
        let pending = std::mem::take(&mut self.pending);
        for (path, bytes) in pending {
            expired(deadline)?;
            let candidates = if bytes.is_some() { &names } else { &added };
            if candidates.is_empty() {
                continue;
            }
            let bytes = bytes.or_else(|| super::read_bounded(&path, deadline))?;
            let observed = super::stamp(&std::fs::metadata(&path).ok()?)?;
            let expected = files.get(&path)?;
            if (observed.size, observed.modified, observed.changed)
                != (expected.size, expected.modified, expected.changed)
            {
                return None;
            }
            let text = std::str::from_utf8(&bytes).ok()?;
            let mut relevant = false;
            for name in candidates {
                expired(deadline)?;
                // Also match bare names: an import alias or forwarding macro
                // can hide a call without spelling `name!` in the same file.
                if text.contains(name) {
                    relevant = true;
                    break;
                }
            }
            if relevant {
                if blake3::hash(&bytes).to_hex().as_str() != expected.hash {
                    return None;
                }
                let mut facts = FileFacts::default();
                facts.parse(root, &path, &bytes, deadline, unresolved)?;
                self.load(
                    root,
                    &path,
                    CachedFacts(Arc::new(facts)),
                    deadline,
                    unresolved,
                )?;
            }
        }
        self.searched_names.extend(names);
        Some(())
    }

    pub(super) fn resolve(
        &mut self,
        root: &Path,
        deadline: Instant,
        unresolved: &mut Option<UnresolvedInput>,
    ) -> Option<Vec<PathBuf>> {
        let mut included = Vec::new();
        for definition in self.facts.values().flat_map(|facts| &facts.0.definitions) {
            expired(deadline)?;
            if definition.includes.is_empty() {
                continue;
            }
            for facts in self.facts.values() {
                expired(deadline)?;
                // Passing a macro by name (including a `use ... as ...`
                // alias) can hide calls from this deliberately small resolver.
                if facts.0.references.contains(&definition.name) {
                    return FileFacts::refuse(&definition.path, false, unresolved);
                }
            }
            for invocation in self.facts.values().flat_map(|facts| &facts.0.invocations) {
                expired(deadline)?;
                if invocation.name != definition.name {
                    continue;
                }
                let Some(parameters) = definition
                    .parameters
                    .as_ref()
                    .filter(|_| !invocation.nested)
                else {
                    return FileFacts::refuse(&definition.path, false, unresolved);
                };
                let Some(args) = arguments(&invocation.args, deadline)
                    .filter(|args| args.len() == parameters.len())
                else {
                    expired(deadline)?;
                    return FileFacts::refuse(&definition.path, false, unresolved);
                };
                let mut bindings = BTreeMap::new();
                for (parameter, arg) in parameters.iter().zip(args) {
                    expired(deadline)?;
                    let [Token::Literal(value)] = arg else {
                        return FileFacts::refuse(&definition.path, false, unresolved);
                    };
                    bindings.insert(parameter.clone(), value.clone());
                }
                for (input, rust_source) in &definition.includes {
                    expired(deadline)?;
                    let target = expression(input, &invocation.path, &bindings, deadline).and_then(
                        |relative| include_path(root, &invocation.path, &relative, deadline),
                    );
                    match target {
                        Some(target) => {
                            if *rust_source {
                                self.rust_sources.insert(target.clone());
                            }
                            included.push(target);
                        }
                        None => {
                            expired(deadline)?;
                            return FileFacts::refuse(&definition.path, false, unresolved);
                        }
                    }
                }
            }
        }
        Some(included)
    }
}

fn parameters(tokens: &[Token], deadline: Instant) -> Option<Vec<String>> {
    let mut names = Vec::new();
    for arg in arguments(tokens, deadline)? {
        expired(deadline)?;
        match arg {
            [Token::Text(name), colon, kind]
                if name.starts_with('$')
                    && colon.text(":")
                    && (kind.text("literal") || kind.text("expr")) =>
            {
                if names.contains(name) {
                    return None;
                }
                names.push(name.clone());
            }
            _ => return None,
        }
    }
    Some(names)
}

fn environment_inputs(tokens: &[Token], keys: &mut Vec<String>, deadline: Instant) -> Option<bool> {
    let mut index = 0;
    while index < tokens.len() {
        expired(deadline)?;
        if let [name, bang, Token::Group(args), ..] = &tokens[index..] {
            if (name.text("env") || name.text("option_env")) && bang.text("!") {
                match arguments(args, deadline)?.first().copied() {
                    Some([Token::Literal(key)]) => keys.push(key.clone()),
                    _ => return Some(false),
                }
                index += 3;
                continue;
            }
        }
        if let Token::Group(args) = &tokens[index] {
            if !environment_inputs(args, keys, deadline)? {
                return Some(false);
            }
        }
        index += 1;
    }
    Some(true)
}

fn string_literal(text: &str, deadline: Instant) -> Option<String> {
    let normalized = text
        .strip_prefix('"')?
        .strip_suffix('"')?
        .replace("\r\n", "\n");
    let mut chars = normalized.chars().peekable();
    let mut value = String::new();
    while let Some(ch) = chars.next() {
        expired(deadline)?;
        if ch != '\\' {
            value.push(ch);
            continue;
        }
        value.push(match chars.next()? {
            '\\' => '\\',
            '\'' => '\'',
            '"' => '"',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '0' => '\0',
            'x' => {
                let code = chars.next()?.to_digit(16)? * 16 + chars.next()?.to_digit(16)?;
                if code > 0x7f {
                    return None;
                }
                char::from_u32(code)?
            }
            'u' => {
                if chars.next()? != '{' {
                    return None;
                }
                let mut code = 0u32;
                let mut digits = 0;
                loop {
                    expired(deadline)?;
                    match chars.next()? {
                        '}' if digits > 0 => break,
                        '_' if digits > 0 => {}
                        ch => {
                            digits += 1;
                            if digits > 6 {
                                return None;
                            }
                            code = code.checked_mul(16)?.checked_add(ch.to_digit(16)?)?;
                        }
                    }
                }
                char::from_u32(code)?
            }
            '\n' => {
                while chars
                    .peek()
                    .is_some_and(|ch| matches!(ch, ' ' | '\t' | '\r' | '\n'))
                {
                    expired(deadline)?;
                    chars.next();
                }
                continue;
            }
            _ => return None,
        });
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_invoked_matcher_refuses_during_file_scan() {
        let mut inputs = SourceInputs::default();
        let mut unresolved = None;
        let bytes = br#"macro_rules! many { ($($name:literal),*) => { $(include_str!(concat!("data/", $name)))* }; } many!("a.txt");"#;
        assert!(inputs
            .add(
                Path::new("workspace"),
                Path::new("workspace/vectors.rs"),
                Some(bytes.to_vec()),
                None,
                Instant::now() + std::time::Duration::from_secs(2),
                &mut unresolved
            )
            .is_none());
        assert!(unresolved.is_some());
        assert!(inputs.facts.is_empty());
    }

    #[test]
    fn literal_macro_matcher_binds_its_metavariable() {
        let bytes = br#"macro_rules! fixture { ($fixture:literal) => { include_str!(concat!("data/", $fixture)) }; }"#;
        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(bytes, None).unwrap();
        let definition = tree.root_node().named_child(0).unwrap();
        let mut cursor = definition.walk();
        let rule = definition
            .named_children(&mut cursor)
            .find(|node| node.kind() == "macro_rule")
            .unwrap();
        let left = rule.child_by_field_name("left").unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        let tokens = tokens(left, bytes, deadline).unwrap();
        assert_eq!(
            parameters(&tokens, deadline),
            Some(vec!["$fixture".into()]),
            "{}: {tokens:?}",
            left.to_sexp()
        );
    }
}
