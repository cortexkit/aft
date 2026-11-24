/// Marker totals without any retained marker text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TodoCounts {
    pub(crate) total: usize,
    pub(crate) by_kind: [usize; 5],
}

impl TodoCounts {
    pub(crate) fn of(items: &[TodoMarker]) -> Self {
        let mut counts = Self {
            total: items.len(),
            ..Self::default()
        };
        for item in items {
            if let Some(index) = MARKERS.iter().position(|marker| *marker == item.marker) {
                counts.by_kind[index] += 1;
            }
        }
        counts
    }
}
