//! The shim identifies remote shells from an inherited native policy, not environment variables.

pub(crate) use aft_base::remote_shell_marker::active;
#[cfg(target_os = "linux")]
use aft_base::remote_shell_marker::PROBE;

#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn verify() -> Result<(), String> {
    active()
        .then_some(())
        .ok_or_else(|| "remote shell kernel marker is not active".to_owned())
}

#[cfg(target_os = "linux")]
#[path = "remote_shell_guard_linux.rs"]
mod linux;
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub use linux::apply;
