use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::registry::PortablePath;
use super::ProjectRegistry;

pub const SESSION_LIMIT: u64 = 64 * 1024 * 1024;
pub const MOVE_LIMIT: u64 = 256 * 1024 * 1024;
pub const ENTRY_LIMIT: usize = 10_000;
#[derive(Default)]
pub struct CarryBudget {
    bytes: u64,
    entries: usize,
}
impl CarryBudget {
    pub fn add_stream(&mut self, bytes: u64, entries: usize) -> Result<(), &'static str> {
        if bytes > SESSION_LIMIT {
            return Err("session_limit");
        }
        let total = self.bytes.checked_add(bytes).ok_or("move_limit")?;
        let count = self.entries.checked_add(entries).ok_or("entry_limit")?;
        if total > MOVE_LIMIT {
            return Err("move_limit");
        }
        if count > ENTRY_LIMIT {
            return Err("entry_limit");
        }
        self.bytes = total;
        self.entries = count;
        Ok(())
    }
}
const MAGIC: &[u8] = b"aft-session/v1\n";

struct BoundedFrame {
    bytes: Vec<u8>,
    cap: usize,
}
impl std::io::Write for BoundedFrame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.cap.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("session_limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A missing path is an explicit baseline, not a missing safety check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "blake3",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ExpectedState {
    Absent,
    File(String),
}

impl ExpectedState {
    pub fn capture(path: &Path) -> Result<Self, String> {
        use crate::backup::PathFingerprint;
        match PathFingerprint::of_path(path) {
            PathFingerprint::Absent => Ok(Self::Absent),
            PathFingerprint::Content(hash) => Ok(Self::File(hash)),
            _ => Err("unsupported_kind".into()),
        }
    }

    pub fn matches(&self, path: &Path) -> bool {
        Self::capture(path).as_ref() == Ok(self)
    }

    pub fn of_bytes(bytes: &Option<Vec<u8>>) -> Self {
        match bytes {
            None => Self::Absent,
            Some(bytes) => Self::File(blake3::hash(bytes).to_hex().to_string()),
        }
    }
}

/// Persisted with imported history so an ordinary undo cannot use the local
/// checkpoint-then-overwrite fallback for a foreign baseline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedGuard {
    pub path: PortablePath,
    pub expected: ExpectedState,
}

impl ImportedGuard {
    pub(crate) fn from_meta(value: Option<&serde_json::Value>) -> Option<Self> {
        let value = value.filter(|value| !value.is_null())?;
        Some(
            serde_json::from_value(value.clone()).unwrap_or_else(|_| Self {
                path: PortablePath {
                    project_id: String::new(),
                    root_key: cortexkit_project_identity::RootKey::new(
                        cortexkit_project_identity::RootKeyKind::Label,
                        String::new(),
                    ),
                    relative_path: String::new(),
                },
                expected: ExpectedState::File(String::new()),
            }),
        )
    }

    pub(crate) fn check(
        &self,
        path: &Path,
        registry: Option<&dyn ProjectRegistry>,
    ) -> Result<(), crate::error::AftError> {
        let result: Result<(), String> = (|| {
            let registry = registry.ok_or("registry unavailable".to_string())?;
            let roots = registry.enumerate()?;
            let resolved = super::registry::import_path(&self.path, &roots)
                .map_err(|reason| reason.reason().to_string())?;
            if resolved != path || !self.expected.matches(path) {
                return Err("postimage_mismatch".into());
            }
            Ok(())
        })();
        result.map_err(|reason| crate::error::AftError::IoError {
            path: path.display().to_string(),
            message: format!("imported history restore refused: {reason}; nothing was restored"),
        })
    }
}

/// Drop counts never enter an agent-data reply: `cortexkit-role-agent-data`
/// version 0.2.3 decodes those replies strictly.
/// `log_internal_drop_report` is the seam for a future negotiated report field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropCounts(pub BTreeMap<String, u64>);

impl DropCounts {
    pub fn add(&mut self, reason: &str, count: u64) {
        *self.0.entry(reason.to_owned()).or_default() += count;
    }

