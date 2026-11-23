//! Machine-local resource limits shared by indexing and query caches.

use serde::Serialize;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::OnceLock;

pub use crate::memory_profile::{MemoryMode, MemoryProfile};

#[derive(Debug, Clone, Serialize)]
pub struct EffectiveMemory {
    pub value: MemoryMode,
    pub source: &'static str,
    pub total_ram_bytes: Option<u64>,
}

pub fn effective(configured: Option<MemoryMode>) -> EffectiveMemory {
    static RAM: OnceLock<Option<u64>> = OnceLock::new();
    let total_ram_bytes = *RAM.get_or_init(total_physical_ram);
    EffectiveMemory {
        value: configured.unwrap_or_else(|| MemoryMode::automatic(total_ram_bytes)),
        source: if configured.is_some() {
            "config"
        } else {
            "auto"
        },
        total_ram_bytes,
    }
}

static CONFIGURED: AtomicU8 = AtomicU8::new(0);
static PUBLICATION: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn current_effective() -> EffectiveMemory {
    effective(match CONFIGURED.load(Ordering::Acquire) {
        1 => Some(MemoryMode::Lean),
        2 => Some(MemoryMode::Balanced),
        _ => None,
    })
}

/// Publish only from startup and configuration publication, never constructors.
#[doc(hidden)]
pub fn publish_with_notification(configured: Option<MemoryMode>, notify: &dyn Fn()) {
    let _publication = PUBLICATION.lock().unwrap_or_else(|e| e.into_inner());
    apply_allocator_decay(effective(configured).value);
    CONFIGURED.store(
        match configured {
            None => 0,
            Some(MemoryMode::Lean) => 1,
            Some(MemoryMode::Balanced) => 2,
        },
        Ordering::Release,
    );
    notify();
}

thread_local! {
    static ADMITTED: std::cell::Cell<Option<MemoryProfile>> = const { std::cell::Cell::new(None) };
}

/// Running work keeps the profile captured when it was admitted.
#[doc(hidden)]
pub fn admitted<T>(profile: MemoryProfile, work: impl FnOnce() -> T) -> T {
    struct Restore(Option<MemoryProfile>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ADMITTED.set(self.0);
        }
    }
    let _restore = Restore(ADMITTED.replace(Some(profile)));
    work()
}

/// Each admission or cache sizing reads the latest published profile.
pub fn current() -> MemoryProfile {
    ADMITTED
        .get()
        .unwrap_or_else(|| current_effective().value.profile())
}

