//! A fragment: one file's call-graph resolution in one checkout, as stored.
//!
//! A fragment is keyed by the file's content record and its path. It holds
//! what each reference resolved to, the method-call sites kept with their
//! resolutions, the dispatch type targets, and everything the resolution
//! recorded about its inputs, so another checkout can reuse it after
//! replaying those inputs. The caller's path and record key are the
//! fragment's key and are not repeated inside it.

use std::collections::BTreeSet;

use aft_store::content::codec::{Decoder, Encoder};
use aft_store::content::DecodeError;

use super::callgraph::{FileResolution, SiteResolution};
use crate::callgraph_store::join::dispatch::{Resolution, SiteHint, Target};
use crate::callgraph_store::join::record::{
    decode_binding, decode_dispatch, encode_binding, encode_dispatch,
};
use crate::callgraph_store::join::{BlobRefKind, DerivedRow, ResolutionStatus};

const FORMAT: u8 = 1;

const KINDS: [BlobRefKind; 9] = [
    BlobRefKind::Construction,
    BlobRefKind::CallbackRef,
    BlobRefKind::Call,
    BlobRefKind::ValueRef,
    BlobRefKind::Import,
    BlobRefKind::Module,
    BlobRefKind::Reexport,
    BlobRefKind::ExportAlias,
    BlobRefKind::MacroMention,
];

const PROVENANCES: [&str; 3] = ["exact", "dispatch", "name_match"];

fn kind_code(kind: BlobRefKind) -> u8 {
    KINDS
        .iter()
        .position(|known| *known == kind)
        .and_then(|code| u8::try_from(code).ok())
        .expect("every reference kind is listed")
}

/// Encodes `file`.
///
/// # Panics
///
/// When a method-call target has a provenance not listed in
/// [`PROVENANCES`], which a resolver change must add.
#[must_use]
pub(crate) fn encode(file: &FileResolution) -> Vec<u8> {
    let mut out = Encoder::default();
    out.byte(FORMAT);
    out.usize(file.rows.len());
    for row in &file.rows {
        out.u32(row.ref_ordinal);
        out.byte(kind_code(row.kind));
        out.bool(row.status == ResolutionStatus::Resolved);
        out.opt_str(
            row.target_path
                .as_deref()
                .map(|path| std::str::from_utf8(path).expect("checkout paths are UTF-8")),
        );
        out.opt_str(row.target_symbol.as_deref());
    }
    encode_binding(&mut out, &file.binding);
    match &file.dispatch {
        None => out.bool(false),
        Some(dispatch) => {
            out.bool(true);
            encode_dispatch(&mut out, dispatch);
        }
    }
    out.usize(file.sites.len());
    for kept in &file.sites {
        let site = &kept.site;
        out.u32(site.ordinal);
        out.opt_str(site.caller.as_deref());
        out.u32(site.line);
        out.opt_str(site.member.as_deref());
        out.opt_str(site.receiver.as_deref());
        out.bool(site.dynamic);
        let resolution = &kept.resolution;
        out.usize(resolution.targets.len());
        for target in &resolution.targets {
            out.str(&target.file);
            out.str(&target.symbol);
            out.byte(
                PROVENANCES
                    .iter()
                    .position(|known| *known == target.provenance)
                    .and_then(|code| u8::try_from(code).ok())
                    .expect("every target provenance is listed"),
            );
        }
        out.usize(resolution.protected.len());
        for (file, symbol) in &resolution.protected {
            out.str(file);
            out.str(symbol);
        }
        out.usize(resolution.unresolved);
        out.usize(resolution.external);
        out.usize(resolution.dynamic);
    }
    out.usize(file.site_dependencies.len());
    for key in &file.site_dependencies {
        out.str(key);
    }
    out.finish()
}

/// Decodes a fragment of the file at `path` whose content record has the
/// key `record_key`.
///
/// # Errors
///
/// When the bytes are not a fragment this build wrote.
pub(crate) fn decode(
    record_key: &str,
    path: &str,
    bytes: &[u8],
) -> Result<FileResolution, DecodeError> {
    let mut input = Decoder::new(bytes)?;
    if input.byte()? != FORMAT {
        return Err(DecodeError("unknown fragment format"));
    }
    // Each row takes at least five bytes (ordinal, kind, status and two
    // optional strings), so a count the bytes cannot hold fails before
    // anything is reserved.
    let count = input.count_of(5)?;
    let mut rows = Vec::with_capacity(count);
    for _ in 0..count {
        let ref_ordinal = input.u32()?;
        let kind = *KINDS
            .get(usize::from(input.byte()?))
            .ok_or(DecodeError("unknown reference kind"))?;
        let status = if input.bool()? {
            ResolutionStatus::Resolved
        } else {
            ResolutionStatus::Unresolved
        };
        rows.push(DerivedRow {
            caller_blob_key: record_key.to_string(),
            ref_ordinal,
            caller_path: path.as_bytes().to_vec(),
            kind,
            status,
            target_path: input.opt_string()?.map(String::into_bytes),
            target_symbol: input.opt_string()?,
        });
    }
    let binding = decode_binding(&mut input)?;
    let dispatch = if input.bool()? {
        Some(decode_dispatch(&mut input)?)
    } else {
        None
    };
    // At least seven bytes per site: ordinal, caller, line, member,
    // receiver, the dynamic flag and the target count.
    let count = input.count_of(7)?;
    let mut sites = Vec::with_capacity(count);
    for _ in 0..count {
        let site = SiteHint {
            ordinal: input.u32()?,
            caller: input.opt_string()?,
            line: input.u32()?,
            member: input.opt_string()?,
            receiver: input.opt_string()?,
            dynamic: input.bool()?,
        };
        let targets = (0..input.count()?)
            .map(|_| {
                Ok(Target {
                    file: input.string()?,
                    symbol: input.string()?,
                    provenance: PROVENANCES
                        .get(usize::from(input.byte()?))
                        .copied()
                        .ok_or(DecodeError("unknown target provenance"))?,
                })
            })
            .collect::<Result<_, DecodeError>>()?;
        let protected = (0..input.count()?)
            .map(|_| Ok((input.string()?, input.string()?)))
            .collect::<Result<_, DecodeError>>()?;
        sites.push(SiteResolution {
            site,
            resolution: Resolution {
                targets,
                protected,
                unresolved: input.usize()?,
                external: input.usize()?,
                dynamic: input.usize()?,
            },
        });
    }
    let site_dependencies = (0..input.count()?)
        .map(|_| input.string())
        .collect::<Result<BTreeSet<_>, _>>()?;
    input.finish()?;
    Ok(FileResolution {
        rows,
        binding,
        dispatch,
        sites,
        site_dependencies,
    })
}
