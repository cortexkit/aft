//! The daemon's policy root, cached beside the signed manifest for child shims.

use super::{manifest_acceptance_lock, unix_seconds, ManifestProblem, StatePaths};
use aft_base::durability::{atomic_write, sync_dir, AtomicWriteDurability};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use std::{fs, io};

mod history;
use history::History;

pub(super) const FILE_NAME: &str = "trust-root.json";
pub const GET: &str = "policy_root.get";
pub(crate) const ANCHOR_ENV: &str = "AFT_GH_SHIM_TRUST_ANCHOR";
pub(super) const HIGH_WATER: &str = "trust-root-generation.json";
const MAX_AGE: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
struct DaemonState {
    history: Option<History>,
    read_at: Option<Instant>,
    root: Option<StoredRoot>,
}

#[cfg(not(test))]
static DAEMON: Mutex<DaemonState> = Mutex::new(DaemonState {
    history: None,
    read_at: None,
    root: None,
});

#[cfg(test)]
static DAEMON: std::sync::LazyLock<
    Mutex<std::collections::HashMap<std::path::PathBuf, DaemonState>>,
> = std::sync::LazyLock::new(Mutex::default);

fn daemon_state<T>(_paths: &StatePaths, f: impl FnOnce(&mut DaemonState) -> T) -> T {
    let mut daemon = DAEMON
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    #[cfg(not(test))]
    return f(&mut daemon);
    #[cfg(test)]
    f(daemon.entry(_paths.root.clone()).or_default())
}

#[derive(Deserialize)]
#[serde(tag = "op")]
pub enum Reply {
    #[serde(rename = "policy_root.get")]
    Root {
        #[serde(deserialize_with = "Option::deserialize")]
        pin: Option<Pin>,
        generation: u64,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Pin {
    pub(super) key_id: String,
    pub(super) public_key_hex: String,
    pub(super) retired_key_ids: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(super) struct StoredRoot {
    #[serde(flatten)]
    pub(super) pin: Pin,
    pub(super) generation: u64,
    read_at_unix_secs: u64,
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
struct Anchor {
    key_id: String,
    public_key_hex: String,
    generation: u64,
}

impl StoredRoot {
    fn anchor(&self) -> Anchor {
        Anchor {
            key_id: self.pin.key_id.clone(),
            public_key_hex: self.pin.public_key_hex.clone(),
            generation: self.generation,
        }
    }
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
    read_snapshot(paths).map(|root| root.pin)
}

pub(super) fn read_snapshot(paths: &StatePaths) -> Result<StoredRoot, ManifestProblem> {
    if paths.daemon_trust {
        return daemon_root(paths).ok_or(ManifestProblem::NoTrustRoot);
    }
    let anchor = std::env::var(ANCHOR_ENV)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    #[cfg(test)]
    let anchor = test_anchor(paths).or(anchor);
    read_file(paths, anchor.as_ref(), unix_seconds())
}

fn read_file(
    paths: &StatePaths,
    anchor: Option<&Anchor>,
    now: u64,
) -> Result<StoredRoot, ManifestProblem> {
    let anchor = anchor.ok_or(ManifestProblem::NoTrustRoot)?;
    let invalid = || ManifestProblem::Invalid("invalid daemon signing root".into());
    let bytes = match fs::read(&paths.trust_root) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(ManifestProblem::NoTrustRoot);
        }
        Err(_) => return Err(invalid()),
    };
    let root: StoredRoot = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if root.anchor() != *anchor
        || !now
            .checked_sub(root.read_at_unix_secs)
            .is_some_and(|age| age < MAX_AGE.as_secs())
    {
        return Err(ManifestProblem::NoTrustRoot);
    }
    root.pin.public_key().ok_or_else(invalid)?;
    Ok(root)
}

fn daemon_root(paths: &StatePaths) -> Option<StoredRoot> {
    daemon_state(paths, |state| {
        if state.read_at.is_some_and(|at| at.elapsed() >= MAX_AGE) {
            state.root = None;
        }
        state.root.clone()
    })
}

pub(super) fn read_daemon(paths: &StatePaths) -> Result<Pin, ManifestProblem> {
    daemon_root(paths)
        .map(|root| root.pin)
        .ok_or(ManifestProblem::NoTrustRoot)
}

pub(crate) fn shell_anchor() -> Option<String> {
    let root = daemon_root(&StatePaths::from_process())?;
    // Do not give the shell an anchor for a key the daemon lists as retired.
    if root.pin.retired_key_ids.contains(&root.pin.key_id) {
        return None;
    }
    serde_json::to_string(&root.anchor()).ok()
}

#[cfg(unix)]
pub(super) fn seed_worker(paths: &StatePaths, destination: &std::path::Path) -> io::Result<()> {
    if let Some(root) = daemon_root(paths) {
        atomic_write(
            &destination.join(FILE_NAME),
            &serde_json::to_vec(&root)?,
            AtomicWriteDurability::None,
        )?;
    }
    Ok(())
}

pub(super) fn publish(paths: &StatePaths, reply: Reply, now: u64) -> io::Result<()> {
    let Reply::Root { pin, generation } = reply;
    if pin.as_ref().is_some_and(|pin| pin.public_key().is_none()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid policy root key",
        ));
    }
    // A publisher cannot change trust while another process is accepting a list.
    let _lock = manifest_acceptance_lock(paths)?;
    let marker = paths.root.join(HIGH_WATER);
    let (persisted, corrupt) = match fs::read(&marker) {
        Ok(bytes) => match serde_json::from_slice::<History>(&bytes) {
            Ok(history) if history.valid() => (Some(history), false),
            _ => (None, true),
        },
        Err(err) if err.kind() == io::ErrorKind::NotFound => (None, false),
        Err(err) => return Err(err),
    };
    let root = pin.map(|pin| StoredRoot {
        pin,
        generation,
        read_at_unix_secs: now,
    });
    daemon_state(paths, |state| -> io::Result<()> {
        let mut history = state
            .history
            .clone()
            .or_else(|| persisted.clone())
            .unwrap_or_default();
        if let Some(disk) = &persisted {
            history.merge(disk);
        }
        let reset = history.advance(root.as_ref().map(|root| &root.pin), generation)?;
        // With no observed pin, a null reply's generation is only a daemon
        // settings version, not evidence that a root key was provisioned.
        let write_marker = corrupt
            || (persisted.as_ref() != Some(&history) && (persisted.is_some() || history.had_pin()));
        if write_marker {
            if let Err(error) = atomic_write(
                &marker,
                &serde_json::to_vec(&history)?,
                AtomicWriteDurability::FileAndDirectory,
            ) {
                // A newer reply invalidates the old pin, but the new pin
                // cannot be accepted until its generation is durable.
                state.root = None;
                return Err(error);
            }
            if corrupt {
                log::warn!("GitHub policy root generation record was corrupt; rebuilt it from the daemon reply");
            }
            if reset {
                log::warn!("The daemon returned a new GitHub policy root at a lower generation; reset the generation floor");
            }
        }
        state.history = Some(history);
        state.read_at = Some(Instant::now());
        state.root = root.clone();
        match &root {
            None => unlink(paths),
            Some(root) => atomic_write(
                &paths.trust_root,
                &serde_json::to_vec(root)?,
                AtomicWriteDurability::FileAndDirectory,
            ),
        }
    })
}

