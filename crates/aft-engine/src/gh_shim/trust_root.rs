//! The daemon's policy root, cached beside the signed manifest for child shims.

use super::{manifest_acceptance_lock, ManifestProblem, StatePaths};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, io};

pub(super) const FILE_NAME: &str = "trust-root.json";
pub const GET: &str = "policy_root.get";

#[derive(Deserialize)]
#[serde(tag = "op")]
pub enum Reply {
    #[serde(rename = "policy_root.get")]
    Root { pin: Option<Pin>, generation: u64 },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Pin {
    pub(super) key_id: String,
    pub(super) public_key_hex: String,
    pub(super) retired_key_ids: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct StoredRoot {
    #[serde(flatten)]
    pin: Pin,
    generation: u64,
    read_at_unix_secs: u64,
}

impl Pin {
    pub(super) fn public_key(&self) -> Option<[u8; 32]> {
        let mut key = [0; 32];
        let hex = self.public_key_hex.as_bytes();
        if hex.len() != key.len() * 2 || self.key_id.is_empty() {
            return None;
        }
        let digit = |byte| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        for (out, pair) in key.iter_mut().zip(hex.chunks_exact(2)) {
            *out = (digit(pair[0])? << 4) | digit(pair[1])?;
        }
        (self.key_id == key_id(&key)).then_some(key)
    }
}

pub(super) fn key_id(public_key: &[u8]) -> String {
    // The daemon's policy_root key ID is the first eight bytes of SHA-256.
    Sha256::digest(public_key)[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn read(paths: &StatePaths) -> Result<Pin, ManifestProblem> {
    let invalid = || ManifestProblem::Invalid("invalid daemon signing root".into());
    let bytes = match fs::read(&paths.trust_root) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ManifestProblem::NoTrustRoot);
        }
        Err(_) => return Err(invalid()),
    };
    let root: StoredRoot = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    root.pin.public_key().ok_or_else(invalid)?;
    Ok(root.pin)
}

pub(super) fn publish(paths: &StatePaths, reply: Reply, now: u64) -> io::Result<()> {
    let Reply::Root { pin, generation } = reply;
    if pin.is_none() && !paths.root.exists() {
        return Ok(());
    }
    // A publisher cannot change trust while another process is accepting a list.
    let _lock = manifest_acceptance_lock(paths)?;
    match pin {
        None => match fs::remove_file(&paths.trust_root) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        },
        Some(pin) => {
            let bytes = serde_json::to_vec(&StoredRoot {
                pin,
                generation,
                read_at_unix_secs: now,
            })?;
            aft_base::durability::atomic_write(
                &paths.trust_root,
                &bytes,
                aft_base::durability::AtomicWriteDurability::FileAndDirectory,
            )
        }
    }
}

#[cfg(all(test, unix))]
pub(crate) fn seed_fixture(root: &std::path::Path) {
    super::tests::write_fixture_pin(&StatePaths::from_root(root.to_path_buf()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_shim_does_not_trust_a_root_file_without_the_shell_anchor() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(dir.path().to_owned());
        publish(
            &paths,
            Reply::Root {
                pin: Some(super::super::tests::fixture_pin()),
                generation: 5,
            },
            super::super::unix_seconds(),
        )
        .unwrap();
        assert!(matches!(read(&paths), Err(ManifestProblem::NoTrustRoot)));
    }

    #[test]
    fn gh_shim_does_not_replace_persisted_generation_with_an_older_reply() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(dir.path().to_owned());
        let now = super::super::unix_seconds();
        let pin = super::super::tests::fixture_pin();
        publish(
            &paths,
            Reply::Root {
                pin: Some(pin),
                generation: 8,
            },
            now,
        )
        .unwrap();
        assert!(publish(
            &paths,
            Reply::Root {
                pin: None,
                generation: 7
            },
            now
        )
        .is_err());
        assert!(paths.trust_root.exists());
    }

    #[test]
    fn gh_shim_rejects_a_pin_whose_id_does_not_match_its_public_key() {
        let mut pin = super::super::tests::fixture_pin();
        pin.key_id = "0000000000000000".into();
        assert!(pin.public_key().is_none());
    }
}
