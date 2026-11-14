pub use aft_base::fs_lock::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::{slog_debug, slog_error, slog_info, slog_warn};
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::fmt;
#[cfg(test)]
#[allow(unused_imports)]
use std::fs::{self, File, OpenOptions};
#[cfg(test)]
#[allow(unused_imports)]
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc, Arc, Mutex, OnceLock, Weak,
};
#[cfg(test)]
#[allow(unused_imports)]
use std::thread::{self, JoinHandle};
#[cfg(test)]
#[allow(unused_imports)]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn semantic_batch_lease_macos_birth_matches_live_libproc() {
        let pid = std::process::id();
        let identity = process_identity(pid).expect("kernel identity for this process");
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
        let written = unsafe { proc_pidinfo(pid as i32, 3, 0, info.as_mut_ptr().cast(), size) };
        assert_eq!(written, size, "libproc must answer for this live process");
        let info = unsafe { info.assume_init() };
        assert_eq!(
            identity.start_time,
            info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec
        );
        assert!(!identity.exited);
        assert_eq!(
            crate::root_cache::process_start_time_ms(pid),
            Some(identity.start_time / 1_000)
        );
    }
}
