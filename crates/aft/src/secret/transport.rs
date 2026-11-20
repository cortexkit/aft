pub use aft_base::secret::transport::*;

#[cfg(test)]
#[allow(unused_imports)]
use super::vault::SecretUseTransport;

#[cfg(test)]
#[allow(unused_imports)]
use serde_json::json;
#[cfg(test)]
#[allow(unused_imports)]
use serde_json::Value;
#[cfg(test)]
#[allow(unused_imports)]
use std::path::PathBuf;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Arc;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Mutex;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::OnceLock;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Duration;
#[cfg(test)]
#[allow(unused_imports)]
use subc_client_rs::CallError;
#[cfg(test)]
#[allow(unused_imports)]
use subc_client_rs::CallOptions;
#[cfg(test)]
#[allow(unused_imports)]
use subc_client_rs::CloseRouteOptions;
#[cfg(test)]
#[allow(unused_imports)]
use subc_client_rs::ConsumerIdentity;
#[cfg(test)]
#[allow(unused_imports)]
use subc_client_rs::ConsumerOptions;
#[cfg(test)]
#[allow(unused_imports)]
use subc_client_rs::RouteHandle;
#[cfg(test)]
#[allow(unused_imports)]
use subc_client_rs::SubcConsumer;
#[cfg(test)]
#[allow(unused_imports)]
use subc_client_rs::SubscribeOptions;
#[cfg(test)]
#[allow(unused_imports)]
use subc_client_rs::Subscription;
#[cfg(test)]
#[allow(unused_imports)]
use subc_protocol::scope::ScopeSelector;
#[cfg(test)]
#[allow(unused_imports)]
use subc_protocol::scope::ScopeStamp;
#[cfg(test)]
#[allow(unused_imports)]
use subc_protocol::BindIdentity;
#[cfg(test)]
#[allow(unused_imports)]
use subc_protocol::RouteTarget;
#[cfg(test)]
#[allow(unused_imports)]
use zeroize::Zeroizing;

#[cfg(test)]
#[path = "transport_tests.rs"]
#[allow(hidden_glob_reexports)]
mod tests;
