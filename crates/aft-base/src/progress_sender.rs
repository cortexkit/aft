use crate::protocol::PushFrame;
use std::sync::{Arc, Mutex};

pub type ProgressSender = Arc<Box<dyn Fn(PushFrame) + Send + Sync>>;
pub type SharedProgressSender = Arc<Mutex<Option<ProgressSender>>>;
