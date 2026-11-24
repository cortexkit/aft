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
