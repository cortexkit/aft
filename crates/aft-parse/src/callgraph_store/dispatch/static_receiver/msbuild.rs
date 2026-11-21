//! Only the local project markers relevant to receiver ownership.

use quick_xml::{
    events::{BytesStart, Event},
    Reader,
};
use std::collections::BTreeSet;

use super::csharp_projects::{AGGREGATOR, COMPILES, UNKNOWN};

pub(super) struct Facts {
    pub kind: u8,
    pub membership_safe: bool,
    pub references: BTreeSet<String>,
    pub imports: BTreeSet<String>,
}

pub(super) fn has_project_root(source: &[u8]) -> bool {
    let prefix = source
        .strip_prefix(b"\xef\xbb\xbf")
        .unwrap_or(source)
        .trim_ascii_start();
    if !prefix.starts_with(b"<") || source.len() as u64 > super::CSharpProjects::PROJECT_READ_LIMIT
    {
        return false;
    }
    let mut xml = Reader::from_reader(prefix);
    loop {
        match xml.read_event() {
            Ok(Event::Start(tag) | Event::Empty(tag)) => return tag.name().as_ref() == b"Project",
            Ok(Event::Decl(_) | Event::Comment(_) | Event::PI(_)) => {}
            Ok(Event::Text(text)) if text.iter().all(u8::is_ascii_whitespace) => {}
            _ => return false,
        }
    }
}

pub(super) fn classify(source: &[u8]) -> Option<Facts> {
    // Do not trust a prefix of a project: a later property could disable defaults.
    if source.len() as u64 > super::CSharpProjects::PROJECT_READ_LIMIT {
        return None;
    }
    let source = std::str::from_utf8(source).ok()?;
    let mut xml = Reader::from_str(source.trim_start_matches('\u{feff}'));
    xml.config_mut().check_comments = true;
    let mut seen_root = false;
    let mut stack: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut references = BTreeSet::new();
    let mut imports = BTreeSet::new();
    let mut aggregator = false;
    let mut defaults = true;
    loop {
        match xml.read_event().ok()? {
            Event::Start(tag) => {
                let name = tag.name().as_ref().to_vec();
                let depth = stack.len();
                if depth == 0 {
                    if seen_root || name != b"Project" {
                        return None;
                    }
                    seen_root = true;
                }
                aggregator |= qualifying_sdk(&xml, &tag, depth)?;
                let (conditional, include) = item(&xml, &tag)?;
                add_reference(&mut references, &stack, &name, conditional, include);
                defaults &= membership_directive(&xml, &tag, &mut imports)?;
                stack.push((name, conditional));
            }
            Event::Empty(tag) => {
                let name = tag.name();
                if stack.is_empty() {
                    if seen_root || name.as_ref() != b"Project" {
                        return None;
                    }
                    seen_root = true;
                }
                aggregator |= qualifying_sdk(&xml, &tag, stack.len())?;
                let (conditional, include) = item(&xml, &tag)?;
                add_reference(&mut references, &stack, name.as_ref(), conditional, include);
                defaults &= membership_directive(&xml, &tag, &mut imports)?;
            }
            Event::End(_) => {
                stack.pop()?;
            }
            Event::Text(text) => {
                let text = text.xml_content().ok()?;
                if stack.is_empty() && !text.trim().is_empty() {
                    return None;
                }
            }
            Event::CData(_) => {
                if stack.is_empty() {
                    return None;
                }
            }
            Event::DocType(_) => return None,
            Event::GeneralRef(_) if stack.is_empty() => return None,
            Event::Eof => break,
            _ => {}
        }
    }
    (seen_root && stack.is_empty()).then_some(Facts {
        kind: if aggregator {
            AGGREGATOR
        } else if defaults {
            COMPILES
        } else {
            UNKNOWN
        },
        membership_safe: defaults,
        references,
        imports,
    })
}

fn membership_directive(
    xml: &Reader<&[u8]>,
    tag: &BytesStart<'_>,
    imports: &mut BTreeSet<String>,
) -> Option<bool> {
    let name = tag.local_name();
    let name = name.as_ref();
    if [
        b"Compile".as_slice(),
        b"EnableDefaultItems",
        b"EnableDefaultCompileItems",
        b"DefaultItemExcludes",
        b"DefaultExcludesInProjectFolder",
        b"DefaultItemExcludesInProjectFolder",
    ]
    .into_iter()
    .any(|directive| name.eq_ignore_ascii_case(directive))
    {
        return Some(false);
    }
    if name.eq_ignore_ascii_case(b"Import") {
        let mut project = None;
        for attr in tag.attributes() {
            let attr = attr.ok()?;
            // An SDK-qualified import is relative to the SDK, not the
            // checkout folder containing the Import.
            if attr.key.as_ref().eq_ignore_ascii_case(b"Sdk") {
                return Some(true);
            }
            if attr.key.as_ref().eq_ignore_ascii_case(b"Project") {
                project = Some(
                    attr.decode_and_unescape_value(xml.decoder())
                        .ok()?
                        .into_owned(),
                );
            }
        }
        let Some(path) = project.filter(|path| !path.contains(['\r', '\n'])) else {
            return Some(false);
        };
        imports.insert(path);
    }
    Some(true)
}

fn item(xml: &Reader<&[u8]>, tag: &BytesStart<'_>) -> Option<(bool, Option<String>)> {
    let mut conditional = false;
    let mut include = None;
    for attr in tag.attributes() {
        let attr = attr.ok()?;
        match attr.key.as_ref() {
            b"Condition" => conditional = true,
            b"Include" if tag.name().as_ref() == b"ProjectReference" => {
                include = Some(
                    attr.decode_and_unescape_value(xml.decoder())
                        .ok()?
                        .into_owned(),
                );
            }
            _ => {}
        }
    }
    Some((conditional, include))
}

fn add_reference(
    references: &mut std::collections::BTreeSet<String>,
    stack: &[(Vec<u8>, bool)],
    name: &[u8],
    conditional: bool,
    include: Option<String>,
) {
    if name != b"ProjectReference"
        || conditional
        || stack.len() != 2
        || stack[0].1
        || stack[1].0 != b"ItemGroup"
        || stack[1].1
    {
        return;
    }
    if let Some(include) = include.filter(|p| !p.contains(['\r', '\n'])) {
        references.insert(include);
    }
}

fn qualifying_sdk(xml: &Reader<&[u8]>, tag: &BytesStart<'_>, depth: usize) -> Option<bool> {
    let mut found = false;
    for attr in tag.attributes() {
        let attr = attr.ok()?;
        let value = attr.decode_and_unescape_value(xml.decoder()).ok()?;
        let key = attr.key.as_ref();
        if (depth == 0 && key == b"Sdk")
            || (depth == 1 && tag.name().as_ref() == b"Sdk" && key == b"Name")
        {
            found |= value.split(';').any(|name| {
                let name = name.trim().split('/').next().unwrap_or_default();
                name.eq_ignore_ascii_case("Microsoft.Build.NoTargets")
                    || name.eq_ignore_ascii_case("Microsoft.Build.Traversal")
            });
        }
    }
    Some(found)
}
