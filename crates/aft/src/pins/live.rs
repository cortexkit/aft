//! The live pin: one per view per daemon, listing keys that are relied on but
//! not reachable from any manifest yet.
//!
//! It covers installed semantic completions not yet folded, trigram blobs of
//! live-delta entries, and segments under construction. A key is written here
//! before its put; the pin is trimmed after each successful fold. The file
//! uses the assembly-pin format (`pins/<label>.json` + `pins/<label>.keys`),
//! so the family sweep reads it exactly like an assembly pin, and it is
//! reclaimed only once its owner process is gone. Like an assembly pin, it is
//! created under the family registry's barrier: a missing-root
//! deregistration either sees it in its final protection check, or has
//! already deregistered the view and the creation fails.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    fs_lock, now_ms, pin_paths, root_cache, validate_generation, write_key_lines, write_metadata,
    PinError, PinMetadata, PinOwner, Protection,
};
use crate::blob_store::v2::{to_hex, FamilyKey};
use crate::views::registry::ViewRegistration;

/// Label prefix of live pins inside `pins/`.
pub const LIVE_PIN_PREFIX: &str = "live-";

static LIVE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct LivePin {
    metadata_path: PathBuf,
    keys_path: PathBuf,
    metadata: PinMetadata,
    keys: BTreeSet<String>,
    released: bool,
}

impl LivePin {
    /// Creates an empty live pin for this process in the view's `pins/`,
    /// under the registry barrier of `registration`. Fails with
    /// [`PinError::NotRegistered`] once the view has been deregistered.
    pub fn create(registration: &ViewRegistration) -> Result<Self, PinError> {
        registration.under_pin_barrier(|view_dir| {
            Self::create_in(
                view_dir,
                registration.family().to_owned(),
                registration.scope().to_owned(),
            )
        })
    }

    /// Creates an empty live pin in the `pins/` of the v3 member `scope`,
    /// inside the v3 registry barrier: the member must still be registered and
    /// not `evicting`, so the retention collector's final protection check
    /// either sees this pin or has already removed the member.
    pub fn create_v3(storage: &Path, family: &str, scope: &str) -> Result<Self, PinError> {
        crate::views::base_refs::with_base_barrier(storage, family, |tx| {
            let state: Option<String> = rusqlite::OptionalExtension::optional(tx.query_row(
                "SELECT state FROM members WHERE scope=?1",
                [scope],
                |r| r.get(0),
            ))?;
            if state.is_none() || state.as_deref() == Some(crate::views::registry::STATE_EVICTING) {
                return Err(crate::views::base_refs::BaseRefError::Unavailable(format!(
                    "v3 member {scope} is not registered"
                )));
            }
            let view_dir = crate::blob_store::v3::view_dir(storage, family, scope)?;
            Self::create_in(&view_dir, family.to_owned(), scope.to_owned()).map_err(|error| {
                crate::views::base_refs::BaseRefError::Io(std::io::Error::other(error.to_string()))
            })
        })
        .map_err(|error| match error {
            crate::views::base_refs::BaseRefError::Unavailable(_) => {
                PinError::NotRegistered(scope.to_owned())
            }
            other => PinError::Registry(other.to_string()),
        })
    }

    fn create_in(view_dir: &Path, family: String, view: String) -> Result<Self, PinError> {
        let pid = std::process::id();
        let now = now_ms();
        let owner = PinOwner {
            pid,
            start_time: root_cache::process_start_time_ms(pid).unwrap_or(now),
        };
        let label = format!(
            "{LIVE_PIN_PREFIX}{}-{}-{}",
            owner.pid,
            owner.start_time,
            LIVE_SEQ.fetch_add(1, Ordering::Relaxed)
        );
        validate_generation(&label)?;
        let pins_dir = view_dir.join("pins");
        crate::private_storage::create_dir_all(&pins_dir)
            .map_err(|e| crate::views::io::at("creating live pin directory", &pins_dir, e))?;
        let (metadata_path, keys_path) = pin_paths(view_dir, &label);
        let metadata = PinMetadata {
            family,
            view,
            generation: label,
            owner,
            created_at: now,
            renewed_at: now,
        };
        let pin = Self {
            metadata_path,
            keys_path,
            metadata,
            keys: BTreeSet::new(),
            released: false,
        };
        // Keys first: a sweep that finds the metadata must also find the keys.
        pin.write_keys()?;
        write_metadata(&pin.metadata_path, &pin.metadata)?;
        Ok(pin)
    }

    pub fn label(&self) -> &str {
        &self.metadata.generation
    }

    pub fn metadata(&self) -> &PinMetadata {
        &self.metadata
    }

    pub fn keys_path(&self) -> &Path {
        &self.keys_path
    }

    /// Adds keys and makes the new list visible to sweeps before returning.
    pub fn protect(&mut self, keys: &[FamilyKey]) -> Result<(), PinError> {
        let before = self.keys.len();
        self.keys.extend(keys.iter().map(FamilyKey::to_hex));
        if self.keys.len() != before {
            self.write_keys()?;
        }
        Ok(())
    }

