//! The stored form of what one file's resolution recorded: its binding and
//! its dispatch type targets, with the surface queries and answers that
//! decide whether the resolution can be reused.
//!
//! Fields derived from others ([`finish_binding`]'s summaries) are not
//! stored; decoding computes them again. A bound file surface is not stored
//! either: binding computes it from the file's own parse.

use std::collections::{BTreeMap, BTreeSet};

use aft_store::content::codec::{Decoder, Encoder};
use aft_store::content::DecodeError;

use super::super::fact_answers::{FactAnswer, FactAnswers, FactQuestion};
use super::{
    finish_binding, ConfigConsultations, DispatchTypeTargetTrace, DispatchTypeTargets,
    ViewBindingDependencies, ViewSurfaceQuery,
};

fn strings(out: &mut Encoder, values: &BTreeSet<String>) {
    out.usize(values.len());
    for value in values {
        out.str(value);
    }
}

fn string_set(input: &mut Decoder<'_>) -> Result<BTreeSet<String>, DecodeError> {
    let count = input.count()?;
    (0..count).map(|_| input.string()).collect()
}

fn config(out: &mut Encoder, value: &ConfigConsultations) {
    out.usize(value.facts.len());
    for (path, name) in &value.facts {
        out.str(path);
        out.str(name);
    }
    strings(out, &value.reads);
    out.bool(value.unattributed);
}

fn read_config(input: &mut Decoder<'_>) -> Result<ConfigConsultations, DecodeError> {
    let count = input.count()?;
    let facts = (0..count)
        .map(|_| Ok((input.string()?, input.string()?)))
        .collect::<Result<_, DecodeError>>()?;
    Ok(ConfigConsultations {
        facts,
        reads: string_set(input)?,
        unattributed: input.bool()?,
    })
}

/// A surface answer is a BLAKE3 hex digest; it is stored as its 32 bytes.
fn answer(out: &mut Encoder, value: &str) {
    match decode_hex(value) {
        Some(bytes) => {
            out.byte(0);
            out.blob(&bytes);
        }
        None => {
            out.byte(1);
            out.str(value);
        }
    }
}

fn read_answer(input: &mut Decoder<'_>) -> Result<String, DecodeError> {
    match input.byte()? {
        0 => {
            let bytes = input.blob()?;
            Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
        }
        1 => input.string(),
        _ => Err(DecodeError("unknown answer form")),
    }
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return None;
    }
    (0..32)
        .map(|index| u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).ok())
        .collect()
}

fn query(out: &mut Encoder, value: &ViewSurfaceQuery) {
    use ViewSurfaceQuery as Q;
    match value {
        Q::Language(file) => {
            out.byte(0);
            out.str(file);
        }
        Q::Module(file, module) => {
            out.byte(1);
            out.str(file);
            out.str(module);
        }
        Q::MacroUse(file) => {
            out.byte(2);
            out.str(file);
        }
        Q::Parent(file) => {
            out.byte(3);
            out.str(file);
        }
        Q::Reexports(file) => {
            out.byte(4);
            out.str(file);
        }
        Q::Node(file, symbol) => {
            out.byte(5);
            out.str(file);
            out.str(symbol);
        }
        Q::Callable(file, node) => {
            out.byte(6);
            out.str(file);
            out.str(node);
        }
        Q::Alias(file, symbol) => {
            out.byte(7);
            out.str(file);
            out.str(symbol);
        }
        Q::Export(file, symbol) => {
            out.byte(8);
            out.str(file);
            out.str(symbol);
        }
        Q::Default(file) => {
            out.byte(9);
            out.str(file);
        }
        Q::Contains(file) => {
            out.byte(10);
            out.str(file);
        }
        Q::Crate(name) => {
            out.byte(11);
            out.str(name);
        }
        Q::CrateRoot(file) => {
            out.byte(12);
            out.str(file);
        }
        Q::Inline(file, segments, symbol) => {
            out.byte(13);
            out.str(file);
            out.strs(segments);
            out.str(symbol);
        }
        Q::GoPackage(file, module) => {
            out.byte(14);
            out.str(file);
            out.opt_str(module.as_deref());
        }
    }
}

fn read_query(input: &mut Decoder<'_>) -> Result<ViewSurfaceQuery, DecodeError> {
    use ViewSurfaceQuery as Q;
    Ok(match input.byte()? {
        0 => Q::Language(input.string()?),
        1 => Q::Module(input.string()?, input.string()?),
        2 => Q::MacroUse(input.string()?),
        3 => Q::Parent(input.string()?),
        4 => Q::Reexports(input.string()?),
        5 => Q::Node(input.string()?, input.string()?),
        6 => Q::Callable(input.string()?, input.string()?),
        7 => Q::Alias(input.string()?, input.string()?),
        8 => Q::Export(input.string()?, input.string()?),
        9 => Q::Default(input.string()?),
        10 => Q::Contains(input.string()?),
        11 => Q::Crate(input.string()?),
        12 => Q::CrateRoot(input.string()?),
        13 => Q::Inline(input.string()?, input.strings()?, input.string()?),
        14 => Q::GoPackage(input.string()?, input.opt_string()?),
        _ => return Err(DecodeError("unknown surface query")),
    })
}

