use fancy_regex::Regex;
use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::sync::OnceLock;

// Generated from ai-tokenizer Claude encoding; vendored in-tree so
// the crate builds from a clean clone without fetching `tmp/ai-tokenizer-pkg/`.
// To refresh: cargo run -p aft-tokenizer --example regenerate_claude.
include!("claude_data.rs");

const PAT_STR: &str = r"'s|'t|'re|'ve|'m|'ll|'d| ?\p{L}+| ?\p{N}+| ?[^\s\p{L}\p{N}]+|\s+(?!\S)|\s+";
const NO_RANK: u32 = u32::MAX;

static PATTERN_REGEX: OnceLock<Regex> = OnceLock::new();
static STRING_ENCODER: OnceLock<HashMap<&'static str, u32>> = OnceLock::new();
static BINARY_ENCODER: OnceLock<HashMap<&'static [u8], u32>> = OnceLock::new();

/// Count Claude tokens in `text`.
///
/// # Errors
///
/// Returns [`crate::TokenizationError`] when pre-tokenization fails, including
/// regex backtracking or stack limits. Never returns an estimate or partial count.
pub fn count_tokens(text: &str) -> Result<usize, crate::TokenizationError> {
    if text.is_empty() {
        return Ok(0);
    }
    // Keep the same direct-token shortcut and pre-tokenizer as encoding, but
    // accumulate counts instead of allocating a final token-ID vector.
    let mut count = 0;
    emit_tokens(text, pattern_regex(), |_| count += 1)?;
    Ok(count)
}

/// Encode `text` into Claude token IDs.
///
/// # Errors
///
/// Returns [`crate::TokenizationError`] when pre-tokenization fails, including
/// regex backtracking or stack limits. Never returns a partial vector or invented IDs.
pub fn encode(text: &str) -> Result<Vec<u32>, crate::TokenizationError> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    encode_internal(text)
}

fn encode_internal(text: &str) -> Result<Vec<u32>, crate::TokenizationError> {
    let mut result = Vec::new();
    emit_tokens(text, pattern_regex(), |rank| result.push(rank))?;
    #[cfg(test)]
    TOKEN_VECTOR_BYTES.with(|bytes| bytes.set(bytes.get() + result.capacity() * 4));
    Ok(result)
}

fn emit_tokens(
    text: &str,
    regex: &Regex,
    mut emit: impl FnMut(u32),
) -> Result<(), crate::TokenizationError> {
    // ai-tokenizer checks direct rank before pre-tokenizing for short strings.
    if text.encode_utf16().take(10).count() < 10 {
        if let Some(&rank) = string_encoder().get(text) {
            emit(rank);
            return Ok(());
        }
    }

    for piece in regex.find_iter(text) {
        let piece = piece?.as_str();

        if let Some(&rank) = string_encoder().get(piece) {
            emit(rank);
            continue;
        }

        visit_byte_pair_tokens(piece.as_bytes(), &mut emit);
    }
    Ok(())
}

fn pattern_regex() -> &'static Regex {
    PATTERN_REGEX.get_or_init(|| Regex::new(PAT_STR).expect("Claude pre-tokenizer regex"))
}

fn string_encoder() -> &'static HashMap<&'static str, u32> {
    STRING_ENCODER.get_or_init(|| STRING_ENCODER_ENTRIES.iter().copied().collect())
}

fn binary_encoder() -> &'static HashMap<&'static [u8], u32> {
    BINARY_ENCODER.get_or_init(|| BINARY_ENCODER_ENTRIES.iter().copied().collect())
}

