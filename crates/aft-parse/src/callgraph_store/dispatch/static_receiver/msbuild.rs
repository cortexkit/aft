//! Only the local project markers relevant to receiver ownership.

use quick_xml::{
    events::{BytesStart, Event},
    Reader,
};

use super::csharp_projects::{AGGREGATOR, COMPILES, UNKNOWN};

pub(super) fn classify(source: &[u8]) -> Option<(u8, std::collections::BTreeSet<String>)> {
    // Do not trust a prefix of a project: a later property could disable defaults.
    if source.len() as u64 > super::CSharpProjects::PROJECT_READ_LIMIT {
        return None;
    }
    let source = std::str::from_utf8(source).ok()?;
    let mut xml = Reader::from_str(source.trim_start_matches('\u{feff}'));
    xml.config_mut().check_comments = true;
    let mut seen_root = false;
    let mut stack: Vec<(Vec<u8>, bool)> = Vec::new();
    let mut references = std::collections::BTreeSet::new();
    let mut marker = None;
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
                if marker.is_some() {
                    return None;
                }
                aggregator |= qualifying_sdk(&xml, &tag, depth)?;
                let (conditional, include) = item(&xml, &tag)?;
                add_reference(&mut references, &stack, &name, conditional, include);
                if name == b"EnableDefaultCompileItems" {
                    marker = Some(String::new());
                }
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
                if marker.is_some() {
                    return None;
                }
                aggregator |= qualifying_sdk(&xml, &tag, stack.len())?;
                let (conditional, include) = item(&xml, &tag)?;
                add_reference(&mut references, &stack, name.as_ref(), conditional, include);
                if name.as_ref() == b"EnableDefaultCompileItems" {
                    defaults = false;
                }
            }
            Event::End(_) => {
                if stack.pop()?.0 == b"EnableDefaultCompileItems" {
                    defaults &= marker.take()?.trim().eq_ignore_ascii_case("true");
                }
            }
            Event::Text(text) => {
                let text = text.xml_content().ok()?;
                if stack.is_empty() && !text.trim().is_empty() {
                    return None;
                }
                if let Some(marker) = &mut marker {
                    marker.push_str(&text);
                }
            }
            Event::CData(text) => {
                if stack.is_empty() {
                    return None;
                }
                if let Some(marker) = &mut marker {
                    marker.push_str(&text.decode().ok()?);
                }
            }
            Event::DocType(_) => return None,
            Event::GeneralRef(_) if marker.is_some() || stack.is_empty() => return None,
            Event::Eof => break,
            _ => {}
        }
    }
    (seen_root && stack.is_empty()).then_some((
        if aggregator {
            AGGREGATOR
        } else if defaults {
            COMPILES
        } else {
            UNKNOWN
        },
        references,
    ))
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