#[cfg(target_os = "linux")]
fn apply_allocator_decay(mode: MemoryMode) {
    use std::sync::Mutex;
    static APPLIED: Mutex<Option<MemoryMode>> = Mutex::new(None);
    let mut applied = APPLIED.lock().unwrap_or_else(|e| e.into_inner());
    if *applied == Some(mode) {
        return;
    }
    // opt.* retains startup defaults even after arenas.* has been changed.
    let result = (|| -> Result<(), tikv_jemalloc_ctl::Error> {
        for (default_key, global_key, arena_key) in [
            (
                b"opt.dirty_decay_ms\0".as_slice(),
                b"arenas.dirty_decay_ms\0".as_slice(),
                "dirty_decay_ms",
            ),
            (
                b"opt.muzzy_decay_ms\0".as_slice(),
                b"arenas.muzzy_decay_ms\0".as_slice(),
                "muzzy_decay_ms",
            ),
        ] {
            unsafe {
                let default = tikv_jemalloc_ctl::raw::read::<isize>(default_key)?;
                let value = mode.profile().decay_ms.unwrap_or(default);
                tikv_jemalloc_ctl::raw::write(global_key, value)?;
                // Per-arena decay controls must be updated individually for
                // initialized arenas; arenas.* above supplies the default
                // for future arenas.
                let arenas = tikv_jemalloc_ctl::raw::read::<u32>(b"arenas.narenas\0")?;
                for arena in 0..arenas {
                    let key = format!("arena.{arena}.{arena_key}\0");
                    if tikv_jemalloc_ctl::raw::read::<isize>(key.as_bytes()).is_ok() {
                        tikv_jemalloc_ctl::raw::write(key.as_bytes(), value)?;
                    }
                }
            }
        }
        Ok(())
    })();
    if result.is_ok() {
        *applied = Some(mode);
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_allocator_decay(_mode: MemoryMode) {}

#[cfg(target_os = "linux")]
fn total_physical_ram() -> Option<u64> {
    let pages = unsafe { libc::sysconf(libc::_SC_PHYS_PAGES) };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if pages <= 0 || page_size <= 0 {
        return None;
    }
    (pages as u64).checked_mul(page_size as u64)
}

#[cfg(target_os = "macos")]
fn total_physical_ram() -> Option<u64> {
    let mut bytes = 0_u64;
    let mut len = std::mem::size_of::<u64>();
    let result = unsafe {
        libc::sysctlbyname(
            c"hw.memsize".as_ptr(),
            (&mut bytes as *mut u64).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && bytes > 0).then_some(bytes)
}

#[cfg(windows)]
fn total_physical_ram() -> Option<u64> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    }
    let mut status: MemoryStatusEx = unsafe { std::mem::zeroed() };
    status.length = std::mem::size_of::<MemoryStatusEx>() as u32;
    let result = unsafe { GlobalMemoryStatusEx(&mut status) };
    (result != 0 && status.total_phys > 0).then_some(status.total_phys)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn total_physical_ram() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_effective_reports_config_or_auto_and_total_ram() {
        let automatic = effective(None);
        assert_eq!(automatic.source, "auto");
        assert_eq!(
            automatic.value,
            MemoryMode::automatic(automatic.total_ram_bytes)
        );
        for value in [MemoryMode::Lean, MemoryMode::Balanced] {
            let configured = effective(Some(value));
            assert_eq!(configured.source, "config");
            assert_eq!(configured.value, value);
            assert_eq!(configured.total_ram_bytes, automatic.total_ram_bytes);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn memory_linux_decay_updates_existing_and_future_arenas_and_restores_defaults() {
        use tikv_jemalloc_ctl::raw;
        let _publication = PUBLICATION.lock().unwrap_or_else(|e| e.into_inner());
        let arena = unsafe { raw::read::<u32>(b"thread.arena\0") }.unwrap();
        apply_allocator_decay(MemoryMode::Lean);
        for name in ["dirty_decay_ms", "muzzy_decay_ms"] {
            assert_eq!(
                unsafe { raw::read::<isize>(format!("arenas.{name}\0").as_bytes()) }.unwrap(),
                0
            );
            assert_eq!(
                unsafe { raw::read::<isize>(format!("arena.{arena}.{name}\0").as_bytes()) }
                    .unwrap(),
                0
            );
        }
        apply_allocator_decay(MemoryMode::Balanced);
        for name in ["dirty_decay_ms", "muzzy_decay_ms"] {
            let default =
                unsafe { raw::read::<isize>(format!("opt.{name}\0").as_bytes()) }.unwrap();
            assert_eq!(
                unsafe { raw::read::<isize>(format!("arenas.{name}\0").as_bytes()) }.unwrap(),
                default
            );
            assert_eq!(
                unsafe { raw::read::<isize>(format!("arena.{arena}.{name}\0").as_bytes()) }
                    .unwrap(),
                default
            );
        }
    }

    #[test]
    fn publication_notifier_observes_the_new_profile_once_inside_the_guard() {
        let observed = std::cell::RefCell::new(Vec::new());
        publish_with_notification(Some(MemoryMode::Lean), &|| {
            assert!(matches!(
                PUBLICATION.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
            observed.borrow_mut().push(current_effective().value);
        });
        publish_with_notification(None, &|| {});
        assert_eq!(*observed.borrow(), [MemoryMode::Lean]);
    }
}
