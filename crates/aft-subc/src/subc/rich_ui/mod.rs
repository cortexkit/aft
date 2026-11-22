//! Provider-owned display records and authorization for app-server reads made
//! on behalf of a daemon-stamped scope. Storage and templates follow
//! tool-display/v1 and tool.views; AFT still checks each read's session ownership,
//! cursor binding and exact call-key replay before dispatch.

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod admission;
pub(crate) mod authorization;
mod bash_output;
mod bash_watch;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod carrier;
pub(crate) mod display;
mod jcs_number;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod operations;
pub(crate) mod outbox;
pub(crate) mod records;
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod schema;
pub(super) mod session;
mod session_reads;
