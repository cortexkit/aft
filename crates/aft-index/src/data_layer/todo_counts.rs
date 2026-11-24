use crate::parser::todo_markers::MARKERS;
use aft_store::content::TodoMarker;

/// Marker totals without any retained marker text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[doc(hidden)]
pub struct TodoCounts {
    #[doc(hidden)]
    pub total: usize,
    #[doc(hidden)]
    pub by_kind: [usize; 5],
}

impl TodoCounts {
    #[doc(hidden)]
    pub fn of(items: &[TodoMarker]) -> Self {
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
