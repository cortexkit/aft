//! Limits the parses and freshly bound surfaces that coexist before cache admission.

use super::{resolve_files_with, BindCache, CheckoutInputs, ParseSlot, SiteFile};
use crate::callgraph_store::join::ManifestJoinError;
use aft_store::heap::{HeapBytes, Seen};
use rayon::prelude::*;
use std::sync::Arc;

fn estimate(slot: &ParseSlot) -> usize {
    // The unpacked record is a proxy for the decoded parse: the decoder adds
    // in-memory structs, vector capacity and separate string allocations.
    let decoded = if let Some((_, parse)) = slot.value.get() {
        parse.heap_bytes(&mut Seen::new())
    } else if let Some(staged) = &slot.staged {
        staged.at.uncompressed_bytes().saturating_mul(2)
    } else if let Some((_, record)) = &slot.encoded {
        record
            .surface
            .len()
            .saturating_add(record.body.len())
            .saturating_mul(2)
    } else {
        // A stored parse has no size here. Admit it alone rather than decoding
        // it just to decide where the batch ends.
        return usize::MAX;
    };
    // Near the Linux kernel project's cold-build peak, fresh bound surfaces and
    // their indexes took about 2,624 MiB to the decoded parses' 656 MiB. Charge
    // both owners even if a file might already be bound when the batch runs.
    decoded.max(1).saturating_mul(5)
}

fn batch_len<T>(remaining: &[T], bytes: usize, estimate: impl Fn(&T) -> usize) -> usize {
    let Some((first, rest)) = remaining.split_first() else {
        return 0;
    };
    // An oversized file must make progress, but must not bring a second file.
    let mut available = bytes.saturating_sub(estimate(first));
    let mut len = 1;
    for file in rest {
        if available == 0 {
            break;
        }
        let Some(left) = available.checked_sub(estimate(file)) else {
            break;
        };
        available = left;
        len += 1;
    }
    len
}

/// Resolves one fixed spool range in smaller binding batches. The range
/// accumulates encoded resolutions for the spool and inputs for method-call
/// resolution; it doesn't keep the working parses decoded between batches.
pub(super) fn resolve(
    inputs: &mut CheckoutInputs,
    bound: &BindCache,
    mut remaining: &[String],
    budget: usize,
    sites: &mut Vec<(String, Arc<SiteFile>)>,
) -> Result<Vec<Vec<u8>>, ManifestJoinError> {
    let mut encoded = Vec::with_capacity(remaining.len());
    while !remaining.is_empty() {
        let parses = &inputs.parses;
        let len = batch_len(remaining, budget, |path| estimate(&parses.map[path]));
        let (batch, rest) = remaining.split_at(len);
        remaining = rest;
        let paths = batch.iter().collect::<Vec<_>>();
        let carry = crate::alloc_trace::carry();
        batch.par_iter().for_each(|path| {
            let _label = carry.enter();
            let _ = parses.get(path);
        });
        bound.bind_missing(inputs, &paths)?;
        let resolved = resolve_files_with(inputs, bound, &paths, &|path, resolution| {
            let (_, parse) = &parses[path];
            let site = Arc::new(SiteFile::new(parse, &resolution));
            (site, super::super::fragment::encode(&resolution))
        })?;
        for (path, (site, bytes)) in resolved {
            sites.push((path, site));
            encoded.push(bytes);
        }
        // Only the batch's own parses are cached; dependency reads borrow or
        // decode without caching. Don't walk the whole checkout per batch.
        let (first, last) = (&batch[0], &batch[len - 1]);
        for (_, slot) in inputs.parses.map.range_mut(first.clone()..=last.clone()) {
            if let Some(slot) = Arc::get_mut(slot) {
                if slot.redecodable() {
                    slot.value.take();
                }
            }
        }
    }
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use super::batch_len;

    #[test]
    fn batches_stop_at_the_byte_budget_not_a_file_count() {
        let files = [4, 6, 3, 8, 1, 1];
        assert_eq!(batch_len(&files, 10, |n| *n), 2);
        assert_eq!(batch_len(&files[2..], 10, |n| *n), 1);
        assert_eq!(batch_len(&files[3..], 10, |n| *n), 3);
    }

    #[test]
    fn an_oversized_file_always_advances_by_one_without_overflow() {
        assert_eq!(batch_len(&[usize::MAX, 2], 15, |n| *n), 1);
        assert_eq!(batch_len(&[1, usize::MAX], usize::MAX, |n| *n), 1);
        assert_eq!(batch_len(&[5, 5], 0, |n| *n), 1);
        assert_eq!(batch_len::<usize>(&[], 0, |n| *n), 0);
    }
}
