pub use aft_base::secret_proxy::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::secret::vault::SecretVault;
#[cfg(test)]
#[allow(unused_imports)]
use crate::secret::SecretScope;
#[cfg(test)]
#[allow(unused_imports)]
use base64::Engine;
#[cfg(test)]
#[allow(unused_imports)]
use bytes::Bytes;
#[cfg(test)]
#[allow(unused_imports)]
use http_body_util::{BodyExt, Full};
#[cfg(test)]
#[allow(unused_imports)]
use hyper::body::Incoming;
#[cfg(test)]
#[allow(unused_imports)]
use hyper::service::service_fn;
#[cfg(test)]
#[allow(unused_imports)]
use hyper::{Method, Request, Response, StatusCode};
#[cfg(test)]
#[allow(unused_imports)]
use hyper_util::rt::{TokioExecutor, TokioIo};
#[cfg(test)]
#[allow(unused_imports)]
use parking_lot::Mutex;
#[cfg(test)]
#[allow(unused_imports)]
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::scrub::{MatcherSet, Scrubber};
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::target::ConnectAuthority;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::BTreeSet;
#[cfg(test)]
#[allow(unused_imports)]
use std::net::{Ipv4Addr, SocketAddr};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Arc;
#[cfg(test)]
#[allow(unused_imports)]
use std::thread::JoinHandle;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Duration;
#[cfg(test)]
#[allow(unused_imports)]
use tokio::net::{TcpListener, TcpStream};
#[cfg(test)]
#[allow(unused_imports)]
use tokio::sync::watch;
#[cfg(test)]
#[allow(unused_imports)]
use zeroize::Zeroizing;

#[cfg(all(test, unix))]
mod tests;