fn visit_byte_pair_tokens(piece: &[u8], mut emit: impl FnMut(u32)) {
    #[cfg(test)]
    if LEGACY_BPE.with(|legacy| legacy.get()) {
        for rank in byte_pair_merge_legacy(piece) {
            emit(rank);
        }
        return;
    }
    let len = piece.len();
    // Byte offsets are stable node IDs. Removing a boundary only relinks its
    // neighbours; it never shifts the remaining offsets or pending candidates.
    let mut next: Vec<usize> = (1..=len + 1).collect();
    let mut previous: Vec<usize> = (0..=len).map(|i| i.saturating_sub(1)).collect();
    let mut heap = BinaryHeap::new();
    for left in 0..len.saturating_sub(1) {
        enqueue_pair(piece, &next, left, &mut heap);
    }
    while let Some(Reverse(candidate)) = heap.pop() {
        let MergeCandidate(_, left, right, end) = candidate;
        // A neighbouring merge invalidates the old pair, including its rank.
        if next[left] != right || next[right] != end {
            continue;
        }
        next[left] = end;
        next[right] = usize::MAX;
        previous[end] = left;
        enqueue_pair(piece, &next, left, &mut heap);
        if left > 0 {
            enqueue_pair(piece, &next, previous[left], &mut heap);
        }
    }
    let mut left = 0;
    while left < len {
        let right = next[left];
        let rank = get_rank_for_slice(&piece[left..right]);
        // Every byte has a vocabulary rank; merges only combine ranked slices.
        debug_assert_ne!(rank, NO_RANK, "unranked final BPE piece");
        if rank != NO_RANK {
            emit(rank);
        }
        left = right;
    }
}

// Ordering by the original left offset preserves the legacy scan's leftmost
// tie-break, even after earlier boundaries have been removed.
#[derive(Eq, PartialEq)]
struct MergeCandidate(u32, usize, usize, usize);

impl Ord for MergeCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        #[cfg(test)]
        BPE_WORK.with(|work| work.set(work.get() + 1));
        (self.0, self.1, self.2, self.3).cmp(&(other.0, other.1, other.2, other.3))
    }
}

impl PartialOrd for MergeCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn enqueue_pair(
    piece: &[u8],
    next: &[usize],
    left: usize,
    heap: &mut BinaryHeap<Reverse<MergeCandidate>>,
) {
    let right = next[left];
    if right >= piece.len() {
        return;
    }
    let end = next[right];
    let rank = get_rank_for_slice(&piece[left..end]);
    if rank != NO_RANK {
        heap.push(Reverse(MergeCandidate(rank, left, right, end)));
    }
}

