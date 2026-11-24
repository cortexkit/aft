//! Read the native marker inherited by a remote shell.

#[cfg(target_os = "linux")]
#[doc(hidden)]
pub const PROBE: libc::c_int = 0x4154_4652;

#[doc(hidden)]
pub fn active() -> bool {
    #[cfg(target_os = "linux")]
    {
        // An unknown prctl returns EINVAL without the remote filter.
        let result = unsafe { libc::prctl(PROBE, 0, 0, 0, 0) };
        result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EREMOTE)
    }
    #[cfg(not(target_os = "linux"))]
    false
}
