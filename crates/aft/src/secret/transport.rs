pub use aft_base::secret::transport::*;

#[cfg(test)]
#[allow(unused_imports)]
use super::vault::SecretUseTransport;
#[cfg(test)]
#[allow(unused_imports)]
use diagnostics::{Failure, Stage};
#[cfg(test)]
#[allow(unused_imports)]
use serde_json::{json, Value};
#[cfg(test)]
#[allow(unused_imports)]
use std::path::PathBuf;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{Arc, Mutex, OnceLock};
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Duration;
#[cfg(test)]
#[allow(unused_imports)]
use subc_client_rs::{
    CallError, CallOptions, CloseRouteOptions, ConsumerIdentity, ConsumerOptions, RouteHandle,
    SubcConsumer, SubscribeOptions, Subscription,
};
#[cfg(test)]
#[allow(unused_imports)]
use subc_protocol::scope::{ScopeSelector, ScopeStamp};
#[cfg(test)]
#[allow(unused_imports)]
use subc_protocol::{BindIdentity, RouteTarget};
#[cfg(test)]
#[allow(unused_imports)]
use zeroize::Zeroizing;

#[cfg(test)]
#[path = "transport_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "transport_fake.rs"]
pub(crate) mod fake;
