use crate::calls::CallTuple;

type Span = (usize, usize);

struct ActiveSpans<'a> {
    spans: &'a [Span],
    pending: std::vec::IntoIter<usize>,
    active: Vec<usize>,
}

impl<'a> ActiveSpans<'a> {
    fn new(spans: &'a [Span]) -> Self {
        let mut order = (0..spans.len()).collect::<Vec<_>>();
        order.sort_unstable_by_key(|&index| spans[index].0);
        Self {
            spans,
            pending: order.into_iter(),
            active: Vec::new(),
        }
    }

    fn at(&mut self, start: usize) -> &[usize] {
        self.active
            .retain(|&index| at_most(start, self.spans[index].1));
        while let Some(&index) = self.pending.as_slice().first() {
            let (low, high) = self.spans[index];
            if !at_most(low, start) {
                break;
            }
            let _ = self.pending.next();
            if at_most(start, high) {
                self.active.push(index);
            }
        }
        &self.active
    }
}

fn at_most(a: usize, b: usize) -> bool {
    #[cfg(test)]
    super::macro_body_tests::SPAN_CHECKS.with(|checks| checks.set(checks.get() + 1));
    a <= b
}

pub(super) fn owners(calls: &[CallTuple], arms: &[Span], items: &[Span]) -> Vec<Option<usize>> {
    let mut ordered_calls = (0..calls.len()).collect::<Vec<_>>();
    ordered_calls.sort_unstable_by_key(|&index| calls[index].3);
    let mut owners = vec![None; calls.len()];
    let mut active_arms = ActiveSpans::new(arms);
    let mut active_items = ActiveSpans::new(items);
    // Sweep by start byte so disjoint ranges expire before later calls. Store
    // the result at the original index to preserve the extractor's call order.
    for index in ordered_calls {
        let (_, _, _, call_start, call_end) = calls[index];
        let arm = active_arms
            .at(call_start)
            .iter()
            .copied()
            .filter(|&arm| at_most(call_end, arms[arm].1))
            .min_by_key(|&arm| (arms[arm].1 - arms[arm].0, arm));
        let Some(arm) = arm else { continue };
        let (arm_start, arm_end) = arms[arm];
        let inside_item = active_items.at(call_start).iter().any(|&item| {
            let (start, end) = items[item];
            at_most(arm_start, start) && at_most(call_end, end) && at_most(end, arm_end)
        });
        if !inside_item {
            owners[index] = Some(arm);
        }
    }
    owners
}
