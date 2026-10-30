//! Bounded scheduling diagnostics, kept separate from publication inputs.
use std::cell::RefCell;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Reason {
    pub events: u16,
    pub periodic: bool,
    pub head: bool,
    pub query: bool,
}

impl Reason {
    pub(crate) const PATHS: u16 = 1;
    pub(crate) const CONTENT: u16 = 2;
    pub(crate) const RESCAN: u16 = 4;
    pub(crate) const IGNORE: u16 = 8;
    pub(crate) const SEMANTIC: u16 = 16;
    pub(crate) const CONFIGURE: u16 = 32;
    pub(crate) const HEAD_METADATA: u16 = 64;

    pub(crate) fn merge(&mut self, other: Self) {
        self.events |= other.events;
        self.periodic |= other.periodic;
        self.head |= other.head;
        self.query |= other.query;
    }

    pub(crate) fn enter(self) -> Scope {
        Scope(CURRENT.with(|current| current.replace(self)))
    }

    pub(crate) fn current() -> Self {
        CURRENT.with(|current| *current.borrow())
    }

    pub(crate) fn fields(self, paths: &BTreeSet<Vec<u8>>) -> String {
        let mut examples = Vec::new();
        let mut clipped = paths.len() > 3;
        for path in paths {
            if examples.len() == 3 {
                break;
            }
            // Percent encoding prevents filenames from injecting log fields or lines.
            let mut example = String::new();
            for byte in path {
                let part = if byte.is_ascii_alphanumeric() || b"-._/".contains(byte) {
                    (*byte as char).to_string()
                } else {
                    format!("%{byte:02X}")
                };
                if example.len() + part.len() > 120 {
                    clipped = true;
                    break;
                }
                example.push_str(&part);
            }
            examples.push(example);
        }
        let kinds: Vec<_> = [
            (Self::PATHS, "paths"),
            (Self::CONTENT, "content_only_paths"),
            (Self::RESCAN, "rescan"),
            (Self::IGNORE, "ignore_rules"),
            (Self::SEMANTIC, "semantic_ready"),
            (Self::CONFIGURE, "configure"),
            (Self::HEAD_METADATA, "head_metadata"),
        ]
        .into_iter()
        .filter_map(|(bit, name)| (self.events & bit != 0).then_some(name))
        .collect();
        format!("schedule_changed_paths={} schedule_path_examples={} schedule_event_kinds={} schedule_periodic_verification={} schedule_head_change={} schedule_query_freshness={} schedule_full_read={} schedule_changed_paths_known={} schedule_examples_truncated={}", paths.len(),
            if examples.is_empty() { "none".into() } else { examples.join(",") },
            if kinds.is_empty() { "none".into() } else { kinds.join(",") },
            self.periodic, self.head, self.query, paths.is_empty(), !paths.is_empty(), clipped)
    }
}

thread_local! { static CURRENT: RefCell<Reason> = const { RefCell::new(Reason { events: 0, periodic: false, head: false, query: false }) }; }
pub(crate) struct Scope(Reason);
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scheduling_reason_is_bounded_and_merge_preserves_triggers() {
        let mut reason = Reason {
            events: Reason::CONTENT,
            query: true,
            ..Reason::default()
        };
        reason.merge(Reason {
            events: Reason::RESCAN,
            head: true,
            periodic: true,
            ..Reason::default()
        });
        let paths = (0..300)
            .map(|index| format!("{index:03}{}\n key=value", "é".repeat(150)).into_bytes())
            .collect();
        let fields = reason.fields(&paths);
        assert!(fields.contains("schedule_changed_paths=300 "));
        let examples = fields
            .split("schedule_path_examples=")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap();
        assert_eq!(examples.split(',').count(), 3);
        assert!(examples.split(',').all(|path| path.len() <= 120));
        assert!(!fields.contains('\n'));
        assert!(fields.contains("schedule_event_kinds=content_only_paths,rescan "));
        assert!(fields.contains("schedule_periodic_verification=true"));
        assert!(fields.contains("schedule_head_change=true"));
        assert!(fields.contains("schedule_query_freshness=true"));
    }
}