fn unlink(paths: &StatePaths) -> io::Result<()> {
    match fs::remove_file(&paths.trust_root) {
        Ok(()) => sync_dir(&paths.root),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
fn test_anchor(paths: &StatePaths) -> Option<Anchor> {
    daemon_root(paths).map(|root| root.anchor())
}

#[cfg(all(test, unix))]
pub(crate) fn seed_fixture(root: &std::path::Path) {
    super::tests::write_fixture_pin(&StatePaths::from_root(root.to_path_buf()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gh_shim_known_pin_history_refuses_rollback_but_new_roots_reset_the_floor() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(dir.path().to_owned());
        let old = super::super::tests::fixture_pin();
        let mut current = super::super::tests::pin_for(&[31; 32]);
        let retired = super::super::tests::pin_for(&[32; 32]);
        current.retired_key_ids.push(retired.key_id.clone());
        let fresh = super::super::tests::pin_for(&[33; 32]);
        let floor = || {
            serde_json::from_slice::<History>(&fs::read(paths.root.join(HIGH_WATER)).unwrap())
                .unwrap()
                .generation
        };
        let publish_pin =
            |pin, generation| publish(&paths, Reply::Root { pin, generation }, unix_seconds());
        publish_pin(Some(old.clone()), 8).unwrap();
        publish_pin(Some(current.clone()), 9).unwrap();
        daemon_state(&paths, |state| *state = DaemonState::default());
        assert!(publish_pin(Some(old), 8).is_err());
        assert!(publish_pin(Some(retired), 8).is_err());
        publish_pin(Some(current.clone()), 7).unwrap();
        assert_eq!(read_daemon(&paths).unwrap(), current);
        assert_eq!(floor(), 9);
        publish_pin(None, 6).unwrap();
        assert_eq!(floor(), 9);
        assert!(matches!(
            read_daemon(&paths),
            Err(ManifestProblem::NoTrustRoot)
        ));
        assert!(publish_pin(Some(current), 8).is_err());
        publish_pin(Some(fresh.clone()), 3).unwrap();
        assert_eq!(read_daemon(&paths).unwrap(), fresh);
        assert_eq!(floor(), 3);
    }

    #[test]
    fn gh_shim_corrupt_generation_marker_recovers_from_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(dir.path().to_owned());
        fs::write(paths.root.join(HIGH_WATER), b"{not a generation").unwrap();
        let pin = super::super::tests::fixture_pin();
        publish(
            &paths,
            Reply::Root {
                pin: Some(pin.clone()),
                generation: 8,
            },
            unix_seconds(),
        )
        .unwrap();
        assert_eq!(read_daemon(&paths).unwrap(), pin);
        let marker: serde_json::Value =
            serde_json::from_slice(&fs::read(paths.root.join(HIGH_WATER)).unwrap()).unwrap();
        assert_eq!(marker["generation"], 8);
    }

    #[test]
    fn gh_shim_later_head_gets_the_daemon_anchor_not_a_replaced_cache() {
        let _state = crate::test_env::gh_shim_state_guard();
        let paths = StatePaths::from_process();
        let now = unix_seconds();
        let trusted = super::super::tests::fixture_pin();
        publish(
            &paths,
            Reply::Root {
                pin: Some(trusted.clone()),
                generation: 5,
            },
            now,
        )
        .unwrap();
        let seed = [31; 32];
        let forged = super::super::tests::pin_for(&seed);
        let list = super::super::tests::signed_with(
            &super::super::tests::fixture_manifest(),
            now,
            &seed,
            &forged.key_id,
        );
        fs::write(&paths.manifest, serde_json::to_vec(&list).unwrap()).unwrap();
        fs::write(
            &paths.trust_root,
            serde_json::to_vec(&StoredRoot {
                pin: forged,
                generation: 5,
                read_at_unix_secs: now,
            })
            .unwrap(),
        )
        .unwrap();
        let mut environment = std::collections::HashMap::new();
        let mut config = crate::config::Config::default();
        config.github.shim = true;
        config.git.co_author = "off".into();
        crate::agent_child_env::inject(&config, &paths.root, &mut environment, None).unwrap();
        let launch_anchor: Anchor = serde_json::from_str(&environment[ANCHOR_ENV]).unwrap();
        assert_eq!(launch_anchor.key_id, trusted.key_id);
        assert!(matches!(
            read_file(&paths, Some(&launch_anchor), now),
            Err(ManifestProblem::NoTrustRoot)
        ));
        assert_eq!(read_daemon(&paths).unwrap(), trusted);
        assert!(super::super::load_manifest(&paths.for_daemon(), now).is_err());
    }

    #[test]
    fn gh_shim_expires_both_daemon_and_file_trust_after_fifteen_minutes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(dir.path().to_owned());
        let now = unix_seconds();
        let pin = super::super::tests::fixture_pin();
        publish(
            &paths,
            Reply::Root {
                pin: Some(pin.clone()),
                generation: 1,
            },
            now,
        )
        .unwrap();
        let anchor = test_anchor(&paths).unwrap();
        assert_eq!(
            read_file(&paths, Some(&anchor), now + MAX_AGE.as_secs() - 1)
                .unwrap()
                .pin,
            pin
        );
        assert!(matches!(
            read_file(&paths, Some(&anchor), now + MAX_AGE.as_secs()),
            Err(ManifestProblem::NoTrustRoot)
        ));
        daemon_state(&paths, |state| {
            state.read_at = Some(Instant::now() - MAX_AGE)
        });
        assert!(matches!(
            read_daemon(&paths),
            Err(ManifestProblem::NoTrustRoot)
        ));
    }

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
        assert!(matches!(
            read_file(&paths, None, super::super::unix_seconds()),
            Err(ManifestProblem::NoTrustRoot)
        ));
    }

    #[test]
    fn gh_shim_null_pin_does_not_create_evidence_until_a_pin_has_been_seen() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::from_root(dir.path().to_owned());
        let now = super::super::unix_seconds();
        let reply = |pin, generation| Reply::Root { pin, generation };
        let marker = paths.root.join(HIGH_WATER);
        publish(&paths, reply(None, 7), now).unwrap();
        assert!(!paths.trust_root.exists());
        assert!(
            !marker.exists(),
            "overall settings generation does not provision the shim"
        );
        let pin = super::super::tests::fixture_pin();
        publish(
            &paths,
            Reply::Root {
                pin: Some(pin.clone()),
                generation: 8,
            },
            now,
        )
        .unwrap();
        daemon_state(&paths, |state| *state = DaemonState::default());
        publish(&paths, reply(None, 7), now).unwrap();
        assert!(!paths.trust_root.exists());
        let history: History = serde_json::from_slice(&fs::read(marker).unwrap()).unwrap();
        assert_eq!(history.generation, 8);
        daemon_state(&paths, |state| *state = DaemonState::default());
        assert!(publish(
            &paths,
            Reply::Root {
                pin: Some(pin),
                generation: 7
            },
            now
        )
        .is_err());
    }

    #[test]
    fn gh_shim_rejects_a_pin_whose_id_does_not_match_its_public_key() {
        let mut pin = super::super::tests::fixture_pin();
        pin.key_id = "0000000000000000".into();
        assert!(pin.public_key().is_none());
    }
}