    pub fn log_internal_drop_report(&self) {
        for (reason, count) in &self.0 {
            log::info!("agent-data portable projection: dropped={count} reason={reason}");
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub path: PortablePath,
    pub id: String,
    #[serde(with = "decimal_order")]
    pub order: u128,
    pub timestamp: u64,
    pub mode: Option<u32>,
    pub before: Option<Vec<u8>>,
    pub expected: ExpectedState,
}

// Decimal text preserves the full ordering value through JSON and Serde's
// internally tagged enum representation, which does not retain u128 numbers.
mod decimal_order {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(value: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SnapshotGroup {
    Undo {
        id: String,
        files: Vec<FileRecord>,
    },
    Checkpoint {
        name: String,
        created_at: u64,
        created_order: u64,
        files: Vec<FileRecord>,
    },
}

impl SnapshotGroup {
    pub fn files(&self) -> &[FileRecord] {
        match self {
            Self::Undo { files, .. } | Self::Checkpoint { files, .. } => files,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSnapshot {
    pub groups: Vec<SnapshotGroup>,
}

impl SessionSnapshot {
    pub fn project(
        &self,
        registry: &dyn ProjectRegistry,
        drops: &mut DropCounts,
    ) -> Result<Vec<(SnapshotGroup, Vec<std::path::PathBuf>)>, String> {
        let roots = registry.enumerate()?;
        let mut simulated = BTreeMap::new();
        let mut broken = std::collections::BTreeSet::new();
        let mut retained = Vec::new();
        for group in self.groups.iter().rev() {
            let undo = matches!(group, SnapshotGroup::Undo { .. });
            let mut paths = Vec::new();
            let mut reason = None;
            for file in group.files() {
                let key = serde_json::to_string(&file.path).map_err(|error| error.to_string())?;
                if undo && broken.contains(&key) {
                    reason = Some("broken_chain");
                }
                match super::registry::import_path(&file.path, &roots) {
                    Ok(path) => paths.push(path),
                    Err(drop) => {
                        reason = Some(drop.reason());
                        break;
                    }
                }
            }
            if reason.is_none() {
                for (file, path) in group.files().iter().zip(&paths) {
                    let actual = if undo {
                        simulated.get(path).cloned()
                    } else {
                        None
                    };
                    let actual = actual.or_else(|| ExpectedState::capture(path).ok());
                    if actual.as_ref() != Some(&file.expected) {
                        reason = Some("postimage_mismatch");
                        break;
                    }
                }
            }
            if let Some(reason) = reason {
                drops.add(reason, group.files().len() as u64);
                if undo {
                    for file in group.files() {
                        broken.insert(
                            serde_json::to_string(&file.path).map_err(|error| error.to_string())?,
                        );
                    }
                }
                continue;
            }
            if undo {
                for (file, path) in group.files().iter().zip(&paths) {
                    simulated.insert(path.clone(), ExpectedState::of_bytes(&file.before));
                }
            }
            retained.push((group.clone(), paths));
        }
        retained.reverse();
        Ok(retained)
    }

    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        let mut count = 0usize;
        for group in &self.groups {
            count = count
                .checked_add(group.files().len().max(1))
                .ok_or("entry_limit")?;
            if count > ENTRY_LIMIT {
                return Err("entry_limit".into());
            }
            // The source refuses an over-limit prepare rather than silently
            // truncating a cut whose digest has already been admitted.
            let cap = (SESSION_LIMIT as usize).saturating_sub(bytes.len() + MAGIC.len() + 12);
            let mut frame = BoundedFrame {
                bytes: Vec::new(),
                cap,
            };
            serde_json::to_writer(&mut frame, group).map_err(|error| error.to_string())?;
            let frame = frame.bytes;
            if (bytes.len() as u64).saturating_add(4 + frame.len() as u64) > SESSION_LIMIT {
                return Err("session_limit".into());
            }
            let len = u32::try_from(frame.len()).map_err(|_| "session_limit")?;
            bytes.extend_from_slice(&len.to_le_bytes());
            bytes.extend(frame);
        }
        let mut framed = MAGIC.to_vec();
        framed.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        framed.extend(bytes);
        if framed.len() as u64 > SESSION_LIMIT {
            return Err("session_limit".into());
        }
        Ok(framed)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() as u64 > SESSION_LIMIT {
            return Err("session_limit".into());
        }
        let mut stream = bytes;
        let mut latest = None;
        while !stream.is_empty() {
            let body = stream.strip_prefix(MAGIC).ok_or("incompatible_version")?;
            let header = body.get(..8).ok_or("invalid_frame")?;
            let length = u64::from_le_bytes(header.try_into().map_err(|_| "invalid_frame")?);
            let length = usize::try_from(length).map_err(|_| "session_limit")?;
            let frame_end = 8usize.checked_add(length).ok_or("invalid_frame")?;
            latest = Some(body.get(8..frame_end).ok_or("invalid_frame")?);
            stream = &body[frame_end..];
        }
        let mut remaining = latest.ok_or("incompatible_version")?;
        let mut groups = Vec::new();
        let mut count = 0usize;
        while !remaining.is_empty() {
            let len = remaining.get(..4).ok_or("invalid_frame")?;
            let len = u32::from_le_bytes(len.try_into().map_err(|_| "invalid_frame")?) as usize;
            let frame = remaining
                .get(4..4usize.checked_add(len).ok_or("invalid_frame")?)
                .ok_or("invalid_frame")?;
            let group: SnapshotGroup =
                serde_json::from_slice(frame).map_err(|error| error.to_string())?;
            count = count
                .checked_add(group.files().len().max(1))
                .ok_or("entry_limit")?;
            if count > ENTRY_LIMIT {
                return Err("entry_limit".into());
            }
            groups.push(group);
            remaining = &remaining[4 + len..];
        }
        Ok(Self { groups })
    }
}
