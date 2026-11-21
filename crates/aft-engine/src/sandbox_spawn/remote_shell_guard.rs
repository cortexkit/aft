//! The shim identifies remote shells from an inherited native policy, not environment variables.

#[cfg(target_os = "linux")]
const PROBE: libc::c_int = 0x4154_4652;

pub(crate) fn active() -> bool {
    #[cfg(target_os = "linux")]
    {
        // An unknown prctl returns EINVAL without the remote filter.
        let result = unsafe { libc::prctl(PROBE, 0, 0, 0, 0) };
        result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EREMOTE)
    }
    #[cfg(not(target_os = "linux"))]
    false
}

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
