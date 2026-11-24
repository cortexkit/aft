pub fn must_not_cross_bin_boundary(body: &[u8]) -> String {
    crate::support::route_client::error_reason(body)
}
