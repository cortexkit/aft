use super::*;

#[derive(Default)]
pub(super) struct IndexedScopes {
    all: HashMap<scope_watch::Key, ScopeStamp>,
    active: HashMap<scope_watch::Key, ScopeStamp>,
    epochs: HashMap<String, HashMap<String, Vec<u64>>>,
    dirty: bool,
    #[cfg(test)]
    rebuilds: usize,
}

impl IndexedScopes {
    pub(super) fn refresh(
        &mut self,
        inventory: &scope_watch::Inventory,
        current: &scope_watch::Inventory,
    ) {
        if inventory.sources == current.sources {
            return;
        }
        self.all = current.scopes();
        self.dirty = true;
    }

    pub(super) fn all(&self) -> &HashMap<scope_watch::Key, ScopeStamp> {
        &self.all
    }

    pub(super) fn active(&self) -> &HashMap<scope_watch::Key, ScopeStamp> {
        &self.active
    }

    pub(super) fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub(super) fn reconcile(&mut self, confirmed: &HashSet<scope_watch::Key>) -> bool {
        if !std::mem::take(&mut self.dirty) {
            return false;
        }
        self.active.clear();
        self.epochs.clear();
        for (key, stamp) in self.all.iter().filter(|(key, _)| !confirmed.contains(*key)) {
            self.active.insert(key.clone(), stamp.clone());
            self.epochs
                .entry(key.0.clone())
                .or_default()
                .entry(key.1.clone())
                .or_default()
                .push(key.2);
        }
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        true
    }

    pub(super) fn group(
        &self,
        reference: &ScopeReference,
    ) -> impl Iterator<Item = (scope_watch::Key, ScopeStamp)> + '_ {
        let (owner, reference) = reference.clone();
        self.epochs
            .get(&owner)
            .and_then(|refs| refs.get(&reference))
            .into_iter()
            .flatten()
            .map(move |epoch| {
                let key = (owner.clone(), reference.clone(), *epoch);
                let stamp = self.active[&key].clone();
                (key, stamp)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_inventory_reuses_index_and_groups_only_matching_epochs() {
        let stamp = |name: &str, epoch| {
            serde_json::from_value::<ScopeStamp>(json!({
            "owner":{"kind":"direct"}, "ref": name, "scope_epoch":epoch, "kind":"head", "owner_authorized":true
        })).unwrap()
        };
        let empty = scope_watch::Inventory::default();
        let mut inventory = scope_watch::Inventory::default();
        inventory.add(
            scope_watch::Source::Shell,
            [stamp("a", 1), stamp("a", 2), stamp("b", 1)],
        );
        let mut indexed = IndexedScopes::default();
        indexed.refresh(&empty, &inventory);
        assert!(indexed.reconcile(&HashSet::new()));
        let mut keys = indexed
            .group(&("direct".into(), "a".into()))
            .map(|(key, _)| key.2)
            .collect::<Vec<_>>();
        keys.sort();
        assert_eq!(keys, [1, 2]);
        for _ in 0..10 {
            indexed.refresh(&inventory, &inventory);
            assert!(!indexed.reconcile(&HashSet::new()));
        }
        assert_eq!(indexed.rebuilds, 1);
        indexed.mark_dirty();
        indexed.reconcile(&HashSet::from([stamp_key(&stamp("a", 1))]));
        assert_eq!(indexed.group(&("direct".into(), "a".into())).count(), 1);
    }
}
