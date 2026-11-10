//! Read-only operator views of AFT's shell registry.
mod admission;
mod output;
mod runtime;
mod state;
mod wire;
pub(super) use admission::{is_request, operator, operator_required};
pub(super) use runtime::Service;
pub(super) use wire::CAPABILITY;

#[cfg(test)]
mod samples;
