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

/// One file's TODO markers as a checkout holds them, for the TODO count.
#[derive(Debug)]
#[doc(hidden)]
pub enum PathTodos {
    /// The path is not one of the view's files.
    Absent,
    /// Larger than the count reads.
    TooLarge,
    /// The file has no record: the store neither parses it nor counts its
    /// comments (it is binary, or a C header that is not UTF-8).
    Unrecorded,
    /// Its record, with the TODO markers it holds, if any.
    Recorded(Option<aft_store::content::TodoMarkers>),
    /// The store could not extract the file, so it has no record.
    Unextracted,
    /// Its record is not there to read: the view's state names a record a
    /// build of this extractor version has not written yet, or the file
    /// changed again since the view was taken.
    Missing,
}

/// The record's language and counts; unlike `TodoMarkers`, owns no marker text.
#[derive(Clone, Debug)]
#[doc(hidden)]
pub struct TodoSummary {
    #[doc(hidden)]
    pub language: Option<crate::parser::todo_markers::TodoLanguage>,
    #[doc(hidden)]
    pub counts: Option<super::todo_counts::TodoCounts>,
}

#[derive(Clone, Debug)]
#[doc(hidden)]
pub enum PathTodoCounts {
    Recorded(Option<TodoSummary>),
    Absent,
    TooLarge,
    Unrecorded,
    Unextracted,
    Missing,
}

impl From<PathTodos> for PathTodoCounts {
    fn from(todos: PathTodos) -> Self {
        use super::todo_counts::TodoCounts;
        use crate::parser::todo_markers::TodoLanguage;
        match todos {
            PathTodos::Absent => Self::Absent,
            PathTodos::TooLarge => Self::TooLarge,
            PathTodos::Unrecorded => Self::Unrecorded,
            PathTodos::Unextracted => Self::Unextracted,
            PathTodos::Missing => Self::Missing,
            PathTodos::Recorded(todos) => Self::Recorded(todos.map(|todos| {
                TodoSummary {
                    language: TodoLanguage::from_name(&todos.language)
                        .filter(|language| language.name() == todos.language),
                    counts: todos.items.as_deref().map(TodoCounts::of),
                }
            })),
        }
    }
}
