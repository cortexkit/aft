//! The logger thread: names and writes the events the allocator recorded.
//!
//! Each event becomes one WARN line. Frames are printed as an image name
//! and an offset into that image (`aft+0x1a2b3c`), with the main image's
//! build id on the line, because the shipped binary is stripped: its
//! symbols live in a separate dSYM (macOS) or debug file (Linux) that
//! `atos -o <dwarf> -offset` or `addr2line -e <debug>` reads later. Where
//! the loader knows a symbol for a frame, its name follows the offset.

use std::fmt::Write as _;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::ring::{self, Event};
use super::{label_name, EventKind, THREAD};

/// Lines written per minute at most; the rest are counted and summarized.
const LINES_PER_MINUTE: u32 = 20;

static LOGGER: OnceLock<std::thread::Thread> = OnceLock::new();
static SINK: OnceLock<fn(&str)> = OnceLock::new();

struct Limiter {
    window_started: Option<Instant>,
    written: u32,
    suppressed: u64,
    dropped_reported: u64,
}

static LIMITER: Mutex<Limiter> = Mutex::new(Limiter {
    window_started: None,
    written: 0,
    suppressed: 0,
    dropped_reported: 0,
});

/// Starts the logger thread once. Events recorded before it starts wait in
/// the ring, which keeps the most recent ones that fit.
pub fn start_logger() {
    if LOGGER.get().is_some() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("aft-alloc-trace".to_string())
        .spawn(|| {
            // Nothing this thread allocates is traced, so writing an event
            // can never record another.
            THREAD.with(|thread| thread.busy.set(true));
            loop {
                drain_now();
                std::thread::park_timeout(Duration::from_secs(5));
            }
        });
    match spawned {
        Ok(handle) => {
            let _ = LOGGER.set(handle.thread().clone());
        }
        Err(error) => log::warn!("allocation tracing: logger thread did not start: {error}"),
    }
}

/// Also hands every written line to `sink`. Set once; for tests.
#[doc(hidden)]
pub fn set_line_sink(sink: fn(&str)) {
    let _ = SINK.set(sink);
}

pub(super) fn logger_started() -> bool {
    LOGGER.get().is_some()
}

pub(super) fn wake() {
    if let Some(logger) = LOGGER.get() {
        logger.unpark();
    }
}

/// Writes every queued event now, on the calling thread.
pub fn drain_now() {
    let was_busy = THREAD.with(|thread| thread.busy.replace(true));
    let mut events = Vec::new();
    ring::drain(&mut events);
    let dropped = ring::dropped();
    let mut limiter = LIMITER.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let window_over = limiter
        .window_started
        .is_none_or(|started| now.duration_since(started) >= Duration::from_secs(60));
    if window_over {
        let suppressed = std::mem::take(&mut limiter.suppressed);
        let newly_dropped = dropped - limiter.dropped_reported;
        if suppressed > 0 || newly_dropped > 0 {
            write_line(&format!(
                "allocation tracing: {suppressed} event(s) not written in the last minute over the {LINES_PER_MINUTE}-line limit, {newly_dropped} lost to a full queue"
            ));
            limiter.dropped_reported = dropped;
        }
        limiter.window_started = Some(now);
        limiter.written = 0;
    }
    for event in &events {
        if limiter.written >= LINES_PER_MINUTE {
            limiter.suppressed += 1;
            continue;
        }
        limiter.written += 1;
        write_line(&format_event(event));
    }
    drop(limiter);
    THREAD.with(|thread| thread.busy.set(was_busy));
}

fn write_line(line: &str) {
    log::warn!("{line}");
    if let Some(sink) = SINK.get() {
        sink(line);
    }
}

pub(super) fn format_event(event: &Event) -> String {
    let mut line = String::with_capacity(256 + event.frames().len() * 24);
    let label = match label_name(event.label) {
        "" => "none",
        name => name,
    };
    match event.kind {
        EventKind::Alloc => {
            let _ = write!(line, "large allocation {} alloc", mib(event.size));
        }
        EventKind::Realloc => {
            let _ = write!(
                line,
                "large allocation {} realloc from {}",
                mib(event.size),
                mib(event.old_size)
            );
        }
        EventKind::BurstCrossed => {
            let _ = write!(
                line,
                "allocation burst: op={label} live {} crossed {}",
                mib(event.size),
                mib(super::LABEL_BURST_BYTES as usize)
            );
        }
        EventKind::BurstEnded => {
            let _ = write!(
                line,
                "allocation burst ended: op={label} peak {} over {:.1}s, live {} now",
                mib(event.size),
                event.duration_ns as f64 / 1e9,
                mib(event.old_size)
            );
        }
    }
    if event.thread_len > 0 {
        let _ = write!(line, " thread={}", event.thread_name());
    }
    if matches!(event.kind, EventKind::Alloc | EventKind::Realloc) {
        let _ = write!(line, " op={label}");
    }
    if !event.root.as_str().is_empty() {
        let _ = write!(line, " root={}", event.root.as_str());
    }
    if !event.frames().is_empty() {
        let _ = write!(line, " build_id={}", build_id());
        line.push_str(" frames=");
        for (index, &ip) in event.frames().iter().enumerate() {
            if index > 0 {
                line.push(',');
            }
            push_frame(&mut line, ip);
        }
    }
    line
}

fn mib(bytes: usize) -> String {
    let mib = bytes as f64 / f64::from(1 << 20);
    if mib >= 10.0 {
        format!("{mib:.0} MiB")
    } else {
        format!("{mib:.1} MiB")
    }
}

