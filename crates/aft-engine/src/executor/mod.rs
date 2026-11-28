pub use aft_base::executor::*;

/// Stable AFT executor path used by application callers and test fixtures.
pub type Executor<C = crate::context::AppContext> = aft_base::executor::Executor<C>;

#[cfg(test)]
#[allow(unused_imports)]
use crate::path_identity::ProjectRootId;
#[cfg(test)]
#[allow(unused_imports)]
use crate::protocol::Response;
#[cfg(test)]
#[allow(unused_imports)]
use crossbeam_channel::Receiver;
#[cfg(test)]
#[allow(unused_imports)]
use crossbeam_channel::RecvError;
#[cfg(test)]
#[allow(unused_imports)]
use crossbeam_channel::RecvTimeoutError;
#[cfg(test)]
#[allow(unused_imports)]
use crossbeam_channel::Sender;
#[cfg(test)]
#[allow(unused_imports)]
use parking_lot::Condvar;
#[cfg(test)]
#[allow(unused_imports)]
use parking_lot::Mutex;
#[cfg(test)]
#[allow(unused_imports)]
use parking_lot::RwLock;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashSet;
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::VecDeque;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::AtomicU64;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::AtomicU8;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::AtomicUsize;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::atomic::Ordering;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Arc;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::Weak;
#[cfg(test)]
#[allow(unused_imports)]
use std::thread;
#[cfg(test)]
#[allow(unused_imports)]
use std::thread::JoinHandle;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Duration;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::Instant;
#[cfg(test)]
#[allow(unused_imports)]
use tokio::sync::oneshot;

#[cfg(any(test, feature = "test-support"))]
pub(crate) mod test_support;

#[cfg(test)]
#[allow(hidden_glob_reexports)]
mod tests;
