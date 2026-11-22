//! Text-only admission for session reads, inside the carried call's policy.

use std::io::Read;
use std::path::Path;

use serde_json::Value;

use crate::{context::AppContext, protocol::Response};

pub(super) fn preflight(
    ctx: &AppContext,
    id: &str,
    session: &str,
    name: &str,
    args: &Value,
) -> Result<(), Response> {
    let targets: Vec<&str> = match name {
        "read" => args["path"].as_str().into_iter().collect(),
        "outline" => args["target"].as_str().into_iter().collect(),
        "zoom" => args["batch"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["target"].as_str())
            .collect(),
        _ => return Ok(()),
    };
    for target in targets {
        // Authorization must precede even a header read: otherwise the media
        // refusal would disclose the type of a file outside the session fence.
        let path = ctx.validate_read_path(id, session, Path::new(target))?;
        let kind =
            crate::bounded_io::run(&path, None, |path| file_kind(&path)).map_err(|error| {
                Response::error(
                    id,
                    crate::bounded_io::timeout_code(&error),
                    "File inspection failed.",
                )
            })?;
        if let Some(code) = kind.and_then(|kind| super::operations::media_refusal(name, kind)) {
            return Err(Response::error(
                id,
                code,
                super::operations::fixed_message(code),
            ));
        }
    }
    Ok(())
}

fn file_kind(path: &Path) -> std::io::Result<Option<&'static str>> {
    if !std::fs::metadata(path)?.is_file() {
        return Ok(None);
    }
    let mut file = crate::task_temp_reads::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        let version = versions::Version::of(&metadata);
        if let Some(kind) = versions::get(version) {
            return Ok(kind);
        }
        let kind = scan_kind(&mut file, metadata.len())?;
        if file
            .metadata()
            .is_ok_and(|after| versions::Version::of(&after) == version)
        {
            versions::remember(version, kind);
        }
        Ok(kind)
    }
    #[cfg(not(unix))]
    scan_kind(&mut file, metadata.len())
}

fn scan_kind(file: &mut impl Read, size: u64) -> std::io::Result<Option<&'static str>> {
    let mut bytes = [0u8; 8192];
    let len = size.min(bytes.len() as u64) as usize;
    file.read_exact(&mut bytes[..len])?;
    #[cfg(test)]
    SCANNED_BYTES.with(|count| count.set(count.get() + len));
    let prefix = &bytes[..len];
    // Match the native read handler's content signatures, not extensions:
    // even a text-looking GIF or PDF must not become a text-only success.
    if prefix.starts_with(b"\x89PNG\r\n\x1a\n")
        || prefix.starts_with(b"\xff\xd8\xff")
        || prefix.starts_with(b"GIF8")
        || (prefix.len() >= 12 && prefix.starts_with(b"RIFF") && &prefix[8..12] == b"WEBP")
    {
        return Ok(Some("image"));
    }
    if prefix.starts_with(b"%PDF-") {
        return Ok(Some("pdf"));
    }
    if content_inspector::inspect(prefix).is_binary() {
        return Ok(Some("binary"));
    }
    // Invalid UTF-8 beyond the sniffed prefix must not become a binary summary
    // or a lossy text success. Scan in fixed space under the call's I/O budget,
    // carrying an incomplete character into the next read rather than refusing
    // valid text whose character happens to cross a buffer boundary.
    let mut len = len;
    loop {
        let carry = match std::str::from_utf8(&bytes[..len]) {
            Ok(_) => 0,
            Err(error) if error.error_len().is_some() => return Ok(Some("binary")),
            Err(error) => len - error.valid_up_to(),
        };
        bytes.copy_within(len - carry..len, 0);
        match file.read(&mut bytes[carry..]) {
            Ok(0) => return Ok((carry != 0).then_some("binary")),
            Ok(read) => {
                #[cfg(test)]
                SCANNED_BYTES.with(|count| count.set(count.get() + read));
                len = carry + read;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => len = carry,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(unix)]
mod versions {
    use std::{collections::VecDeque, os::unix::fs::MetadataExt, sync::Mutex};

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) struct Version([u64; 7]);

    impl Version {
        pub(super) fn of(metadata: &std::fs::Metadata) -> Self {
            // As for backup's DiskFileVersion, a restored mtime cannot hide
            // writes from ctime, and replacements change the file identity.
            Self([
                metadata.dev(),
                metadata.ino(),
                metadata.len(),
                metadata.mtime() as u64,
                metadata.mtime_nsec() as u64,
                metadata.ctime() as u64,
                metadata.ctime_nsec() as u64,
            ])
        }
    }

    type Kind = Option<&'static str>;
    const MAX_VERSIONS: usize = 64;
    static RECENT: Mutex<VecDeque<(Version, Kind)>> = Mutex::new(VecDeque::new());

    pub(super) fn get(version: Version) -> Option<Kind> {
        let mut recent = RECENT.lock().ok()?;
        let index = recent
            .iter()
            .position(|(candidate, _)| *candidate == version)?;
        let entry = recent.remove(index)?;
        recent.push_front(entry);
        Some(entry.1)
    }

    pub(super) fn remember(version: Version, kind: Kind) {
        if let Ok(mut recent) = RECENT.lock() {
            if recent.iter().any(|(candidate, _)| *candidate == version) {
                return;
            }
            if recent.len() == MAX_VERSIONS {
                recent.pop_back();
            }
            recent.push_front((version, kind));
        }
    }
}

#[cfg(test)]
thread_local! { static SCANNED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn pipeline_review_session_read_preflight_reuses_only_the_same_file_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("range.txt");
        let mut contents = b"app range-read fixture\n".repeat(8 * 1024);
        std::fs::write(&path, &contents).unwrap();
        SCANNED_BYTES.with(|count| count.set(0));
        assert_eq!(file_kind(&path).unwrap(), None);
        assert_eq!(SCANNED_BYTES.with(|count| count.replace(0)), contents.len());
        assert_eq!(file_kind(&path).unwrap(), None);
        assert_eq!(SCANNED_BYTES.with(|count| count.replace(0)), 0);

        let replacement = dir.path().join("replacement.txt");
        *contents.last_mut().unwrap() = 0xff;
        std::fs::write(&replacement, &contents).unwrap();
        let modified =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&path).unwrap());
        filetime::set_file_mtime(&replacement, modified).unwrap();
        std::fs::rename(replacement, &path).unwrap();
        assert_eq!(file_kind(&path).unwrap(), Some("binary"));
    }
}