fn fact_answers(out: &mut Encoder, values: &FactAnswers) {
    out.usize(values.len());
    for (asked, answered) in values {
        let (code, path) = match asked {
            FactQuestion::IsFile(path) => (0, path),
            FactQuestion::IsDir(path) => (1, path),
            FactQuestion::Link(path) => (2, path),
            FactQuestion::Canonical(path) => (3, path),
            FactQuestion::ListDir(path) => (4, path),
            FactQuestion::Config(path) => (5, path),
            FactQuestion::ConfigField(path, _) => (6, path),
        };
        out.byte(code);
        out.blob(path);
        if let FactQuestion::ConfigField(_, name) = asked {
            out.str(name);
        }
        match answered {
            FactAnswer::Bool(value) => {
                out.byte(0);
                out.bool(*value);
            }
            FactAnswer::Path(None) => out.byte(1),
            FactAnswer::Path(Some(path)) => {
                out.byte(2);
                out.blob(path);
            }
            FactAnswer::Digest(digest) => {
                out.byte(3);
                out.blob(digest);
            }
        }
    }
}

fn read_fact_answers(input: &mut Decoder<'_>) -> Result<FactAnswers, DecodeError> {
    let count = input.count()?;
    (0..count)
        .map(|_| {
            let code = input.byte()?;
            let path = input.blob()?.to_vec();
            let asked = match code {
                0 => FactQuestion::IsFile(path),
                1 => FactQuestion::IsDir(path),
                2 => FactQuestion::Link(path),
                3 => FactQuestion::Canonical(path),
                4 => FactQuestion::ListDir(path),
                5 => FactQuestion::Config(path),
                6 => FactQuestion::ConfigField(path, input.string()?),
                _ => return Err(DecodeError("unknown fact question")),
            };
            let answered = match input.byte()? {
                0 => FactAnswer::Bool(input.bool()?),
                1 => FactAnswer::Path(None),
                2 => FactAnswer::Path(Some(input.blob()?.to_vec())),
                3 => FactAnswer::Digest(
                    input
                        .blob()?
                        .try_into()
                        .map_err(|_| DecodeError("a fact digest is not 16 bytes"))?,
                ),
                _ => return Err(DecodeError("unknown fact answer")),
            };
            Ok((asked, answered))
        })
        .collect()
}

fn queries(out: &mut Encoder, values: &[(ViewSurfaceQuery, String)]) {
    out.usize(values.len());
    for (asked, answered) in values {
        query(out, asked);
        answer(out, answered);
    }
}

fn read_queries(input: &mut Decoder<'_>) -> Result<Vec<(ViewSurfaceQuery, String)>, DecodeError> {
    let count = input.count()?;
    (0..count)
        .map(|_| Ok((read_query(input)?, read_answer(input)?)))
        .collect()
}

/// Writes a binding.
pub(crate) fn encode_binding(out: &mut Encoder, value: &ViewBindingDependencies) {
    out.usize(value.references.len());
    for (reference, dependencies) in &value.references {
        out.u32(*reference);
        strings(out, dependencies);
    }
    config(out, &value.binding_facts);
    config(out, &value.resolution_facts);
    strings(out, &value.binding_probes);
    strings(out, &value.resolved_dependencies);
    queries(out, &value.surface_queries);
    fact_answers(out, &value.binding_answers);
    fact_answers(out, &value.resolution_answers);
}

/// Reads a binding written by [`encode_binding`], with its summaries.
pub(crate) fn decode_binding(
    input: &mut Decoder<'_>,
) -> Result<ViewBindingDependencies, DecodeError> {
    let count = input.count()?;
    let references = (0..count)
        .map(|_| Ok((input.u32()?, string_set(input)?)))
        .collect::<Result<BTreeMap<_, _>, DecodeError>>()?;
    let mut binding = ViewBindingDependencies {
        references,
        binding_facts: read_config(input)?,
        resolution_facts: read_config(input)?,
        binding_probes: string_set(input)?,
        resolved_dependencies: string_set(input)?,
        surface_queries: read_queries(input)?,
        binding_answers: read_fact_answers(input)?,
        resolution_answers: read_fact_answers(input)?,
        ..ViewBindingDependencies::default()
    };
    finish_binding(&mut binding);
    Ok(binding)
}

/// Writes a file's dispatch type targets.
pub(crate) fn encode_dispatch(out: &mut Encoder, value: &DispatchTypeTargets) {
    out.usize(value.targets.len());
    for (name, (file, symbol)) in &value.targets {
        out.str(name);
        out.str(file);
        out.str(symbol);
    }
    queries(out, &value.trace.queries);
    strings(out, &value.trace.probes);
    fact_answers(out, &value.trace.answers);
}

/// Reads dispatch type targets written by [`encode_dispatch`].
pub(crate) fn decode_dispatch(input: &mut Decoder<'_>) -> Result<DispatchTypeTargets, DecodeError> {
    let count = input.count()?;
    let targets = (0..count)
        .map(|_| Ok((input.string()?, (input.string()?, input.string()?))))
        .collect::<Result<BTreeMap<_, _>, DecodeError>>()?;
    Ok(DispatchTypeTargets {
        targets,
        trace: DispatchTypeTargetTrace {
            queries: read_queries(input)?,
            probes: string_set(input)?,
            answers: read_fact_answers(input)?,
        },
    })
}
