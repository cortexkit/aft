//! Offline smallest-enclosing-range lookup. Sweeping starts and querying a
//! reversed-end Fenwick tree avoids scanning every syntax node for every ref.

test_support_visibility! {
pub(super) struct Preorder<'tree> {
    cursor: tree_sitter::TreeCursor<'tree>,
    started: bool,
    finished: bool,
}
}

test_support_visibility! {
pub(super) fn preorder(root: tree_sitter::Node<'_>) -> Preorder<'_> {
    Preorder {
        cursor: root.walk(),
        started: false,
        finished: false,
    }
}
}

impl<'tree> Iterator for Preorder<'tree> {
    type Item = tree_sitter::Node<'tree>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if !self.started {
            self.started = true;
        } else if !self.cursor.goto_first_child() {
            while !self.cursor.goto_next_sibling() {
                if !self.cursor.goto_parent() {
                    self.finished = true;
                    return None;
                }
            }
        }
        Some(self.cursor.node())
    }
}

test_support_visibility! {
#[derive(Clone, Copy)]
pub(super) enum LookupWork {
    Ordinal,
    Dispatch,
}
}

impl LookupWork {
    fn visit(self) {
        #[cfg(any(test, feature = "test-support"))]
        super::extraction_work::note(|work| match self {
            Self::Ordinal => work.ordinal_visits += 1,
            Self::Dispatch => work.dispatch_visits += 1,
        });
    }
}

test_support_visibility! {
/// Return the index of the shortest enclosing range, breaking ties by input
/// order. This also handles empty ranges, shared spans and error-recovery nodes;
/// using a parent walk alone would miss adjacent nodes at an empty-range boundary.
/// After sorting node and query starts, each update and query costs O(log nodes).
pub(super) fn range_minima(
    nodes: &[(usize, usize)],
    queries: &[(usize, usize)],
    work: LookupWork,
) -> Vec<Option<usize>> {
    let mut by_start = (0..nodes.len()).collect::<Vec<_>>();
    by_start.sort_unstable_by_key(|&i| (nodes[i].0, i));
    let mut ends = nodes.iter().map(|n| n.1).collect::<Vec<_>>();
    ends.sort_unstable();
    ends.dedup();
    let mut query_order = (0..queries.len()).collect::<Vec<_>>();
    query_order.sort_unstable_by_key(|&i| (queries[i].0, i));
    let mut minima = vec![(usize::MAX, usize::MAX); ends.len() + 1];
    let mut results = vec![None; queries.len()];
    let mut next = 0;
    for query in query_order {
        let (start, end) = queries[query];
        while next < by_start.len() && nodes[by_start[next]].0 <= start {
            work.visit();
            let index = by_start[next];
            let (node_start, node_end) = nodes[index];
            let key = (node_end.saturating_sub(node_start), index);
            let mut bucket = ends.len()
                - ends.partition_point(|&candidate| {
                    work.visit();
                    candidate < node_end
                });
            while bucket < minima.len() {
                work.visit();
                minima[bucket] = minima[bucket].min(key);
                bucket += bucket & bucket.wrapping_neg();
            }
            next += 1;
        }
        let mut bucket = ends.len()
            - ends.partition_point(|&candidate| {
                work.visit();
                candidate < end
            });
        let mut best = (usize::MAX, usize::MAX);
        while bucket > 0 {
            work.visit();
            best = best.min(minima[bucket]);
            bucket &= bucket - 1;
        }
        if best.1 != usize::MAX {
            results[query] = Some(best.1);
        }
    }
    results
}

}
