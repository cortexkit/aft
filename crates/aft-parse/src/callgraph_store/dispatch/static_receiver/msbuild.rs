//! Only the local project markers relevant to receiver ownership.

use quick_xml::{
    events::{BytesStart, Event},
    Reader,
};

use super::csharp_projects::{AGGREGATOR, COMPILES, UNKNOWN};

pub(super) fn classify(source: &[u8]) -> Option<u8> {
    // Do not trust a prefix of a project: a later property could disable defaults.
    if source.len() as u64 > super::CSharpProjects::PROJECT_READ_LIMIT {
        return None;
    }
    let source = std::str::from_utf8(source).ok()?;
    let mut xml = Reader::from_str(source.trim_start_matches('\u{feff}'));
    xml.config_mut().check_comments = true;
    let mut seen_root = false;
    let mut stack = Vec::new();
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
                if name == b"EnableDefaultCompileItems" {
                    marker = Some(String::new());
                }
                stack.push(name);
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
                if name.as_ref() == b"EnableDefaultCompileItems" {
                    defaults = false;
                }
            }
            Event::End(_) => {
                if stack.pop()? == b"EnableDefaultCompileItems" {
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
    (seen_root && stack.is_empty()).then_some(if aggregator {
        AGGREGATOR
    } else if defaults {
        COMPILES
    } else {
        UNKNOWN
    })
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
