//! Applying a filesystem scan to the checkout that it observed.

use super::*;

impl Checkout {
    /// Makes `state` the checkout's current state, recording each path in
    /// `changed` (compared with `base`) as a change, with the files the scan
    /// read in `read`.
    /// Returns `false`, applying nothing, when a publication served since
    /// `generation` replaced the one `base` came from.
    pub(super) fn apply_scan(
        &self,
        base: &CheckoutState,
        generation: u64,
        mut state: CheckoutState,
        changed: &[Vec<u8>],
        updated: Option<&[Vec<u8>]>,
        mut read: HashMap<Vec<u8>, Vec<u8>>,
    ) -> bool {
        // A record the scan read again keeps its language when its content
        // did not change; a changed text file gets the language its path and
        // bytes choose. Only changed paths get a language anew: a file read
        // again only because its stat moved (a touch, a copied checkout) has
        // the content it had.
        // Choosing a language reads the path, and only for a C header parses
        // the bytes; the changed files' extraction waits for the call graph
        // (see `ChangeContent`).
        if changed.is_empty()
            && updated.is_some_and(<[_]>::is_empty)
            && state.skipped_directories() == base.skipped_directories()
        {
            return self
                .live
                .read()
                .expect("no panics while holding it")
                .generation
                == generation;
        }
        if let Some(updated) = updated {
            // A scoped scan already kept all untouched records. Restoring
            // languages by point lookup avoids flattening its path overlay.
            let reread = updated.iter().filter_map(|path| {
                let record = state.get(path)?;
                let language = if changed.binary_search(path).is_ok() {
                    read.get(path).map(|bytes| changed_language(record, bytes))
                } else {
                    None
                };
                language
                    .or_else(|| {
                        let before = base.get(path)?;
                        (record.language.is_empty()
                            && before.content == record.content
                            && !before.language.is_empty())
                        .then(|| before.language.clone())
                    })
                    .map(|language| (path.as_slice(), language))
            });
            let retry = changed
                .iter()
                .filter(|path| updated.binary_search(path).is_err())
                .filter_map(|path| {
                    let record = state.get(path)?;
                    let bytes = read.get(path)?;
                    Some((path.as_slice(), changed_language(record, bytes)))
                });
            let languages = reread.chain(retry).collect::<Vec<_>>();
            state
                .set_languages_at(languages)
                .expect("a language a state stores");
        } else {
            let mut languages = Vec::new();
            for path in changed {
                if let (Some(bytes), Some(index)) = (read.get(path), state.position(path)) {
                    languages.push((index, changed_language(&state.paths()[index], bytes)));
                }
            }
            let mut extracted_at = languages
                .iter()
                .map(|(index, _)| *index)
                .collect::<Vec<_>>();
            extracted_at.sort_unstable();
            let mut before = base.paths().iter().peekable();
            for (index, record) in state.paths().iter().enumerate() {
                if !record.language.is_empty() || extracted_at.binary_search(&index).is_ok() {
                    continue;
                }
                while before.next_if(|before| before.path < record.path).is_some() {}
                if let Some(before) = before.peek().filter(|before| {
                    before.path == record.path
                        && before.content == record.content
                        && !before.language.is_empty()
                }) {
                    languages.push((index, before.language.clone()));
                }
            }
            for (index, language) in languages {
                state
                    .set_language(index, language)
                    .expect("a language a state stores");
            }
        }
        let gaps = base
            .skipped_parses()
            .iter()
            .filter(|(path, _)| {
                changed.binary_search(path).is_err()
                    && state.get(path).is_some_and(|now| {
                        base.get(path).is_some_and(|before| {
                            same_member(before, now) && before.language == now.language
                        })
                    })
            })
            .map(|(path, reason)| (path.clone(), reason.clone()))
            .collect();
        state = state
            .with_skipped_parses(gaps)
            .expect("unchanged gap members");
        if changed.is_empty() {
            let mut live = self.write_live();
            if live.generation != generation {
                return false;
            }
            live.state = Some(Arc::new(state));
            self.touch_memory();
            return true;
        }
        let mut entries = Vec::with_capacity(changed.len());
        for path in changed {
            let record = state.get(path).cloned();
            // Text queries read the new trigrams at once; what the call graph
            // reads is extracted from the same bytes when it first needs it.
            let bytes = read.remove(path);
            let entry = match (&record, &bytes) {
                (None, _) => OverlayEntry::Deleted,
                (Some(record), Some(bytes)) if record.kind == PathKind::Text => {
                    OverlayEntry::Content(Arc::new(ContentTrigrams::of(bytes)))
                }
                (Some(_), _) => OverlayEntry::Unindexed,
            };
            let content = match (&record, bytes) {
                (Some(record), Some(bytes)) => ChangeContent::recorded(record, bytes),
                _ => ChangeContent::nothing(),
            };
            entries.push((path.clone(), entry, record, content));
        }
        let mut guard = self.write_live();
        let live = &mut *guard;
        if live.generation != generation {
            return false;
        }
        live.sequence += 1;
        self.touch_memory();
        let sequence = live.sequence;
        // Each set is copied only when a view still holds it; otherwise the
        // changed paths are set in place.
        let text = Arc::make_mut(&mut live.text);
        let pending = Arc::make_mut(&mut live.callgraph_pending);
        let unparsed = Arc::make_mut(&mut live.unparsed);
        for (path, entry, record, content) in entries {
            pending.insert(path.clone());
            let change = Change {
                sequence,
                record,
                content,
            };
            if change.failed() {
                unparsed.insert(path.clone());
            } else {
                unparsed.remove(&path);
            }
            text.set(path.clone(), entry);
            live.changes.insert(path, change);
        }
        live.text_members = Arc::default();
        live.state = Some(Arc::new(state));
        true
    }
}
