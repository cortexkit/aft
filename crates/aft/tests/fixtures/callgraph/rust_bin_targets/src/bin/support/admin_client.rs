use super::route_client;

pub fn first(body: &[u8]) -> String {
    route_client::error_reason(body)
}

pub fn second(body: &[u8]) -> String {
    route_client::error_reason(body)
}

pub fn via_super(body: &[u8]) -> String {
    super::route_client::error_reason(body)
}

pub fn via_crate(body: &[u8]) -> String {
    crate::support::route_client::error_reason(body)
}
