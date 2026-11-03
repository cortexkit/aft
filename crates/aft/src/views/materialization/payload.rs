//! Stored form of the call-graph view's per-file JSON payloads.
//!
//! Resolver surfaces and bindings (`view_file_surfaces`, `view_bindings`) and
//! method-call facts, sites and type target traces (`view_dispatch_files`)
//! are repetitive JSON text: before compression they were about 300 MB of the
//! 1.9 GB call-graph base built for AFT's own repository. They are stored
//! zlib-compressed at the fastest level: a cold build packs every payload, and
//! for this text the fastest level costs a few percent more bytes than the
//! default for a fraction of the CPU. The zlib checksum makes a damaged row
//! fail to read rather than decode to different text. Everything in memory, including the digests
//! computed over the JSON text, stays the JSON text; only the stored bytes
//! change.

use std::io::{Read, Write};

use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::callgraph_store::{CallGraphStoreError, Result};

/// The stored bytes of a JSON payload.
pub(crate) fn pack(json: &str) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(
        Vec::with_capacity(json.len() / 4 + 16),
        Compression::fast(),
    );
    encoder
        .write_all(json.as_bytes())
        .expect("writing into a Vec cannot fail");
    encoder.finish().expect("writing into a Vec cannot fail")
}

/// The JSON text of stored payload bytes.
pub(crate) fn unpack(bytes: &[u8]) -> Result<String> {
    let mut json = String::new();
    ZlibDecoder::new(bytes)
        .read_to_string(&mut json)
        .map_err(|error| {
            CallGraphStoreError::Unavailable(format!("stored view payload is unreadable: {error}"))
        })?;
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_round_trip_and_shrink() {
        let json = serde_json::to_string(
            &(0..200)
                .map(|n| serde_json::json!({"path": "src/views/materialization.rs", "line": n}))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let packed = pack(&json);
        assert!(
            packed.len() * 4 < json.len(),
            "{} -> {}",
            json.len(),
            packed.len()
        );
        assert_eq!(unpack(&packed).unwrap(), json);
        assert_eq!(unpack(&pack("")).unwrap(), "");
    }

    #[test]
    fn unreadable_payloads_are_refused() {
        assert!(matches!(
            unpack(b"{\"not\":\"zlib\"}"),
            Err(CallGraphStoreError::Unavailable(_))
        ));
    }
}