/// One frame as `image+0xoffset`, then `[symbol]` when the loader knows one.
fn push_frame(line: &mut String, ip: usize) {
    match frame_location(ip) {
        Some(location) => {
            let _ = write!(line, "{}+{:#x}", location.image, ip - location.base);
            if let Some(symbol) = location.symbol {
                let _ = write!(line, "[{symbol}]");
            }
        }
        None => {
            let _ = write!(line, "{ip:#x}");
        }
    }
}

struct FrameLocation {
    image: String,
    base: usize,
    symbol: Option<String>,
}

#[cfg(unix)]
fn frame_location(ip: usize) -> Option<FrameLocation> {
    let mut info = std::mem::MaybeUninit::<libc::Dl_info>::zeroed();
    // SAFETY: dladdr only reads the loader's tables and fills `info`.
    if unsafe { libc::dladdr(ip as *const libc::c_void, info.as_mut_ptr()) } == 0 {
        return None;
    }
    // SAFETY: dladdr succeeded, so `info` is initialized.
    let info = unsafe { info.assume_init() };
    if info.dli_fbase.is_null() || (info.dli_fbase as usize) > ip {
        return None;
    }
    let image = if info.dli_fname.is_null() {
        "?".to_string()
    } else {
        // SAFETY: the loader's image path is a NUL-terminated string.
        let path = unsafe { std::ffi::CStr::from_ptr(info.dli_fname) }.to_string_lossy();
        path.rsplit('/').next().unwrap_or(&path).to_string()
    };
    let symbol = (!info.dli_sname.is_null()).then(|| {
        // SAFETY: as for the image path.
        let raw = unsafe { std::ffi::CStr::from_ptr(info.dli_sname) }.to_string_lossy();
        format!("{:#}", rustc_demangle::demangle(&raw))
    });
    Some(FrameLocation {
        image,
        base: info.dli_fbase as usize,
        symbol,
    })
}

#[cfg(not(unix))]
fn frame_location(_ip: usize) -> Option<FrameLocation> {
    None
}

/// The running binary's build id: the Mach-O UUID on macOS, the GNU build
/// id on Linux, as uppercase hex without separators. That is how
/// `aft profile` names the folder it keeps a binary's debug symbols in.
pub(super) fn build_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| read_build_id().unwrap_or_else(|| "unknown".to_string()))
}

#[cfg(target_os = "macos")]
fn read_build_id() -> Option<String> {
    const LC_UUID: u32 = 0x1b;
    // SAFETY: image 0 is the main executable, whose header and load commands
    // stay mapped for the life of the process; each command is read within
    // the size the header declares.
    unsafe {
        let header = libc::_dyld_get_image_header(0);
        if header.is_null() {
            return None;
        }
        let header = &*header.cast::<libc::mach_header_64>();
        let mut command = (header as *const libc::mach_header_64)
            .cast::<u8>()
            .add(std::mem::size_of::<libc::mach_header_64>());
        for _ in 0..header.ncmds {
            let cmd = command.cast::<u32>().read_unaligned();
            let size = command.add(4).cast::<u32>().read_unaligned() as usize;
            if cmd == LC_UUID && size >= 24 {
                let uuid = std::slice::from_raw_parts(command.add(8), 16);
                return Some(format_uuid(uuid));
            }
            if size == 0 {
                return None;
            }
            command = command.add(size);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn format_uuid(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

#[cfg(target_os = "linux")]
fn read_build_id() -> Option<String> {
    unsafe extern "C" fn first_image(
        info: *mut libc::dl_phdr_info,
        _size: libc::size_t,
        data: *mut libc::c_void,
    ) -> libc::c_int {
        // SAFETY: called by dl_iterate_phdr with a valid record; `data` is
        // the Option the caller passed.
        let out = unsafe { &mut *data.cast::<Option<String>>() };
        let info = unsafe { &*info };
        for index in 0..usize::from(info.dlpi_phnum) {
            let header = unsafe { &*info.dlpi_phdr.add(index) };
            if header.p_type != libc::PT_NOTE {
                continue;
            }
            let start = (info.dlpi_addr as usize + header.p_vaddr as usize) as *const u8;
            let notes = unsafe { std::slice::from_raw_parts(start, header.p_memsz as usize) };
            if let Some(id) = gnu_build_id(notes) {
                *out = Some(id);
                break;
            }
        }
        // The first image is the executable; stop after it.
        1
    }
    let mut out: Option<String> = None;
    // SAFETY: the callback only reads the loader's records.
    unsafe {
        libc::dl_iterate_phdr(Some(first_image), (&mut out as *mut Option<String>).cast());
    }
    out
}

#[cfg(target_os = "linux")]
fn gnu_build_id(mut notes: &[u8]) -> Option<String> {
    const NT_GNU_BUILD_ID: u32 = 3;
    let word = |bytes: &[u8]| u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let align = |n: usize| (n + 3) & !3;
    while notes.len() >= 12 {
        let name_size = word(&notes[0..4]) as usize;
        let desc_size = word(&notes[4..8]) as usize;
        let kind = word(&notes[8..12]);
        let name_end = 12 + align(name_size);
        let desc_end = name_end + align(desc_size);
        if desc_end > notes.len() {
            return None;
        }
        if kind == NT_GNU_BUILD_ID && &notes[12..12 + name_size] == b"GNU\0" {
            let desc = &notes[name_end..name_end + desc_size];
            return Some(desc.iter().map(|byte| format!("{byte:02X}")).collect());
        }
        notes = &notes[desc_end..];
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn read_build_id() -> Option<String> {
    None
}