    /// Protects a segment id (segments share the 32-byte key space).
    pub fn protect_segment(&mut self, segment_id: &[u8; 32]) -> Result<(), PinError> {
        if self.keys.insert(to_hex(segment_id)) {
            self.write_keys()?;
        }
        Ok(())
    }

    /// Keeps only the keys `keep` accepts, after a fold made the rest reachable
    /// from a published manifest.
    pub fn trim(&mut self, keep: impl Fn(&str) -> bool) -> Result<(), PinError> {
        let before = self.keys.len();
        self.keys.retain(|key| keep(key));
        if self.keys.len() != before {
            self.write_keys()?;
        }
        Ok(())
    }

    /// A completed fill may leave its live pin only after durable publication
    /// protects it. Unpublished override keys survive even if content equality
    /// removed their corresponding live entry.
    pub fn trim_published(
        &mut self,
        published: &BTreeSet<String>,
        keep: impl Fn(&str) -> bool,
    ) -> Result<(), PinError> {
        self.trim(|key| keep(key) || !published.contains(key))
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    fn write_keys(&self) -> Result<(), PinError> {
        let temporary = self.keys_path.with_extension(format!(
            "keys.tmp.{}.{}",
            std::process::id(),
            LIVE_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| -> Result<(), PinError> {
            let file = crate::private_storage::options()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|e| crate::views::io::at("creating live pin keys", &temporary, e))?;
            write_key_lines(file, self.keys.iter().map(String::as_str))
                .map_err(|e| crate::views::io::at("writing live pin keys", &temporary, e))?;
            fs_lock::rename_over(&temporary, &self.keys_path).map_err(|e| {
                crate::views::io::at(
                    &format!("renaming live pin keys {} to", temporary.display()),
                    &self.keys_path,
                    e,
                )
            })?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn release(&mut self) {
        if self.released {
            return;
        }
        let _ = fs::remove_file(&self.metadata_path);
        let _ = fs::remove_file(&self.keys_path);
        self.released = true;
    }
}

impl Protection for LivePin {
    fn durable_keys_path(&self) -> &Path {
        &self.keys_path
    }
}

impl Drop for LivePin {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn durability_live_pin_counts() {
        let view = tempfile::tempdir().unwrap();
        crate::durability::take();
        let mut pin = LivePin::create_in(view.path(), "family".into(), "view".into()).unwrap();
        let events = crate::durability::take();
        assert_eq!(crate::durability::sync_count(&events), 1, "{events:?}");
        pin.protect(&keys(10)).unwrap();
        pin.trim(|_| false).unwrap();
        pin.release();
        let events = crate::durability::take();
        assert_eq!(crate::durability::sync_count(&events), 0, "{events:?}");
    }
    use super::*;
    use crate::pins::work_counters::key_file_work;

    fn keys(count: u32) -> Vec<FamilyKey> {
        (0..count)
            .map(|index| {
                let mut bytes = [0u8; 32];
                bytes[..4].copy_from_slice(&index.to_be_bytes());
                crate::blob_store::v2::TrigramKey {
                    content: crate::blob_store::v2::ContentHash::of(&bytes),
                    policy: crate::blob_store::v2::TrigramPolicy {
                        max_file_size: 1 << 20,
                    },
                }
                .family_key()
            })
            .collect()
    }

    #[test]
    fn override_fill_stays_live_until_durable_publication_protects_it() {
        let view = tempfile::tempdir().unwrap();
        let mut pin = LivePin::create_in(view.path(), "family".into(), "worker".into()).unwrap();
        let key = keys(1)[0];
        pin.protect(&[key]).unwrap();
        pin.trim_published(&BTreeSet::new(), |_| false).unwrap();
        assert_eq!(
            pin.len(),
            1,
            "content equality cannot retire an unpublished override key"
        );
        assert!(fs::read_to_string(pin.keys_path())
            .unwrap()
            .contains(&key.to_hex()));
        pin.trim_published(&BTreeSet::from([key.to_hex()]), |_| false)
            .unwrap();
        assert!(pin.is_empty());
    }

    #[test]
    fn protecting_a_batch_writes_the_key_file_once_without_sync() {
        let view = tempfile::tempdir().unwrap();
        let mut pin = LivePin::create_in(view.path(), "family".into(), "view".into()).unwrap();
        let batch = keys(500);
        let (writes_before, syncs_before) = key_file_work();
        pin.protect(&batch).unwrap();
        let (writes_after, syncs_after) = key_file_work();
        assert_eq!(
            (writes_after - writes_before, syncs_after - syncs_before),
            (1, 0),
            "a batch of 500 keys must cost one write and no sync; keys are read only while the owner lives"
        );
        let mut expected: Vec<String> = batch.iter().map(FamilyKey::to_hex).collect();
        expected.sort();
        expected.dedup();
        let written = fs::read_to_string(pin.keys_path()).unwrap();
        assert_eq!(written.lines().collect::<Vec<_>>(), expected);

        // Re-protecting keys that are already listed writes nothing.
        pin.protect(&batch).unwrap();
        assert_eq!(key_file_work(), (writes_after, syncs_after));
    }
}
