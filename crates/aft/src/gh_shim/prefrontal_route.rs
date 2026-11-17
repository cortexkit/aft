//! Restrict the gh.route advertiser census to prefrontal's reserved aliases.

use super::{BTreeSet, RouteHolder};

pub(super) fn select_route_holder(advertisers: impl IntoIterator<Item = String>) -> RouteHolder {
    let mut holders = Vec::new();
    let mut unexpected_advertisers = BTreeSet::new();
    for advertiser in advertisers {
        // Governed routes carry identity-bearing writes, so only prefrontal may
        // hold `gh.route`; another module advertising it must not capture the route.
        // The holder module identifies the routing server, not the bound agent. Using
        // its module ID would merge all agents into one audit and rate-accounting session.
        if crate::prefrontal::is_module(&advertiser) {
            holders.push(advertiser);
        } else {
            unexpected_advertisers.insert(advertiser);
        }
    }
    RouteHolder {
        module_id: (!holders.is_empty()).then(|| {
            crate::prefrontal::catalog_module(holders.iter().map(String::as_str)).to_string()
        }),
        unexpected_advertisers: unexpected_advertisers.into_iter().collect(),
    }
}
