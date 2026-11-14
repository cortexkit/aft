//! Stream immutable graph output; only the file table and sort budget grow
//! resident. Terms use the shared checksummed external sorter.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use super::Scratch;
use super::{
    DATA, FragmentKey, INDEX, SLICE_DATA, SLICE_INDEX, TERM_LOOKUP, TERM_POSTINGS, TermKind,
    UNIT_KEY_SPACE, UNITS, layout, slice_spec, spec, term_hash,
};
use crate::ids::Digest128;
use crate::packed::{PackedCopy, StoredUnit};
use crate::postings::PostingsBuilder;
use crate::segment::{ErrorKind, SegmentError, SegmentWriter};
use crate::spill::{Config, Sorter, WriteGuard};
use crate::table::{LookupTableBuilder, UnitLocation, UnitTableBuilder};

/// Packed on the resolver thread, before the append lock is taken.
#[derive(Debug)]
pub struct PackedFragment {
    data: StoredUnit,
    slice: StoredUnit,
    terms: Vec<(TermKind, u64)>,
}

impl PackedFragment {
    pub fn new<'a>(
        bytes: &[u8],
        slice: &[u8],
        terms: impl IntoIterator<Item = (TermKind, &'a [u8])>,
    ) -> Self {
        let mut terms = terms
            .into_iter()
            .map(|(kind, term)| (kind, term_hash(term)))
            .collect::<Vec<_>>();
        terms.sort_unstable();
        terms.dedup();
        Self {
            data: StoredUnit::packed(bytes),
            slice: StoredUnit::packed(slice),
            terms,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Placed {
    range: usize,
    offset: u64,
    data: (u8, u32, u32),
    slice: (u8, u32, u32),
}

/// Writes into one folder of the build's shared workspace. Dropping the spill,
/// even on an error or a disk-floor pause, deletes nothing: the build deletes
/// the workspace after publication, and the family writer's sweep deletes
/// those of builds that are no longer live.
#[derive(Debug)]
pub struct GraphSectionsSpill {
    data: Option<BufWriter<File>>,
    range_hash: blake3::Hasher,
    range_bytes: u64,
    ranges: Vec<u64>,
    range_files: usize,
    terms: Option<Sorter>,
    units: BTreeMap<FragmentKey, Placed>,
    len: u64,
    guard: WriteGuard,
    workspace: Box<dyn Scratch>,
    space: Option<SpaceEstimate>,
    term_count: u64,
}

#[derive(Debug)]
struct SpaceEstimate {
    files: u64,
    baseline: u64,
    remaining: Arc<AtomicU64>,
}

impl GraphSectionsSpill {
    pub fn new(
        workspace: Box<dyn Scratch>,
        config: Config,
        guard: WriteGuard,
    ) -> Result<Self, crate::spill::Error> {
        guard.check()?;
        let mut terms = Sorter::ephemeral_fixed_keys(workspace.path(), config, 25)?;
        terms.set_write_guard(guard.clone())?;
        Ok(Self {
            data: None,
            range_hash: blake3::Hasher::new(),
            range_bytes: 0,
            ranges: Vec::new(),
            range_files: 0,
            terms: Some(terms),
            units: BTreeMap::new(),
            len: 0,
            guard,
            workspace,
            space: None,
            term_count: 0,
        })
    }

    /// Reserve both scratch and the final segment while both coexist. The
    /// initial source-size estimate is refined from completed files as output
    /// arrives; every run write checks the caller's updated disk policy.
    pub fn estimate_space(&mut self, files: usize, baseline: u64, remaining: Arc<AtomicU64>) {
        self.space = Some(SpaceEstimate {
            files: files as u64,
            baseline,
            remaining,
        });
    }

    pub fn prepare_finish(&self) -> io::Result<()> {
        if let Some(space) = &self.space {
            // Packed units plus a conservative allowance for postings, lookup
            // tables and a temporary posting section: the lookup section must
            // precede its postings to preserve the existing segment bytes.
            space.remaining.store(
                self.len
                    .saturating_add(self.term_count.saturating_mul(25))
                    .saturating_mul(2),
                Ordering::Relaxed,
            );
        }
        self.guard.check()
    }

    pub fn len(&self) -> usize {
        self.units.len()
    }
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    pub fn held_bytes(&self) -> usize {
        crate::heap::btree_bytes(
            self.units.len(),
            std::mem::size_of::<(FragmentKey, Placed)>(),
        ) + self
            .terms
            .as_ref()
            .map_or(0, |terms| terms.stats().buffered_bytes)
    }

    /// Keys omitted after resolution are also filtered out of the term merge.
    /// The packed bytes need not be rewritten or retained in memory.
    pub fn retain(&mut self, mut keep: impl FnMut(FragmentKey) -> bool) {
        self.units.retain(|key, _| keep(*key));
    }

    pub fn add(
        &mut self,
        key: FragmentKey,
        fragment: PackedFragment,
    ) -> Result<(), crate::spill::Error> {
        if self.units.contains_key(&key) {
            return Ok(());
        }
        let (dc, du, data) = fragment.data.parts();
        let (sc, su, slice) = fragment.slice.parts();
        self.term_count = self.term_count.saturating_add(fragment.terms.len() as u64);
        if let Some(space) = &self.space {
            let completed = self.units.len() as u64 + 1;
            let output = self
                .len
                .saturating_add((data.len() + slice.len()) as u64)
                .saturating_add(self.term_count.saturating_mul(25));
            // Early completion order is parallel and can start with one
            // unusually large file. Use the source estimate until there is
            // a representative batch, rather than extrapolating that outlier.
            let projected = if completed >= (space.files / 32).max(32) {
                output
                    .saturating_mul(space.files)
                    .div_ceil(completed)
                    .saturating_mul(3)
            } else {
                0
            };
            let written = self
                .len
                .saturating_add(self.terms.as_ref().expect("sorter").stats().disk_bytes);
            space.remaining.store(
                space.baseline.max(projected).saturating_sub(written),
                Ordering::Relaxed,
            );
        }
        self.guard.check()?;
        if self.data.is_none() {
            self.data = Some(BufWriter::new(crate::fs::create_new_private(
                &self.range_path(self.ranges.len()),
            )?));
        }
        for bytes in [data, slice] {
            for chunk in bytes.chunks(1 << 20) {
                self.guard.check()?;
                self.data.as_mut().expect("range writer").write_all(chunk)?;
                self.range_hash.update(chunk);
            }
        }
        self.units.insert(
            key,
            Placed {
                range: self.ranges.len(),
                offset: self.range_bytes,
                data: (
                    dc,
                    du,
                    u32::try_from(data.len()).map_err(|_| crate::spill::Error::Overflow)?,
                ),
                slice: (
                    sc,
                    su,
                    u32::try_from(slice.len()).map_err(|_| crate::spill::Error::Overflow)?,
                ),
            },
        );
        self.len += (data.len() + slice.len()) as u64;
        self.range_bytes += (data.len() + slice.len()) as u64;
        self.terms
            .as_mut()
            .expect("sorter")
            .set_source_range(self.ranges.len() as u64);
        for (kind, hash) in fragment.terms {
            let mut record = [0; 25];
            record[0] = u8::try_from(
                TermKind::ALL
                    .iter()
                    .position(|k| *k == kind)
                    .expect("known kind"),
            )
            .expect("few kinds");
            record[1..9].copy_from_slice(&hash.to_be_bytes());
            record[9..].copy_from_slice(key.digest().as_bytes());
            self.terms.as_mut().expect("sorter").push(&record, &[])?;
        }
        self.range_files += 1;
        if self.range_files == 64 {
            self.close_range()?;
        }
        Ok(())
    }

    fn range_path(&self, index: usize) -> std::path::PathBuf {
        self.workspace
            .path()
            .join(format!("fragments-{index:020}.bin"))
    }

    fn close_range(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.data.take() {
            self.guard.check()?;
            file.write_all(self.range_hash.finalize().as_bytes())?;
            file.flush()?;
            drop(file);
            // Only closed, checksummed ranges enter the completed range list.
            self.ranges.push(self.range_bytes);
            self.range_hash = blake3::Hasher::new();
            self.range_bytes = 0;
            self.range_files = 0;
        }
        Ok(())
    }

    fn verify_ranges(&self) -> io::Result<()> {
        let mut buffer = [0; 64 * 1024];
        for (index, len) in self.ranges.iter().enumerate() {
            let mut file = crate::fs::open_read(&self.range_path(index))?;
            if file.metadata()?.len() != len + 32 {
                return Err(io::Error::other("fragment range length mismatch"));
            }
            let mut left = *len;
            let mut hash = blake3::Hasher::new();
            while left != 0 {
                let count = left.min(buffer.len() as u64) as usize;
                file.read_exact(&mut buffer[..count])?;
                hash.update(&buffer[..count]);
                left -= count as u64;
            }
            let mut expected = [0; 32];
            file.read_exact(&mut expected)?;
            if hash.finalize().as_bytes() != &expected {
                return Err(io::Error::other("fragment range checksum mismatch"));
            }
        }
        Ok(())
    }

    pub fn write(mut self, writer: &mut SegmentWriter) -> Result<(), SegmentError> {
        self.write_inner(writer)
            .map_err(|e| SegmentError::new(self.workspace.path(), ErrorKind::Io(e)))
    }

    fn write_inner(&mut self, writer: &mut SegmentWriter) -> io::Result<()> {
        self.close_range()?;
        self.verify_ranges()?;
        let keys = self.units.keys().copied().collect::<Vec<_>>();
        let count = keys.len() as u64;
        let mut table = UnitTableBuilder::new(Digest128::LEN);
        for key in &keys {
            table.push(key.digest().as_bytes(), UnitLocation { offset: 0, len: 0 });
        }
        let (table, units) = table.finish();
        self.guard.check()?;
        writer
            .write_section(spec(UNITS, UNIT_KEY_SPACE), &table, units)
            .map_err(io::Error::other)?;
        for slices in [false, true] {
            writer
                .begin_section(if slices {
                    slice_spec(SLICE_DATA)
                } else {
                    spec(DATA, UNIT_KEY_SPACE)
                })
                .map_err(io::Error::other)?;
            let mut copy = PackedCopy::with_capacity(keys.len());
            for placed in self.units.values() {
                self.guard.check()?;
                let (codec, unpacked, len) = if slices { placed.slice } else { placed.data };
                let offset = placed.offset + if slices { u64::from(placed.data.2) } else { 0 };
                let mut source =
                    BufReader::new(crate::fs::open_read(&self.range_path(placed.range))?);
                source.seek(SeekFrom::Start(offset))?;
                let mut bytes = vec![0; len as usize];
                source.read_exact(&mut bytes)?;
                copy.push(writer, &StoredUnit::from_parts(codec, unpacked, bytes))
                    .map_err(io::Error::other)?;
            }
            writer.end_section(count).map_err(io::Error::other)?;
            writer
                .write_section(
                    if slices {
                        slice_spec(SLICE_INDEX)
                    } else {
                        spec(INDEX, UNIT_KEY_SPACE)
                    },
                    &copy.finish(),
                    count,
                )
                .map_err(io::Error::other)?;
        }
        self.write_terms(writer, &keys)
    }

    fn write_terms(&mut self, writer: &mut SegmentWriter, keys: &[FragmentKey]) -> io::Result<()> {
        let mut sorted = self
            .terms
            .take()
            .expect("sorter")
            .finish()
            .map_err(io::Error::other)?;
        let next = |sorted: &mut crate::spill::SortedRecords| -> io::Result<Option<[u8; 25]>> {
            sorted
                .next_record()
                .map_err(io::Error::other)?
                .map(|record| record.key.try_into().map_err(io::Error::other))
                .transpose()
        };
        let mut record = next(&mut sorted)?;
        let mut ordinals = Vec::new();
        for (index, kind) in TermKind::ALL.into_iter().enumerate() {
            let postings_path = self
                .workspace
                .path()
                .join(format!("postings-{index:020}.bin"));
            let mut postings_file = BufWriter::new(crate::fs::create_new_private(&postings_path)?);
            let mut postings_hash = blake3::Hasher::new();
            let mut postings = PostingsBuilder::new(layout());
            let mut lookup = LookupTableBuilder::new();
            let mut current = None;
            self.guard.check()?;
            writer
                .begin_section(spec(TERM_LOOKUP, kind.key_space()))
                .map_err(io::Error::other)?;
            while let Some(bytes) = record {
                if usize::from(bytes[0]) != index {
                    break;
                }
                let hash = u64::from_be_bytes(bytes[1..9].try_into().expect("hash"));
                let key = FragmentKey::from_digest(Digest128::from_bytes(
                    bytes[9..].try_into().expect("key"),
                ));
                record = next(&mut sorted)?;
                // Shared fragments remain in the checkout's key map, but not
                // this segment's unit table or any of its posting lists.
                let Ok(ordinal) = keys.binary_search(&key) else {
                    continue;
                };
                if current != Some(hash) {
                    if let Some(term) = current {
                        lookup.push(term, postings.add_list(&ordinals, &[]));
                        if lookup.held() >= 64 * 1024 {
                            self.guard.check()?;
                            writer.write(&lookup.drain()).map_err(io::Error::other)?;
                        }
                        if postings.held() >= 64 * 1024 {
                            self.guard.check()?;
                            let bytes = postings.drain();
                            postings_hash.update(&bytes);
                            postings_file.write_all(&bytes)?;
                        }
                        ordinals.clear();
                    }
                    current = Some(hash);
                }
                ordinals.push(u32::try_from(ordinal).map_err(io::Error::other)?);
            }
            if let Some(term) = current {
                self.guard.check()?;
                lookup.push(term, postings.add_list(&ordinals, &[]));
                ordinals.clear();
            }
            let (rest, entries) = postings.finish();
            postings_hash.update(&rest);
            postings_file.write_all(&rest)?;
            postings_file.write_all(postings_hash.finalize().as_bytes())?;
            postings_file.flush()?;
            drop(postings_file);
            let (rest, terms) = lookup.finish();
            writer.write(&rest).map_err(io::Error::other)?;
            writer.end_section(terms).map_err(io::Error::other)?;
            let mut source = BufReader::new(crate::fs::open_read(&postings_path)?);
            let mut left = source
                .get_ref()
                .metadata()?
                .len()
                .checked_sub(32)
                .ok_or_else(|| io::Error::other("truncated postings checksum"))?;
            let mut copied_hash = blake3::Hasher::new();
            writer
                .begin_section(spec(TERM_POSTINGS, kind.key_space()))
                .map_err(io::Error::other)?;
            let mut buffer = [0; 64 * 1024];
            loop {
                if left == 0 {
                    break;
                }
                let n = left.min(buffer.len() as u64) as usize;
                source.read_exact(&mut buffer[..n])?;
                copied_hash.update(&buffer[..n]);
                left -= n as u64;
                self.guard.check()?;
                writer.write(&buffer[..n]).map_err(io::Error::other)?;
            }
            let mut expected = [0; 32];
            source.read_exact(&mut expected)?;
            if copied_hash.finalize().as_bytes() != &expected {
                return Err(io::Error::other("postings checksum mismatch"));
            }
            writer.end_section(entries).map_err(io::Error::other)?;
            drop(source);
            std::fs::remove_file(postings_path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragment_ranges_close_before_listing_and_refuse_corruption() {
        let root = tempfile::tempdir().unwrap();
        let workspace = super::super::scratch::for_test(root.path());
        let path = workspace.path().to_path_buf();
        let mut spill =
            GraphSectionsSpill::new(workspace, Config::new(1 << 20), WriteGuard::new(|| Ok(())))
                .unwrap();
        for n in 0..65u32 {
            spill
                .add(
                    FragmentKey::from_digest(Digest128::of(&n.to_le_bytes())),
                    PackedFragment::new(b"fragment", b"slice", []),
                )
                .unwrap();
        }
        assert_eq!(spill.ranges.len(), 1);
        assert!(spill.data.is_some());
        spill.close_range().unwrap();
        assert_eq!(spill.ranges.len(), 2);
        assert!(spill.data.is_none());
        spill.verify_ranges().unwrap();
        let range = spill.range_path(0);
        let mut file = std::fs::OpenOptions::new().write(true).open(range).unwrap();
        file.write_all(b"X").unwrap();
        drop(file);
        assert!(
            spill
                .verify_ranges()
                .unwrap_err()
                .to_string()
                .contains("checksum")
        );
        drop(spill);
        assert!(path.exists(), "dropping the spill never deletes its folder");
    }
}
