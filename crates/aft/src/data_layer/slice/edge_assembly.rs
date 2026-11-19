use std::cmp::Ordering;

use super::{NodeId, TargetLookup};

/// An edge as assembly collects it, before the slice is packed.
pub(super) struct EdgeRow<'r> {
    pub(super) ref_ordinal: u32,
    pub(super) source: NodeId,
    pub(super) target_file: &'r str,
    pub(super) target_symbol: &'r str,
    pub(super) kind: u64,
    pub(super) line: u32,
    pub(super) provenance: u64,
    pub(super) lookup: TargetLookup,
}

/// The legacy edge id without its file path, which is the same for every
/// key in a slice. Equality also follows the spelled id so an ambiguous
/// `file:symbol` split still replaces the earlier row.
#[derive(Clone, Copy, Debug)]
pub(super) enum EdgeKey<'a> {
    Edge {
        ordinal: u32,
    },
    Dispatch {
        ordinal: u32,
        file: &'a str,
        symbol: &'a str,
    },
}

fn decimal(mut ordinal: u32, digits: &mut [u8; 10]) -> &[u8] {
    let mut start = digits.len();
    loop {
        start -= 1;
        digits[start] = b'0' + (ordinal % 10) as u8;
        ordinal /= 10;
        if ordinal == 0 {
            return &digits[start..];
        }
    }
}

fn cmp_ordinals(left: u32, right: u32, dispatch: bool) -> Ordering {
    if left == right {
        return Ordering::Equal;
    }
    let (mut left_digits, mut right_digits) = ([0; 10], [0; 10]);
    let left = decimal(left, &mut left_digits);
    let right = decimal(right, &mut right_digits);
    let len = left.len().min(right.len());
    left[..len].cmp(&right[..len]).then_with(|| {
        // An ordinary id ends at the ordinal, so `1` sorts before `12`.
        // In a dispatch id, `:` sorts after every digit: `12:` before `1:`.
        if dispatch {
            right.len().cmp(&left.len())
        } else {
            left.len().cmp(&right.len())
        }
    })
}

fn cmp_targets(
    left_file: &str,
    left_symbol: &str,
    right_file: &str,
    right_symbol: &str,
) -> Ordering {
    let (left, right) = (left_file.as_bytes(), right_file.as_bytes());
    let len = left.len().min(right.len());
    left[..len].cmp(&right[..len]).then_with(|| {
        if left.len() == right.len() {
            return left_symbol.cmp(right_symbol);
        }
        // The longer file can extend across the other id's separator and
        // into its symbol. Compare the rest across those boundaries.
        left[len..]
            .iter()
            .copied()
            .chain([b':'])
            .chain(left_symbol.bytes())
            .cmp(
                right[len..]
                    .iter()
                    .copied()
                    .chain([b':'])
                    .chain(right_symbol.bytes()),
            )
    })
}

impl Ord for EdgeKey<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        use EdgeKey::{Dispatch, Edge};
        match (*self, *other) {
            (Edge { ordinal: left }, Edge { ordinal: right }) => cmp_ordinals(left, right, false),
            (
                Dispatch {
                    ordinal: left,
                    file: left_file,
                    symbol: left_symbol,
                },
                Dispatch {
                    ordinal: right,
                    file: right_file,
                    symbol: right_symbol,
                },
            ) => cmp_ordinals(left, right, true)
                .then_with(|| cmp_targets(left_file, left_symbol, right_file, right_symbol)),
            (Dispatch { .. }, Edge { .. }) => Ordering::Less,
            (Edge { .. }, Dispatch { .. }) => Ordering::Greater,
        }
    }
}

impl PartialOrd for EdgeKey<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for EdgeKey<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for EdgeKey<'_> {}

#[cfg(test)]
mod tests {
    use super::EdgeKey::{self, Dispatch, Edge};

    fn legacy_id(key: EdgeKey<'_>, path: &str) -> String {
        match key {
            Edge { ordinal } => format!("edge:view:{path}:{ordinal}"),
            Dispatch {
                ordinal,
                file,
                symbol,
            } => {
                format!("dispatch:{path}:{ordinal}:{file}:{symbol}")
            }
        }
    }

    #[test]
    fn edge_key_order_matches_legacy_ids() {
        let mut keys = Vec::new();
        for ordinal in [0, 1, 9, 10, 12, 99, 100, u32::MAX] {
            keys.push(Edge { ordinal });
            for (file, symbol) in [
                ("", ""),
                ("x", "a::b"),
                ("x:a:", "b"), // Same id as ("x", "a::b").
                ("x", "a:é"),
                ("x::", "名"),
                ("x/y", "é"),
                ("xλ", "a"),
            ] {
                keys.push(Dispatch {
                    ordinal,
                    file,
                    symbol,
                });
            }
        }

        // A fixed seed adds longer ordinals and more mixes of UTF-8 and
        // delimiter bytes while keeping any failure reproducible.
        let mut state = 0xd401_6d23u32;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state
        };
        let parts = ["", "a", ":", "::", "é", "名", "🦀", "file/"];
        let mut text = || {
            (0..3)
                .map(|_| parts[(next() >> 29) as usize])
                .collect::<String>()
        };
        let paths = ["src/f.rs".to_owned(), "src/é::名/f.rs".to_owned(), text()];
        let names = (0..96).map(|_| (text(), text())).collect::<Vec<_>>();
        for (file, symbol) in &names {
            let ordinal = next();
            keys.push(Edge { ordinal });
            keys.push(Dispatch {
                ordinal,
                file,
                symbol,
            });
            keys.push(Dispatch {
                ordinal: 1,
                file,
                symbol,
            });
        }

        for path in &paths {
            let ids = keys
                .iter()
                .map(|&key| legacy_id(key, path))
                .collect::<Vec<_>>();
            for (left, left_id) in keys.iter().zip(&ids) {
                for (right, right_id) in keys.iter().zip(&ids) {
                    assert_eq!(
                        left.cmp(right),
                        left_id.cmp(right_id),
                        "{left_id:?} vs {right_id:?}"
                    );
                    assert_eq!(left == right, left_id == right_id, "{left:?} vs {right:?}");
                }
            }
        }
    }
}
