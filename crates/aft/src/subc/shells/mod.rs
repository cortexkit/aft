//! Read-only operator views of AFT's shell registry.
mod admission;
mod capture;
mod output;
mod runtime;
mod state;
mod wire;
pub(super) use admission::{is_request, operator, operator_refusal, operator_required};
pub(super) use runtime::{project_status, Service};
pub(super) use wire::CAPABILITY;

#[cfg(test)]
mod samples;