#[cfg(test)]
thread_local! {
    static BPE_WORK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LEGACY_BPE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TOKEN_VECTOR_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn byte_pair_merge_legacy(piece: &[u8]) -> Vec<u32> {
    let len = piece.len();
    let mut starts = Vec::with_capacity(len + 1);
    let mut ranks = Vec::with_capacity(len + 1);

    for i in 0..=len {
        starts.push(i);
        if i < len.saturating_sub(1) {
            ranks.push(get_rank_for_slice(&piece[i..i + 2]));
        } else {
            ranks.push(NO_RANK);
        }
    }

    while starts.len() > 1 {
        let mut min_rank = NO_RANK;
        let mut min_idx = None;
        for (idx, &rank) in ranks.iter().take(ranks.len().saturating_sub(1)).enumerate() {
            BPE_WORK.with(|work| work.set(work.get() + 1));
            if rank < min_rank {
                min_rank = rank;
                min_idx = Some(idx);
            }
        }

        let Some(min_idx) = min_idx else { break };
        if min_rank == NO_RANK {
            break;
        }

        BPE_WORK.with(|work| {
            work.set(work.get() + starts.len() - min_idx - 2 + ranks.len() - min_idx - 1)
        });
        starts.remove(min_idx + 1);
        ranks.remove(min_idx);
        if min_idx < ranks.len() {
            ranks[min_idx] = get_rank(piece, &starts, min_idx);
        }
        if min_idx > 0 {
            ranks[min_idx - 1] = get_rank(piece, &starts, min_idx - 1);
        }
    }

    let mut output = Vec::with_capacity(starts.len().saturating_sub(1));
    for window in starts.windows(2) {
        let rank = get_rank_for_slice(&piece[window[0]..window[1]]);
        if rank != NO_RANK {
            output.push(rank);
        }
    }
    output
}

#[cfg(test)]
fn get_rank(piece: &[u8], starts: &[usize], start_index: usize) -> u32 {
    let Some(&pair_start) = starts.get(start_index) else {
        return NO_RANK;
    };
    let Some(&pair_end) = starts.get(start_index + 2) else {
        return NO_RANK;
    };
    get_rank_for_slice(&piece[pair_start..pair_end])
}

fn get_rank_for_slice(slice: &[u8]) -> u32 {
    if let Ok(as_string) = std::str::from_utf8(slice) {
        if let Some(&rank) = string_encoder().get(as_string) {
            return rank;
        }
    }

    binary_encoder().get(slice).copied().unwrap_or(NO_RANK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_apis_report_regex_failure_without_panicking() {
        let text = format!("prefix{}suffix", " ".repeat(1_000_001));
        assert!(pattern_regex().find_iter(&text).any(|piece| piece.is_err()));
        let count_error = std::panic::catch_unwind(|| count_tokens(&text))
            .expect("counting must not panic")
            .expect_err("counting must report the regex failure");
        let encode_error = std::panic::catch_unwind(|| encode(&text))
            .expect("encoding must not panic")
            .expect_err("encoding must report the regex failure");
        assert!(matches!(
            count_error,
            crate::TokenizationError::RuntimeError(_)
        ));
        assert!(matches!(
            encode_error,
            crate::TokenizationError::RuntimeError(_)
        ));
    }

    #[test]
    fn emission_reports_genuine_low_budget_regex_failure() {
        let regex = fancy_regex::RegexBuilder::new(PAT_STR)
            .backtrack_limit(1)
            .build()
            .unwrap();
        let text = format!("prefix{}suffix", " ".repeat(1024));
        assert!(regex.find_iter(&text).any(|piece| piece.is_err()));
        let result = std::panic::catch_unwind(|| emit_tokens(&text, &regex, |_| {}));
        let error = result
            .expect("emission must not panic")
            .expect_err("regex error must propagate");
        assert!(matches!(error, crate::TokenizationError::RuntimeError(_)));
    }

    #[test]
    fn binary_encoder_borrows_static_keys() {
        let (bytes, rank) = BINARY_ENCODER_ENTRIES[0];
        let stored = binary_encoder()
            .keys()
            .find(|key| &key[..] == bytes)
            .unwrap();
        assert_eq!(stored.as_ptr(), bytes.as_ptr());
        assert_eq!(binary_encoder().get(bytes), Some(&rank));
    }

    #[test]
    fn vocabulary_covers_every_single_byte() {
        for byte in 0..=u8::MAX {
            assert_ne!(get_rank_for_slice(&[byte]), NO_RANK, "byte {byte}");
        }
    }

    #[test]
    fn a02_count_only_does_not_materialize_token_vectors() {
        let lossy = String::from_utf8_lossy(&[0xff, b'a', 0x80, b'\n']).into_owned();
        let corpus = [
            String::new(),
            "!".into(),
            " café 中文 🦀 e\u{301} 's\n\t \r\n".into(),
            " ".repeat(8192),
            "=".repeat(8192),
            "ordinary log output\n".repeat(100),
            lossy,
        ];
        for text in corpus {
            let reference = encode(&text).unwrap().len();
            TOKEN_VECTOR_BYTES.with(|bytes| bytes.set(0));
            assert_eq!(
                count_tokens(&text).unwrap(),
                reference,
                "exact encoding length for {text:?}"
            );
            assert_eq!(
                TOKEN_VECTOR_BYTES.with(|bytes| bytes.get()),
                0,
                "count-only path allocated token vectors for {} input bytes",
                text.len()
            );
        }
    }

    #[test]
    #[ignore = "release-mode count latency measurement"]
    fn a02_count_only_measurement() {
        for bytes in [1024, 256 * 1024] {
            let mut text = "ordinary output café 中文\n".repeat(bytes / 29 + 1);
            let mut end = bytes.min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            let expected = encode(&text).unwrap().len();
            let mut elapsed = Vec::new();
            let mut encoded_elapsed = Vec::new();
            let mut count_bytes = 0;
            let mut encode_bytes = 0;
            for _ in 0..16 {
                TOKEN_VECTOR_BYTES.with(|count| count.set(0));
                let start = std::time::Instant::now();
                assert_eq!(count_tokens(std::hint::black_box(&text)).unwrap(), expected);
                elapsed.push(start.elapsed().as_secs_f64() * 1000.0);
                count_bytes += TOKEN_VECTOR_BYTES.with(|count| count.get());
                TOKEN_VECTOR_BYTES.with(|count| count.set(0));
                let start = std::time::Instant::now();
                assert_eq!(encode(std::hint::black_box(&text)).unwrap().len(), expected);
                encoded_elapsed.push(start.elapsed().as_secs_f64() * 1000.0);
                encode_bytes += TOKEN_VECTOR_BYTES.with(|count| count.get());
            }
            elapsed.sort_by(f64::total_cmp);
            encoded_elapsed.sort_by(f64::total_cmp);
            println!("A02_TOKEN_COUNT input_bytes={} calls=16 token_vector_capacity_bytes={} p50_ms={:.3} p95_ms={:.3}",
                text.len(), count_bytes, (elapsed[7] + elapsed[8]) / 2.0, elapsed[15]);
            println!("A02_TOKEN_ENCODING_REFERENCE input_bytes={} calls=16 token_vector_capacity_bytes={} p50_ms={:.3} p95_ms={:.3}",
                text.len(), encode_bytes, (encoded_elapsed[7] + encoded_elapsed[8]) / 2.0, encoded_elapsed[15]);
        }
    }

    #[test]
    fn empty_input_has_no_tokens() {
        assert!(encode("").unwrap().is_empty());
        assert_eq!(count_tokens("").unwrap(), 0);
    }

    #[test]
    fn direct_ascii_token() {
        assert_eq!(encode("!").unwrap(), vec![5]);
        assert_eq!(count_tokens("!").unwrap(), 1);
    }

    #[test]
    fn bpe_heap_work_is_bounded_and_tokens_are_identical() {
        let mut corpus = vec![
            "=".repeat(8192),
            "\n".repeat(8192),
            " \n\t".repeat(2048),
            "Hello, world! café 中文 🦀\nexport const foo = (x: number) => x + 1;\n".repeat(20),
        ];
        // Exercise tied ranks, binary byte tokens and changes on both sides of
        // a merge without deriving expectations from the new implementation.
        for n in 0..128 {
            corpus.push(
                (0..n)
                    .map(|i| ['=', 'a', '\n', 'é', '🦀', ' '][(i * 17 + n) % 6])
                    .collect(),
            );
        }
        for (index, text) in corpus.iter().enumerate() {
            LEGACY_BPE.with(|legacy| legacy.set(true));
            BPE_WORK.with(|work| work.set(0));
            let reference = encode(text).unwrap();
            let before = BPE_WORK.with(|work| work.get());
            LEGACY_BPE.with(|legacy| legacy.set(false));
            BPE_WORK.with(|work| work.set(0));
            let actual = encode(text).unwrap();
            let after = BPE_WORK.with(|work| work.get());
            assert_eq!(actual, reference, "corpus item {index}");
            assert_eq!(count_tokens(text).unwrap(), reference.len());
            if index < 4 {
                eprintln!(
                    "BPE corpus {index}: comparisons + shifts {before} -> {after}, {} tokens",
                    actual.len()
                );
            }
            assert!(
                after <= 64 * text.len(),
                "{after} operations for {} bytes",
                text.len()
            );
        }
        for text in ["=".repeat(128 * 1024), "\n".repeat(128 * 1024)] {
            assert!(!encode(&text).unwrap().is_empty());
        }
    }
}
