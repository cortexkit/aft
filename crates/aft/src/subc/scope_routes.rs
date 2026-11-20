//! Keep scoped route keys current without rebuilding them when route identities are unchanged.

use super::{stamp_key, RouteChannel, RouteIdentity, RouteIdentityData};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Weak},
};

#[derive(Default)]
pub(super) struct RoutedScopes {
    identities: Vec<Weak<RouteIdentityData>>,
    pub(super) keys: HashSet<(String, String, u64)>,
    #[cfg(test)]
    rebuilds: usize,
    #[cfg(test)]
    key_builds: usize,
}

impl RoutedScopes {
    pub(super) fn update(&mut self, routes: &HashMap<RouteChannel, RouteIdentity>) {
        let scoped = routes.values().filter(|route| route.scope.is_some());
        // Weak pointers prevent address reuse but do not keep closed routes alive.
        // An unchanged map yields the same immutable identities in the same order.
        if scoped
            .clone()
            .map(|route| Arc::as_ptr(&route.0))
            .eq(self.identities.iter().map(Weak::as_ptr))
        {
            return;
        }

        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        self.identities.clear();
        let mut current = HashSet::with_capacity(self.keys.len());
        for route in scoped {
            self.identities.push(Arc::downgrade(&route.0));
            let stamp = route.scope.as_ref().expect("scoped route");
            let key = stamp_key(stamp);
            #[cfg(test)]
            {
                self.key_builds += 1;
            }
            current.insert(key);
        }
        self.keys = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subc::{route_key, test_support, ProjectRootId};

    fn route(root: &ProjectRootId, index: u16, scope: u16) -> RouteIdentity {
        let mut identity = test_support::route_identity(root, &format!("session-{index}"));
        Arc::get_mut(&mut identity.0).unwrap().scope = Some(
            serde_json::from_value(serde_json::json!({
                "owner": {"kind": "reserved", "module_id": "prefrontal"},
                "ref": format!("run:{scope}"), "scope_epoch": 1,
                "kind": "ephemeral", "owner_authorized": true
            }))
            .unwrap(),
        );
        identity
    }

    #[test]
    fn scope_routes_steady_frames_do_not_rebuild_the_routed_key_set() {
        let (_dir, root) = test_support::test_root("scope-route-key-count");
        let mut routes: HashMap<_, _> = (1..=60)
            .map(|i| (route_key(i, 1), route(&root, i, i)))
            .collect();
        let mut routed = RoutedScopes::default();
        let reference = |routes: &HashMap<_, RouteIdentity>| -> HashSet<_> {
            routes
                .values()
                .filter_map(|route| route.scope.as_ref())
                .map(stamp_key)
                .collect()
        };
        routed.update(&routes);
        assert_eq!(routed.keys, reference(&routes));
        assert_eq!((routed.rebuilds, routed.key_builds), (1, 60));

        const FRAMES: usize = 100;
        let before = routed.key_builds;
        let previous = reference(&routes);
        let mut old_key_builds = 0;
        for _ in 0..FRAMES {
            // Without the identity cache, every unchanged frame rebuilds the set.
            let rebuilt: HashSet<_> = routes
                .values()
                .filter_map(|route| route.scope.as_ref())
                .map(|stamp| {
                    old_key_builds += 1;
                    stamp_key(stamp)
                })
                .collect();
            assert_eq!(rebuilt, previous);
            routed.update(&routes);
        }
        assert_eq!(routed.rebuilds, 1);
        assert_eq!(routed.keys, previous);
        let key_builds = routed.key_builds - before;
        assert_eq!(key_builds, 0);
        eprintln!("{FRAMES} steady frames with 60 scoped routes: key builds {old_key_builds} -> {key_builds}; set rebuilds {FRAMES} -> 0");

        routes.insert(route_key(61, 1), route(&root, 61, 61));
        routed.update(&routes);
        routed.update(&routes);
        assert_eq!(routed.rebuilds, 2);
        assert_eq!(routed.keys, reference(&routes));

        routes.insert(route_key(61, 1), route(&root, 61, 60));
        routed.update(&routes);
        assert_eq!(routed.rebuilds, 3);
        assert_eq!(routed.keys, reference(&routes));

        routes.remove(&route_key(60, 1));
        routed.update(&routes);
        routed.update(&routes);
        assert_eq!(routed.rebuilds, 4);
        assert_eq!(routed.keys, reference(&routes));

        for i in [62, 63] {
            routes.insert(route_key(i, 1), route(&root, i, 62));
        }
        routed.update(&routes);
        assert_eq!(routed.keys, reference(&routes));
        assert_eq!(routed.rebuilds, 5);
    }
}
