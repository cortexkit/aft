//! Restrict the gh.route advertiser census to Prefrontal.

use super::{BTreeSet, RouteHolder};

pub(super) fn select_route_holder(advertisers: impl IntoIterator<Item = String>) -> RouteHolder {
    let mut module_id = None;
    let mut unexpected_advertisers = BTreeSet::new();
    for advertiser in advertisers {
        // Governed routes carry identity-bearing writes, so only prefrontal may
        // hold `gh.route`; another module advertising it must not capture the route.
        // The holder module identifies the routing server, not the bound agent. Using
        // its module ID would merge all agents into one audit and rate-accounting session.
        if crate::access_policy::prefrontal::is_module(&advertiser) {
            module_id = Some(advertiser);
        } else {
            unexpected_advertisers.insert(advertiser);
        }
    }
    RouteHolder {
        module_id,
        unexpected_advertisers: unexpected_advertisers.into_iter().collect(),
    }
}
