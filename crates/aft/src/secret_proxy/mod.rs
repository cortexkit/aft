pub use aft_base::secret_proxy::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::secret::vault::SecretVault;

#[cfg(test)]
#[allow(unused_imports)]
use base64::Engine;
#[cfg(test)]
#[allow(unused_imports)]
use bytes::Bytes;
#[cfg(test)]
#[allow(unused_imports)]
use http_body_util::BodyExt;
#[cfg(test)]
#[allow(unused_imports)]
use http_body_util::Full;
#[cfg(test)]
#[allow(unused_imports)]
use hyper::body::Incoming;
#[cfg(test)]
#[allow(unused_imports)]
use hyper::service::service_fn;
#[cfg(test)]
#[allow(unused_imports)]
use hyper::Method;
#[cfg(test)]
#[allow(unused_imports)]
use hyper::Request;
#[cfg(test)]
#[allow(unused_imports)]
use hyper::Response;
#[cfg(test)]
#[allow(unused_imports)]
use hyper::StatusCode;
#[cfg(test)]
#[allow(unused_imports)]
use hyper_util::rt::TokioExecutor;
#[cfg(test)]
#[allow(unused_imports)]
use hyper_util::rt::TokioIo;
#[cfg(test)]
#[allow(unused_imports)]
use parking_lot::Mutex;
#[cfg(test)]
#[allow(unused_imports)]
use rcgen::BasicConstraints;
#[cfg(test)]
#[allow(unused_imports)]
use rcgen::CertificateParams;
#[cfg(test)]
#[allow(unused_imports)]
use rcgen::IsCa;
#[cfg(test)]
#[allow(unused_imports)]
use rcgen::KeyPair;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::scrub::MatcherSet;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::scrub::Scrubber;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::target::ConnectAuthority;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::BTreeSet;
#[cfg(test)]
#[allow(unused_imports)]
use std::net::Ipv4Addr;
#[cfg(test)]
#[allow(unused_imports)]
use std::net::SocketAddr;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::AtomicI32;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::Ordering;
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
use tokio::net::TcpListener;
#[cfg(test)]
#[allow(unused_imports)]
use tokio::net::TcpStream;
#[cfg(test)]
#[allow(unused_imports)]
use tokio::sync::watch;
#[cfg(test)]
#[allow(unused_imports)]
use zeroize::Zeroizing;

#[cfg(all(test, unix))]
mod tests;
