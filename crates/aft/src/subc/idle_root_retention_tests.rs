//! What an idle-evicted root keeps in the process: contexts, threads and heap.
//!
//! The measurement runs in a child copy of this test binary so no other test
//! starts or stops threads while it counts them.

use super::*;
use std::sync::Weak;

const CHILD_ENV: &str = "AFT_TEST_IDLE_ROOT_RETENTION_CHILD";
const ROOTS: usize = 12;

fn run_in_child(test_name: &str) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, "1")
        // Other tests in this process set it while they run; the child must
        // start the real watchers whatever this process held at spawn time.
        .env_remove("AFT_TEST_DISABLE_FILE_WATCHER")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    println!("{stdout}");
    assert!(
        output.status.success(),
        "measurement child failed:\n{stdout}\n{stderr}"
    );
    assert!(stdout.contains("1 passed"), "child ran no test:\n{stdout}");
}

/// This process's thread count. A census that counts nothing would make every
/// retention check below pass without measuring, so it fails the test
/// instead: the measuring thread itself always exists.
fn thread_total() -> u64 {
    let total = crate::lifecycle_census::thread_census().total;
    assert!(
        total > 0,
        "the thread census cannot count threads on this platform; the retention checks would be vacuous"
    );
    total
}

/// One thread of this process, as the retention checks report it.
#[derive(Debug)]
struct ThreadRecord {
    /// Kernel thread ID: the Mach thread ID on macOS (the number Rust prints
    /// in panic messages), the task ID on Linux, the Toolhelp thread ID on
    /// Windows.
    id: u64,
    name: Option<String>,
    /// How the thread started: its stack entry on macOS, or its Win32 start
    /// address and containing module on Windows. Not available on Linux.
    start: Option<String>,
    /// An OS-managed pool thread: a macOS workqueue entry or a Windows
    /// thread whose Win32 start address belongs to ntdll.dll. The OS starts,
    /// parks and retires these workers independently of root teardown.
    /// AFT's own threads start through pthread_create on macOS and an
    /// executable-resident Rust trampoline on Windows; neither is excluded.
    /// An unreadable start address is always counted as potentially owned.
    os_workqueue: bool,
}

impl std::fmt::Display for ThreadRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "thread {} {:?} started by {}",
            self.id,
            self.name.as_deref().unwrap_or("<unnamed>"),
            self.start
                .as_deref()
                .unwrap_or("<start routine not readable on this platform>")
        )
    }
}

/// The IDs of this process's threads. Like [`thread_total`], an empty list
/// fails the test: the measuring thread itself always exists.
fn thread_ids() -> std::collections::HashSet<u64> {
    let ids = thread_inventory::ids();
    assert!(
        !ids.is_empty(),
        "cannot list this process's threads on this platform; the retention checks would be vacuous"
    );
    ids.into_iter().collect()
}

/// The threads started since `baseline` was taken that a root could own, and
/// separately the OS workqueue threads started since then, which it
/// cannot (see [`ThreadRecord::os_workqueue`]). Comparing thread IDs, not
/// counts, keeps threads the system starts or retires on its own from
/// passing for, or hiding, a thread a root kept.
fn threads_started_since(
    baseline: &std::collections::HashSet<u64>,
) -> (Vec<ThreadRecord>, Vec<ThreadRecord>) {
    assert!(
        !thread_ids().is_empty(),
        "the thread listing went empty during the measurement"
    );
    thread_inventory::records_except(baseline)
        .into_iter()
        .partition(|thread| !thread.os_workqueue)
}

/// Give detached teardown threads (watcher and LSP reapers) time to exit:
/// polls every 100 ms until no root-owned candidates remain or the existing
/// 10-second deadline passes. An unchanged set is not completion: detached
/// watcher joins can remain blocked for longer than five samples.
fn settled_threads_started_since(
    baseline: &std::collections::HashSet<u64>,
) -> (Vec<ThreadRecord>, Vec<ThreadRecord>) {
    settle_thread_inventory(
        || threads_started_since(baseline),
        || std::thread::sleep(Duration::from_millis(100)),
    )
}

fn settle_thread_inventory(
    mut sample: impl FnMut() -> (Vec<ThreadRecord>, Vec<ThreadRecord>),
    mut wait: impl FnMut(),
) -> (Vec<ThreadRecord>, Vec<ThreadRecord>) {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(10);
    let mut previous = sample();
    let mut pending = HashMap::new();
    let mut exit_times = Vec::new();
    loop {
        let current_ids: std::collections::HashSet<_> = previous.0.iter().map(|t| t.id).collect();
        for thread in &previous.0 {
            pending.insert(thread.id, thread.to_string());
        }
        pending.retain(|id, description| {
            if current_ids.contains(id) {
                true
            } else {
                let elapsed = started.elapsed().as_millis();
                println!("retention: exited after {elapsed} ms: {description}");
                exit_times.push(elapsed);
                false
            }
        });
        if previous.0.is_empty() || Instant::now() >= deadline {
            if !exit_times.is_empty() {
                exit_times.sort_unstable();
                println!(
                    "retention: observed_exits={} median_exit_ms={} max_exit_ms={}",
                    exit_times.len(),
                    (exit_times[(exit_times.len() - 1) / 2] + exit_times[exit_times.len() / 2]) / 2,
                    exit_times.last().unwrap(),
                );
            }
            return previous;
        }
        wait();
        previous = sample();
    }
}

#[test]
fn stable_thread_samples_are_not_teardown_completion() {
    let mut samples = 0;
    let (kept, _) = settle_thread_inventory(
        || {
            samples += 1;
            let threads = if samples <= 8 {
                vec![ThreadRecord {
                    id: 42,
                    name: Some("stable-set-probe".to_string()),
                    start: None,
                    os_workqueue: false,
                }]
            } else {
                Vec::new()
            };
            (threads, Vec::new())
        },
        || {},
    );
    assert!(kept.is_empty(), "a plateau was mistaken for completion");
    assert_eq!(samples, 9, "the census must observe the thread's exit");
}

/// A failure message line per thread.
fn describe_threads(threads: &[ThreadRecord]) -> String {
    threads
        .iter()
        .map(|thread| format!("\n  {thread}"))
        .collect()
}

/// Per-platform thread listing for the retention checks.
mod thread_inventory {
    use super::ThreadRecord;
    use std::collections::HashSet;

    #[cfg(target_os = "linux")]
    pub(super) fn ids() -> Vec<u64> {
        let Ok(entries) = std::fs::read_dir("/proc/self/task") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
            .collect()
    }

    #[cfg(target_os = "linux")]
    pub(super) fn records_except(baseline: &HashSet<u64>) -> Vec<ThreadRecord> {
        ids()
            .into_iter()
            .filter(|id| !baseline.contains(id))
            .map(|id| ThreadRecord {
                id,
                name: std::fs::read_to_string(format!("/proc/self/task/{id}/comm"))
                    .ok()
                    .map(|name| name.trim().to_string()),
                start: None,
                os_workqueue: false,
            })
            .collect()
    }

    #[cfg(windows)]
    pub(super) fn ids() -> Vec<u64> {
        crate::lifecycle_census::thread_ids_for_test().unwrap_or_default()
    }

    #[cfg(windows)]
    pub(super) fn records_except(baseline: &HashSet<u64>) -> Vec<ThreadRecord> {
        ids()
            .into_iter()
            .filter(|id| !baseline.contains(id))
            .map(|id| {
                let (start, os_workqueue) = win::describe_start(id as u32);
                ThreadRecord {
                    id,
                    name: win::thread_name(id as u32),
                    start,
                    os_workqueue,
                }
            })
            .collect()
    }

    #[cfg(windows)]
    mod win {
        use std::ffi::c_void;
        use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE, HMODULE};
        use windows_sys::Win32::System::Threading::{
            GetThreadDescription, OpenThread, THREAD_QUERY_INFORMATION,
        };

        const THREAD_QUERY_SET_WIN32_START_ADDRESS: i32 = 9;
        const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x00000002;
        const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x00000004;

        #[link(name = "ntdll")]
        extern "system" {
            fn NtQueryInformationThread(
                thread: HANDLE,
                information_class: i32,
                information: *mut c_void,
                information_length: u32,
                return_length: *mut u32,
            ) -> i32;
        }

        #[link(name = "kernel32")]
        extern "system" {
            fn GetModuleHandleExW(flags: u32, address: *const u16, module: *mut HMODULE) -> i32;
            fn GetModuleFileNameW(module: HMODULE, filename: *mut u16, size: u32) -> u32;
        }

        pub(super) fn thread_name(id: u32) -> Option<String> {
            // SAFETY: query a thread in this process and close the owned handle.
            let thread = unsafe { OpenThread(THREAD_QUERY_INFORMATION, 0, id) };
            if thread.is_null() {
                return None;
            }
            let mut name = std::ptr::null_mut();
            // SAFETY: name is writable; a successful call allocates a NUL-
            // terminated UTF-16 description, released with LocalFree below.
            let status = unsafe { GetThreadDescription(thread, &mut name) };
            unsafe { CloseHandle(thread) };
            if status < 0 || name.is_null() {
                return None;
            }
            let mut length = 0;
            // SAFETY: the successful call returned a NUL-terminated string.
            while unsafe { *name.add(length) } != 0 {
                length += 1;
            }
            let description =
                unsafe { String::from_utf16_lossy(std::slice::from_raw_parts(name, length)) };
            // SAFETY: GetThreadDescription allocated this buffer with LocalAlloc.
            unsafe { LocalFree(name.cast()) };
            (!description.is_empty()).then_some(description)
        }

        pub(super) fn describe_start(id: u32) -> (Option<String>, bool) {
            // SAFETY: query only a thread in this process; its handle is closed
            // after the query even if the thread exited since the snapshot.
            let thread = unsafe { OpenThread(THREAD_QUERY_INFORMATION, 0, id) };
            if thread.is_null() {
                return (None, false);
            }
            let mut start: *mut c_void = std::ptr::null_mut();
            // SAFETY: this information class writes one pointer into start.
            let status = unsafe {
                NtQueryInformationThread(
                    thread,
                    THREAD_QUERY_SET_WIN32_START_ADDRESS,
                    (&mut start as *mut *mut c_void).cast(),
                    std::mem::size_of_val(&start) as u32,
                    std::ptr::null_mut(),
                )
            };
            // SAFETY: OpenThread returned this owned handle.
            unsafe { CloseHandle(thread) };
            if status < 0 || start.is_null() {
                return (None, false);
            }

            let mut module: HMODULE = std::ptr::null_mut();
            // SAFETY: FROM_ADDRESS interprets start as an address, not a UTF-16
            // string. UNCHANGED_REFCOUNT borrows the already-loaded module.
            let found = unsafe {
                GetModuleHandleExW(
                    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                        | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                    start.cast(),
                    &mut module,
                )
            };
            if found == 0 {
                return (Some(format!("{start:p} (module unknown)")), false);
            }
            let mut path = vec![0u16; 32768];
            // SAFETY: path is writable for the supplied length; module is a
            // loaded module handle returned by GetModuleHandleExW.
            let length = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) }
                as usize;
            if length == 0 || length >= path.len() {
                return (Some(format!("{start:p} (module path unreadable)")), false);
            }
            let path = String::from_utf16_lossy(&path[..length]);
            let name = path.rsplit('\\').next().unwrap_or(&path);
            let description = format!("{name}+{:#x} ({start:p})", start as usize - module as usize);
            // Only the native OS pool entry module is excluded. Rust's Win32
            // thread start lives in the test executable, not in ntdll.dll.
            (Some(description), name.eq_ignore_ascii_case("ntdll.dll"))
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    pub(super) fn ids() -> Vec<u64> {
        Vec::new()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    pub(super) fn records_except(_baseline: &HashSet<u64>) -> Vec<ThreadRecord> {
        Vec::new()
    }

    #[cfg(target_os = "macos")]
    pub(super) use mach::{ids, records_except};

    #[cfg(target_os = "macos")]
    mod mach {
        use super::{HashSet, ThreadRecord};
        use std::ffi::{c_char, c_int, CStr};

        type Port = u32;
        type KernReturn = c_int;

        const THREAD_IDENTIFIER_INFO: u32 = 4;
        const THREAD_EXTENDED_INFO: u32 = 5;
        #[cfg(target_arch = "aarch64")]
        const THREAD_STATE_FLAVOR: c_int = 6; // ARM_THREAD_STATE64
        #[cfg(target_arch = "x86_64")]
        const THREAD_STATE_FLAVOR: c_int = 4; // x86_THREAD_STATE64
        /// Clears pointer-authentication bits from saved frame and return
        /// addresses; macOS user addresses fit in 47 bits.
        const ADDRESS_MASK: u64 = 0x0000_7fff_ffff_ffff;
        /// Frames walked per thread. A deeper stack reports no start routine.
        const MAX_FRAMES: usize = 512;
        /// Frames that only carry a thread into its work: the pthread and
        /// workqueue entry points and the Rust standard library's spawn path.
        const ENTRY_PREFIXES: &[&str] = &[
            "thread_start",
            "_pthread_start",
            "start_wqthread",
            "_pthread_wqthread",
            "std::",
            "core::",
            "alloc::",
            "<std::",
            "<core::",
            "<alloc::",
            "__rust",
        ];

        extern "C" {
            static mach_task_self_: Port;
            fn mach_thread_self() -> Port;
            fn task_threads(task: Port, threads: *mut *mut Port, count: *mut u32) -> KernReturn;
            fn thread_info(
                thread: Port,
                flavor: u32,
                info: *mut c_int,
                count: *mut u32,
            ) -> KernReturn;
            fn thread_get_state(
                thread: Port,
                flavor: c_int,
                state: *mut u32,
                count: *mut u32,
            ) -> KernReturn;
            fn thread_suspend(thread: Port) -> KernReturn;
            fn thread_resume(thread: Port) -> KernReturn;
            fn vm_read_overwrite(
                task: Port,
                address: usize,
                size: usize,
                data: usize,
                out_size: *mut usize,
            ) -> KernReturn;
            fn mach_port_deallocate(task: Port, name: Port) -> KernReturn;
            fn vm_deallocate(task: Port, address: usize, size: usize) -> KernReturn;
        }

        #[repr(C)]
        #[derive(Default)]
        struct IdentifierInfo {
            thread_id: u64,
            thread_handle: u64,
            _dispatch_qaddr: u64,
        }

        #[repr(C)]
        struct ExtendedInfo {
            _times: [u64; 2],
            _fields: [i32; 8],
            name: [c_char; 64],
        }

        #[cfg(target_arch = "aarch64")]
        #[repr(C)]
        #[derive(Default)]
        struct ThreadState {
            _x: [u64; 29],
            fp: u64,
            lr: u64,
            _sp: u64,
            pc: u64,
            _cpsr: u32,
            _flags: u32,
        }

        #[cfg(target_arch = "x86_64")]
        #[repr(C)]
        #[derive(Default)]
        struct ThreadState {
            _rax_to_rsi: [u64; 6],
            fp: u64,
            _rsp_to_r15: [u64; 9],
            pc: u64,
            _rflags_to_gs: [u64; 4],
        }

        fn task() -> Port {
            // SAFETY: the C runtime initializes this before main runs.
            unsafe { mach_task_self_ }
        }

        /// Send rights to every thread of this process; the caller releases
        /// each with [`release`].
        fn thread_ports() -> Vec<Port> {
            let mut list: *mut Port = std::ptr::null_mut();
            let mut count = 0u32;
            // SAFETY: both out-pointers are valid for writes.
            if unsafe { task_threads(task(), &mut list, &mut count) } != 0 || list.is_null() {
                return Vec::new();
            }
            // SAFETY: the kernel returned `count` ports at `list`.
            let ports = unsafe { std::slice::from_raw_parts(list, count as usize) }.to_vec();
            // SAFETY: `list` is the out-of-line array the kernel allocated.
            unsafe {
                vm_deallocate(
                    task(),
                    list as usize,
                    count as usize * std::mem::size_of::<Port>(),
                );
            }
            ports
        }

        fn release(port: Port) {
            // SAFETY: `port` is a send right this module received.
            unsafe {
                mach_port_deallocate(task(), port);
            }
        }

        /// The thread's kernel ID and its thread-local storage base. The
        /// kernel sets the storage base before the thread first runs code in
        /// this process, so it is zero only for a thread that never has.
        fn identity(port: Port) -> Option<(u64, u64)> {
            let mut info = IdentifierInfo::default();
            let mut count = (std::mem::size_of::<IdentifierInfo>() / 4) as u32;
            // SAFETY: `info` is writable storage of `count` 32-bit words.
            let result = unsafe {
                thread_info(
                    port,
                    THREAD_IDENTIFIER_INFO,
                    (&mut info as *mut IdentifierInfo).cast(),
                    &mut count,
                )
            };
            (result == 0).then_some((info.thread_id, info.thread_handle))
        }

        fn thread_name(port: Port) -> Option<String> {
            // SAFETY: the record is plain C data.
            let mut info: ExtendedInfo = unsafe { std::mem::zeroed() };
            let mut count = (std::mem::size_of::<ExtendedInfo>() / 4) as u32;
            // SAFETY: `info` is writable storage of `count` 32-bit words.
            let result = unsafe {
                thread_info(
                    port,
                    THREAD_EXTENDED_INFO,
                    (&mut info as *mut ExtendedInfo).cast(),
                    &mut count,
                )
            };
            if result != 0 {
                return None;
            }
            info.name[63] = 0;
            // SAFETY: the buffer is NUL-terminated just above.
            let name = unsafe { CStr::from_ptr(info.name.as_ptr()) }.to_string_lossy();
            (!name.is_empty()).then(|| name.into_owned())
        }

        pub(in super::super) fn ids() -> Vec<u64> {
            thread_ports()
                .into_iter()
                .filter_map(|port| {
                    let id = identity(port).map(|(id, _)| id);
                    release(port);
                    id
                })
                .collect()
        }

        pub(in super::super) fn records_except(baseline: &HashSet<u64>) -> Vec<ThreadRecord> {
            // SAFETY: plain Mach call; the returned right is released below.
            let current = unsafe { mach_thread_self() };
            let mut records = Vec::new();
            for port in thread_ports() {
                if let Some((id, tls_base)) =
                    identity(port).filter(|(id, _)| !baseline.contains(id))
                {
                    let (start, os_workqueue) = if port == current {
                        (Some("<the measuring thread>".to_string()), false)
                    } else {
                        describe_start(tls_base, stack(port))
                    };
                    records.push(ThreadRecord {
                        id,
                        name: thread_name(port),
                        start,
                        os_workqueue,
                    });
                }
                release(port);
            }
            release(current);
            records
        }

        /// Reads one saved frame record (caller's frame pointer, return
        /// address). The kernel copies it, so a stale pointer fails the read
        /// instead of faulting.
        fn read_frame(address: u64) -> Option<[u64; 2]> {
            let mut frame = [0u64; 2];
            let mut read = 0usize;
            // SAFETY: `frame` is 16 writable bytes; the kernel checks `address`.
            let result = unsafe {
                vm_read_overwrite(
                    task(),
                    address as usize,
                    16,
                    frame.as_mut_ptr() as usize,
                    &mut read,
                )
            };
            (result == 0 && read == 16).then_some(frame)
        }

        /// The registers of a suspended thread that locate its stack.
        struct Registers {
            pc: u64,
            fp: u64,
            /// The link register on arm64, which holds the running
            /// function's return address until that function saves it in its
            /// frame record. x86_64 keeps the return address on the stack
            /// instead, so it has none.
            lr: Option<u64>,
        }

        /// A thread's saved registers and return addresses.
        struct Stack {
            /// `None` when the thread could not be suspended or its registers
            /// could not be read.
            registers: Option<Registers>,
            /// Return addresses, innermost first.
            returns: Vec<u64>,
        }

        /// Reads the thread's registers and walks its frame pointer chain
        /// (Apple's ABIs always keep frame pointers). The thread is suspended
        /// for the walk so its stack holds still; nothing while it is
        /// suspended allocates or locks, because it may hold the allocator's
        /// lock.
        fn stack(port: Port) -> Stack {
            let mut frames = [0u64; MAX_FRAMES];
            let mut depth = 0;
            // SAFETY: suspends another thread of this process; resumed below.
            if unsafe { thread_suspend(port) } != 0 {
                return Stack {
                    registers: None,
                    returns: Vec::new(),
                };
            }
            let mut state = ThreadState::default();
            let mut count = (std::mem::size_of::<ThreadState>() / 4) as u32;
            // SAFETY: `state` is writable storage of `count` 32-bit words.
            let got_state = unsafe {
                thread_get_state(
                    port,
                    THREAD_STATE_FLAVOR,
                    (&mut state as *mut ThreadState).cast(),
                    &mut count,
                )
            } == 0;
            let mut fp = state.fp & ADDRESS_MASK;
            while got_state && depth < MAX_FRAMES && fp != 0 && fp % 8 == 0 {
                let Some([next, ret]) = read_frame(fp) else {
                    break;
                };
                let ret = ret & ADDRESS_MASK;
                if ret == 0 {
                    break;
                }
                frames[depth] = ret;
                depth += 1;
                let next = next & ADDRESS_MASK;
                // Callers' frames sit at higher addresses; anything else is
                // the end of the chain or a corrupt record.
                if next <= fp {
                    break;
                }
                fp = next;
            }
            // SAFETY: balances the suspend above.
            unsafe {
                thread_resume(port);
            }
            #[cfg(target_arch = "aarch64")]
            let lr = Some(state.lr & ADDRESS_MASK);
            #[cfg(target_arch = "x86_64")]
            let lr = None;
            Stack {
                registers: got_state.then_some(Registers {
                    pc: state.pc & ADDRESS_MASK,
                    fp: state.fp & ADDRESS_MASK,
                    lr,
                }),
                returns: frames[..depth].to_vec(),
            }
        }

        /// What `dladdr` knows about `address`; `None` outside every image.
        fn lookup(address: u64) -> Option<libc::Dl_info> {
            // SAFETY: Dl_info is plain C data that dladdr fills.
            let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
            // SAFETY: dladdr only reads loader tables for the address.
            (unsafe { libc::dladdr(address as *const libc::c_void, &mut info) } != 0)
                .then_some(info)
        }

        /// The demangled name of the symbol recorded in `info`, if `dladdr`
        /// found one.
        fn symbol_name(info: &libc::Dl_info) -> Option<String> {
            if info.dli_sname.is_null() {
                return None;
            }
            // SAFETY: dladdr returned a NUL-terminated symbol name.
            let name = unsafe { CStr::from_ptr(info.dli_sname) }.to_string_lossy();
            // `{:#}` leaves out the hash suffix; other names pass through.
            Some(format!("{:#}", rustc_demangle::demangle(&name)))
        }

        /// The symbol containing `address`, demangled, or the image and
        /// offset when the image exports no symbol there. A return address
        /// points after its call, so it is looked up one byte back.
        fn symbol(address: u64, return_address: bool) -> String {
            let address = address.saturating_sub(u64::from(return_address));
            let Some(info) = lookup(address) else {
                return format!("{address:#x}");
            };
            if let Some(name) = symbol_name(&info) {
                return name;
            }
            let image = if info.dli_fname.is_null() {
                "?".to_string()
            } else {
                // SAFETY: dladdr returned a NUL-terminated path.
                let path = unsafe { CStr::from_ptr(info.dli_fname) }.to_string_lossy();
                path.rsplit('/').next().unwrap_or_default().to_string()
            };
            format!("{image}+{:#x}", address - info.dli_fbase as u64)
        }

        fn is_workqueue_entry(name: &str) -> bool {
            name == "start_wqthread" || name == "_pthread_wqthread"
        }

        fn is_entry_frame(name: &str) -> bool {
            ENTRY_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
        }

        /// The functions the registers name that the saved frames cannot:
        /// the running function (from the program counter), then on arm64
        /// the function the link register returns into.
        ///
        /// The frame pointer chain yields return addresses: once the running
        /// function has saved its frame record, the chain names its caller
        /// and everything outside it, but never the running function itself. A thread caught executing its spawn
        /// closure's own code has no saved frame naming the closure; neither
        /// has one caught entering or leaving a callee (before the callee
        /// saves its frame record, or after it restores it), because the
        /// frame pointer still points at the closure's record, which names
        /// only the closure's caller. In the second case the closure is where
        /// the link register returns to.
        ///
        /// The program counter always lies in code. The link register can be
        /// reused as scratch once its function has saved it, so it is used
        /// only when it falls in a named symbol, and only after the program
        /// counter.
        fn running_functions(registers: &Registers) -> Vec<String> {
            let mut names = vec![symbol(registers.pc, false)];
            if let Some(lr) = registers.lr.filter(|&lr| lr != 0) {
                if let Some(name) = lookup(lr - 1).as_ref().and_then(symbol_name) {
                    names.push(name);
                }
            }
            names
        }

        /// How a thread started, from its stack: the two outermost frames and
        /// the first function past the entry and spawn machinery, which for a
        /// Rust thread is the closure it was spawned with. Also whether it is
        /// a kernel workqueue thread.
        ///
        /// That function is searched for in the saved frames from the
        /// outermost in, then in the functions the registers name (see
        /// [`running_functions`]). When none qualifies, only the two
        /// outermost frames are shown.
        ///
        /// The kernel creates workqueue threads ahead of demand and parks
        /// them before they ever run: such a thread has all-zero registers
        /// and no thread-local storage. `pthread_create` (which `std::thread`,
        /// rayon and tokio use) sets a new thread's registers and storage
        /// before it is scheduled, so no AFT thread looks like that.
        fn describe_start(tls_base: u64, stack: Stack) -> (Option<String>, bool) {
            let Some(registers) = stack.registers else {
                return (Some("<stack not readable>".to_string()), false);
            };
            if stack.returns.is_empty() {
                if registers.pc == 0 && registers.fp == 0 && tls_base == 0 {
                    return (
                        Some("<kernel workqueue thread that has not run yet>".to_string()),
                        true,
                    );
                }
                let name = symbol(registers.pc, false);
                let os_workqueue = is_workqueue_entry(&name);
                return (Some(format!("{name} (no saved frames)")), os_workqueue);
            }
            let outer_first: Vec<String> = stack
                .returns
                .iter()
                .rev()
                .map(|&f| symbol(f, true))
                .collect();
            let os_workqueue = outer_first
                .iter()
                .take(2)
                .any(|name| is_workqueue_entry(name));
            let mut chain: Vec<&str> = outer_first.iter().take(2).map(String::as_str).collect();
            let running = running_functions(&registers);
            if let Some(routine) = outer_first
                .iter()
                .chain(&running)
                .find(|name| !is_entry_frame(name))
            {
                if !chain.contains(&routine.as_str()) {
                    chain.push(routine);
                }
            }
            (Some(chain.join(" > ")), os_workqueue)
        }

        /// A thread caught where no saved frame names its spawn closure (see
        /// [`running_functions`]) must still be reported with the closure,
        /// not with the entry and runtime frames alone. The stack here is
        /// built by hand so that case is certain: every saved frame is a
        /// standard-library function, as the frames outside a Rust spawn
        /// closure are.
        #[test]
        fn start_routine_comes_from_the_registers_when_no_saved_frame_names_it() {
            #[inline(never)]
            fn spawned_work_marker() {
                std::hint::black_box(());
            }
            let marker = spawned_work_marker as fn() as usize as u64;
            let runtime_function = std::thread::yield_now as fn() as usize as u64;
            let outer_runtime_function = std::thread::park as fn() as usize as u64;
            // Return addresses, innermost first: each points one byte into
            // its function, as `symbol` expects after looking one byte back.
            let runtime_returns = vec![runtime_function + 1, outer_runtime_function + 1];

            // Running the closure's own code: only the program counter
            // names it.
            let (start, os_workqueue) = describe_start(
                1,
                Stack {
                    registers: Some(Registers {
                        pc: marker,
                        fp: 1,
                        lr: None,
                    }),
                    returns: runtime_returns.clone(),
                },
            );
            let start = start.unwrap_or_default();
            assert!(start.contains("spawned_work_marker"), "{start}");
            assert!(start.starts_with("std::"), "{start}");
            assert!(!os_workqueue, "{start}");

            // This synthetic register state models an arm64 thread entering
            // a runtime callee before the callee saved its frame record: the
            // program counter is in the callee, and only the link register,
            // not yet saved, returns into the closure.
            let (start, _) = describe_start(
                1,
                Stack {
                    registers: Some(Registers {
                        pc: runtime_function,
                        fp: 1,
                        lr: Some(marker + 4),
                    }),
                    returns: runtime_returns.clone(),
                },
            );
            let start = start.unwrap_or_default();
            assert!(start.contains("spawned_work_marker"), "{start}");

            // A saved frame that names the closure wins over the running
            // function, which is whatever the closure called into last.
            let callee = libc::getpid as unsafe extern "C" fn() -> libc::pid_t as usize as u64;
            let (start, _) = describe_start(
                1,
                Stack {
                    registers: Some(Registers {
                        pc: callee,
                        fp: 1,
                        lr: None,
                    }),
                    returns: vec![marker + 4, outer_runtime_function + 1],
                },
            );
            let start = start.unwrap_or_default();
            assert!(start.contains("spawned_work_marker"), "{start}");
            assert!(!start.contains("getpid"), "{start}");
        }
    }
}

/// The kernel ID of the calling thread, as the thread inventory reports it.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
fn current_thread_id() -> u64 {
    #[cfg(windows)]
    {
        // SAFETY: GetCurrentThreadId has no preconditions.
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() as u64 }
    }
    #[cfg(target_os = "linux")]
    {
        // SAFETY: gettid has no preconditions.
        unsafe { libc::gettid() as u64 }
    }
    #[cfg(target_os = "macos")]
    {
        let mut id = 0u64;
        // SAFETY: a null thread means the calling thread; `id` is writable.
        unsafe { libc::pthread_threadid_np(0 as libc::pthread_t, &mut id) };
        id
    }
}

/// The retention checks name the threads a root kept. A thread started
/// after the baseline must be listed with its ID and, on macOS or Windows,
/// its start routine or module, even when it has no name; and it must not
/// be mistaken for a system workqueue thread, which the checks leave out.
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[test]
fn thread_inventory_lists_a_new_unnamed_thread_with_its_start_routine() {
    let baseline = thread_ids();
    let (id_tx, id_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let probe = std::thread::spawn(move || {
        id_tx.send(current_thread_id()).unwrap();
        let _ = stop_rx.recv();
    });
    let probe_id = id_rx.recv().unwrap();
    let (owned, os_workqueue) = threads_started_since(&baseline);
    stop_tx.send(()).unwrap();
    probe.join().unwrap();

    let record = owned
        .iter()
        .find(|thread| thread.id == probe_id)
        .unwrap_or_else(|| {
            panic!(
                "the probe thread {probe_id} is missing from the threads started since the baseline:{}\nworkqueue:{}",
                describe_threads(&owned),
                describe_threads(&os_workqueue)
            )
        });
    let line = record.to_string();
    assert!(line.contains(&probe_id.to_string()), "{line}");
    #[cfg(windows)]
    {
        let executable = std::env::current_exe().unwrap();
        let module = executable.file_name().unwrap().to_string_lossy();
        let start = record.start.as_deref().unwrap_or_default();
        assert!(start.starts_with(module.as_ref()), "{line}");
    }
    #[cfg(target_os = "macos")]
    {
        assert_eq!(record.name, None, "{line}");
        let start = record.start.as_deref().unwrap_or_default();
        assert!(
            start.contains(
                "idle_root_retention_tests::thread_inventory_lists_a_new_unnamed_thread_with_its_start_routine"
            ),
            "the start routine must name the spawning closure: {line}"
        );
    }
}

/// The macOS check leaves out kernel workqueue threads, so it must recognise
/// a real one: a libdispatch worker held busy here must be reported as one,
/// and its start must show the workqueue entry point.
#[cfg(target_os = "macos")]
#[test]
fn thread_inventory_recognises_a_libdispatch_worker() {
    extern "C" {
        fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut libc::c_void;
        fn dispatch_async_f(
            queue: *mut libc::c_void,
            context: *mut libc::c_void,
            work: extern "C" fn(*mut libc::c_void),
        );
    }
    type Handoff = (std::sync::mpsc::Sender<u64>, std::sync::mpsc::Receiver<()>);
    extern "C" fn hold_worker(context: *mut libc::c_void) {
        // SAFETY: the test passed a leaked Box<Handoff> and gives it up here.
        let (id_tx, stop_rx) = *unsafe { Box::from_raw(context.cast::<Handoff>()) };
        id_tx.send(current_thread_id()).unwrap();
        let _ = stop_rx.recv();
    }

    let (id_tx, id_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let handoff: Box<Handoff> = Box::new((id_tx, stop_rx));
    // SAFETY: the default-priority global queue always exists; the worker
    // takes ownership of the context.
    unsafe {
        dispatch_async_f(
            dispatch_get_global_queue(0, 0),
            Box::into_raw(handoff).cast(),
            hold_worker,
        );
    }
    let worker_id = id_rx.recv().unwrap();
    // The pool may have started this worker before the test did, so list it
    // by leaving it out of the baseline rather than relying on its age.
    let mut baseline = thread_ids();
    baseline.remove(&worker_id);
    let (owned, os_workqueue) = threads_started_since(&baseline);
    stop_tx.send(()).unwrap();

    assert!(
        !owned.iter().any(|thread| thread.id == worker_id),
        "the libdispatch worker {worker_id} was counted as a thread a root could own:{}",
        describe_threads(&owned)
    );
    let worker = os_workqueue
        .iter()
        .find(|thread| thread.id == worker_id)
        .unwrap_or_else(|| {
            panic!("the libdispatch worker {worker_id} is missing from the listing")
        });
    let start = worker.start.as_deref().unwrap_or_default();
    assert!(start.starts_with("start_wqthread"), "{worker}");
}

/// Windows owns and may park native pool workers after the operation that
/// requested them has finished. A real pool callback must be classified by
/// its start module, without excluding Rust threads in the executable.
#[cfg(windows)]
#[test]
fn thread_inventory_recognises_a_windows_pool_worker() {
    use windows_sys::Win32::System::Threading::{QueueUserWorkItem, WT_EXECUTEDEFAULT};

    type Handoff = (std::sync::mpsc::Sender<u64>, std::sync::mpsc::Receiver<()>);
    unsafe extern "system" fn hold_worker(context: *mut std::ffi::c_void) -> u32 {
        // SAFETY: QueueUserWorkItem received ownership of this Box<Handoff>.
        let (id_tx, stop_rx) = *unsafe { Box::from_raw(context.cast::<Handoff>()) };
        id_tx.send(current_thread_id()).unwrap();
        let _ = stop_rx.recv();
        0
    }

    let (id_tx, id_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let handoff: Box<Handoff> = Box::new((id_tx, stop_rx));
    let context = Box::into_raw(handoff);
    // SAFETY: the callback owns context until it returns; stop_tx keeps it
    // blocked while the inventory reads its thread's start address.
    let queued = unsafe { QueueUserWorkItem(Some(hold_worker), context.cast(), WT_EXECUTEDEFAULT) };
    if queued == 0 {
        // SAFETY: a failed submission did not transfer ownership.
        unsafe { drop(Box::from_raw(context)) };
        panic!(
            "QueueUserWorkItem failed: {}",
            std::io::Error::last_os_error()
        );
    }
    let worker_id = id_rx.recv().unwrap();
    // An existing worker is just as useful as one started for this callback.
    let mut baseline = thread_ids();
    baseline.remove(&worker_id);
    let (owned, os_workqueue) = threads_started_since(&baseline);
    stop_tx.send(()).unwrap();

    assert!(
        !owned.iter().any(|thread| thread.id == worker_id),
        "the Windows pool worker {worker_id} was counted as a thread a root could own:{}",
        describe_threads(&owned)
    );
    let worker = os_workqueue
        .iter()
        .find(|thread| thread.id == worker_id)
        .unwrap_or_else(|| {
            panic!("the Windows pool worker {worker_id} is missing from the listing")
        });
    let start = worker.start.as_deref().unwrap_or_default();
    assert!(
        start.to_ascii_lowercase().starts_with("ntdll.dll+"),
        "{worker}"
    );
    println!("Windows pool callback: {worker}");
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn heap_in_use() -> Option<u64> {
    // SAFETY: mallinfo2 only reads allocator statistics.
    Some(unsafe { libc::mallinfo2() }.uordblks as u64)
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn heap_in_use() -> Option<u64> {
    None
}

/// Give detached teardown threads (watcher and LSP reapers) time to exit:
/// polls the thread count every 100 ms and returns once it is at most
/// `at_most`, has not changed for five samples, or 10 seconds have passed.
/// A settling heuristic, not proof that teardown finished.
fn settled_thread_total(at_most: u64) -> u64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut previous = thread_total();
    let mut stable_samples = 0;
    loop {
        if previous <= at_most || stable_samples >= 5 || Instant::now() >= deadline {
            return previous;
        }
        std::thread::sleep(Duration::from_millis(100));
        let total = thread_total();
        stable_samples = if total == previous {
            stable_samples + 1
        } else {
            0
        };
        previous = total;
    }
}

#[test]
fn evicted_idle_roots_release_their_per_context_threads() {
    if std::env::var_os(CHILD_ENV).is_none() {
        run_in_child(
            "subc::idle_root_retention_tests::evicted_idle_roots_release_their_per_context_threads",
        );
        return;
    }

    let app = App::default_shared();
    let executor = Arc::new(Executor::new());
    let storage = tempfile::tempdir().unwrap();
    // Warm process-wide state (pools, registries) with one root that is
    // created and dropped, so the baseline holds only process-lifetime work.
    // The filesystem helper pool starts workers on demand, up to its
    // configured size, and keeps them; fill it before the baseline so its
    // growth cannot read as threads the roots kept.
    crate::bounded_io::fill_helper_pool_for_test();
    {
        let (_dir, root) = test_support::test_root("retention-warmup");
        let ctx = Arc::new(AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(root.as_path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        ));
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        executor.remove_actor(&root);
    }
    std::thread::sleep(Duration::from_millis(200));
    let baseline_threads = thread_total();
    let baseline_thread_ids = thread_ids();
    let baseline_heap = heap_in_use();

    let mut dirs = Vec::new();
    let mut weak_contexts: Vec<Weak<AppContext>> = Vec::new();
    let mut live_roots = HashMap::new();
    for index in 0..ROOTS {
        let (dir, root) = test_support::test_root(&format!("retention-{index}"));
        let ctx = Arc::new(AppContext::from_app(
            Arc::clone(&app),
            Config {
                project_root: Some(root.as_path().to_path_buf()),
                storage_dir: Some(storage.path().to_path_buf()),
                ..Config::default()
            },
        ));
        assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
        weak_contexts.push(Arc::downgrade(&ctx));
        drop(ctx);
        live_roots.insert(root, RootMeta::new(Instant::now()));
        dirs.push(dir);
    }
    let bound_threads = thread_total();

    // Every route closed: quiesce each root, age it past its TTL and reap.
    let roots: Vec<_> = live_roots.keys().cloned().collect();
    for root in &roots {
        quiesce_unbound_root(root, &mut live_roots, &executor);
        live_roots.get_mut(root).unwrap().last_touched =
            Instant::now() - IDLE_ROOT_TTL - Duration::from_secs(1);
    }
    let outcome = reap_idle_roots(
        Instant::now(),
        &mut live_roots,
        &HashMap::new(),
        &HashMap::new(),
        &executor,
        &DispatchPathMetrics::new(),
    );
    assert_eq!(outcome.evicted, ROOTS, "every idle root is evicted");

    let (kept_threads, os_workqueue_threads) = settled_threads_started_since(&baseline_thread_ids);
    let evicted_threads = thread_total();
    let evicted_heap = heap_in_use();
    let live_contexts = weak_contexts
        .iter()
        .filter(|ctx| ctx.strong_count() > 0)
        .count();
    println!(
        "retention: roots={ROOTS} baseline_threads={baseline_threads} bound_threads={bound_threads} \
         evicted_threads={evicted_threads} kept_threads={} os_workqueue_threads={} \
         live_contexts={live_contexts} heap_baseline={baseline_heap:?} heap_evicted={evicted_heap:?}{}{}",
        kept_threads.len(),
        os_workqueue_threads.len(),
        describe_threads(&kept_threads),
        describe_threads(&os_workqueue_threads),
    );
    assert!(
        kept_threads.len() <= 1,
        "idle-evicted roots kept {} thread(s) started after the baseline for {ROOTS} roots:{}\n\
         (not counted: {} OS workqueue thread(s), which no root owns:{})",
        kept_threads.len(),
        describe_threads(&kept_threads),
        os_workqueue_threads.len(),
        describe_threads(&os_workqueue_threads),
    );
}

const WORKED_ROOTS: usize = 6;
const SOURCE_FILES: usize = 40;
const SESSION: &str = "retention-session";

fn request(command: &str, params: serde_json::Value) -> crate::protocol::RawRequest {
    crate::protocol::RawRequest {
        id: format!("{command}-retention"),
        command: command.to_string(),
        lsp_hints: None,
        session_id: Some(SESSION.to_string()),
        params,
    }
}

/// A Git checkout with enough Rust source that its search index, callgraph
/// and symbol cache hold real entries.
fn worked_fixture(name: &str) -> (tempfile::TempDir, ProjectRootId) {
    let (dir, root) = test_support::test_root(name);
    for index in 0..SOURCE_FILES {
        let mut source = String::new();
        for function in 0..20 {
            source.push_str(&format!(
                "/// Function {function} of module {index}.\npub fn item_{index}_{function}(value: u64) -> u64 {{\n    let doubled = value * 2;\n    helper_{index}(doubled) + {function}\n}}\n\n"
            ));
        }
        source.push_str(&format!(
            "fn helper_{index}(value: u64) -> u64 {{\n    value + 1\n}}\n"
        ));
        std::fs::write(dir.path().join(format!("module_{index}.rs")), source).unwrap();
    }
    // A legacy project config location: the project watcher never covers it,
    // so the root keeps a config file watch of its own.
    std::fs::create_dir_all(dir.path().join(".opencode")).unwrap();
    std::fs::write(dir.path().join(".opencode/aft.jsonc"), "{}\n").unwrap();
    for args in [
        &["init", "--quiet"][..],
        &["add", "."][..],
        &[
            "-c",
            "user.name=AFT Tests",
            "-c",
            "user.email=aft-tests@example.com",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ][..],
    ] {
        let mut command = std::process::Command::new("git");
        crate::test_env::apply_hermetic_git_env(command.current_dir(dir.path()));
        assert!(command.args(args).status().unwrap().success());
    }
    (dir, root)
}

/// Configure the root, wait for its index builds, then do an agent's worth of
/// reads, outlines, searches and writes in one session.
fn work_root(ctx: &AppContext, root: &Path, storage: &Path) {
    let user_config = storage.join(format!(
        "user-{}.jsonc",
        root.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(
        &user_config,
        serde_json::json!({
            "indexes": {"trigram": true, "semantic": false, "callgraph": true}
        })
        .to_string(),
    )
    .unwrap();
    let configure = request(
        "configure",
        serde_json::json!({
            "project_root": root,
            "harness": "opencode",
            "storage_dir": storage,
            "cortexkit_user_config_path": user_config,
        }),
    );
    {
        let _git_env = crate::test_env::hermetic_git_env_guard();
        let response = crate::commands::configure::handle_configure(&configure, ctx);
        assert!(response.success, "configure failed: {response:?}");
    }
    crate::commands::configure::drain_deferred_configure_maintenance(ctx);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        crate::runtime_drain::drain_search_index_events(ctx);
        crate::runtime_drain::drain_callgraph_store_events(ctx);
        crate::runtime_drain::drain_semantic_index_events(ctx);
        if ctx.search_index_rx().read().unwrap().is_none()
            && ctx.callgraph_store_rx().lock().is_none()
            && ctx.semantic_index_rx().lock().is_none()
        {
            break;
        }
        assert!(Instant::now() < deadline, "index builds did not settle");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        ctx.search_index().read().unwrap().is_some(),
        "the fixture must load a search index"
    );

    for index in 0..10 {
        let file = root.join(format!("module_{index}.rs"));
        let read = crate::commands::read::handle_read(
            &request("read", serde_json::json!({"file": file})),
            ctx,
        );
        assert!(read.success, "read failed: {read:?}");
        let outline = crate::commands::outline::handle_outline(
            &request("outline", serde_json::json!({"file": file})),
            ctx,
        );
        assert!(outline.success, "outline failed: {outline:?}");
    }
    for pattern in ["helper_3", "doubled", "item_7_1"] {
        let grep = crate::commands::grep::handle_grep(
            &request(
                "grep",
                serde_json::json!({"pattern": pattern, "max_results": 50}),
            ),
            ctx,
        );
        assert!(grep.success, "grep failed: {grep:?}");
    }
    for index in 0..3 {
        let file = root.join(format!("module_{index}.rs"));
        let mut content = std::fs::read_to_string(&file).unwrap();
        content.push_str("\npub fn appended() {}\n");
        let write = crate::commands::write::handle_write(
            &request(
                "write",
                serde_json::json!({"file": file, "content": content}),
            ),
            ctx,
        );
        assert!(write.success, "write failed: {write:?}");
    }
}

/// Give detached teardown work (LSP and watcher reapers, semantic lane
/// release) time to free its memory: samples allocator usage every 100 ms
/// and returns once two readings differ by less than 16 KiB, or after 50
/// samples. A settling heuristic; `None` where allocator statistics are
/// unavailable.
fn settled_heap() -> Option<u64> {
    let mut previous = heap_in_use()?;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        let current = heap_in_use()?;
        if current.abs_diff(previous) < 16 * 1024 {
            return Some(current);
        }
        previous = current;
    }
    Some(previous)
}

#[test]
fn evicted_worked_roots_keep_only_bookkeeping() {
    if std::env::var_os(CHILD_ENV).is_none() {
        run_in_child("subc::idle_root_retention_tests::evicted_worked_roots_keep_only_bookkeeping");
        return;
    }
    // The roots below use a legacy project config file, which the project
    // watcher never covers, so each root starts its own config file watch.
    crate::config_live::enable_config_watches_for_test();

    let app = App::default_shared();
    let executor = Arc::new(Executor::new());
    let storage = tempfile::tempdir().unwrap();
    let metrics = DispatchPathMetrics::new();
    let mut dirs = Vec::new();
    let mut live_roots = HashMap::new();

    // Bind, work, unbind, age and reap `roots`, returning weak references to
    // their contexts.
    let mut cycle = |names: std::ops::Range<usize>,
                     live_roots: &mut HashMap<ProjectRootId, RootMeta>|
     -> Vec<(ProjectRootId, Weak<AppContext>)> {
        let mut bound = Vec::new();
        for index in names {
            let (dir, root) = worked_fixture(&format!("retention-worked-{index}"));
            let ctx = Arc::new(AppContext::from_app(
                Arc::clone(&app),
                Config {
                    project_root: Some(root.as_path().to_path_buf()),
                    storage_dir: Some(storage.path().to_path_buf()),
                    ..Config::default()
                },
            ));
            assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
            work_root(&ctx, root.as_path(), storage.path());
            // Configure's deferred tail normally starts this watch once the
            // project watcher is up. Sync here as well so the fixture does not
            // depend on when that tail runs.
            crate::config_live::sync_config_watches(&ctx);
            assert!(
                ctx.config_live().has_project_fallback_watch(),
                "the legacy project config never got its own watch: {:?}",
                ctx.config_live()
                    .sources()
                    .map(|sources| sources.project.path)
            );
            let mut semantic = ctx.config().semantic.clone();
            semantic.backend = crate::config::SemanticBackend::OpenAiCompatible;
            semantic.base_url = Some("http://127.0.0.1:9".to_string());
            *ctx.semantic_embedding_model().lock() = Some(
                crate::semantic_index::EmbeddingModel::from_config(&semantic)
                    .expect("query embedding model"),
            );
            bound.push((root.clone(), Arc::downgrade(&ctx)));
            live_roots.insert(root, RootMeta::new(Instant::now()));
            dirs.push(dir);
        }
        for (root, _) in &bound {
            quiesce_unbound_root(root, live_roots, &executor);
            live_roots.get_mut(root).unwrap().last_touched =
                Instant::now() - IDLE_ROOT_TTL - Duration::from_secs(1);
        }
        // Writes leave unpersisted search deltas; the first sweeps queue
        // their teardown persist, and later sweeps evict once it has run.
        let deadline = Instant::now() + Duration::from_secs(60);
        while bound
            .iter()
            .any(|(root, _)| !live_roots[root].idle_artifacts_evicted)
        {
            assert!(Instant::now() < deadline, "worked roots were not evicted");
            reap_idle_roots(
                Instant::now(),
                live_roots,
                &HashMap::new(),
                &HashMap::new(),
                &executor,
                &metrics,
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        bound
    };

    // One root goes through the whole cycle first and is then retired, so
    // process-wide state (grammars, SQLite, pools) is warm before the
    // baseline. The filesystem helper pool starts workers on demand, up to
    // its configured size, and how many the warm-up root needed depends on
    // load; fill it before the baseline so later growth cannot read as
    // threads the roots kept.
    crate::cache_freshness::warm_strict_verify_pool_for_test();
    crate::bounded_io::fill_helper_pool_for_test();
    for (root, _) in cycle(0..1, &mut live_roots) {
        executor.remove_actor(&root);
        live_roots.remove(&root);
    }
    let baseline_heap = settled_heap();
    let baseline_threads = settled_thread_total(0);
    let baseline_thread_ids = thread_ids();

    let evicted = cycle(1..WORKED_ROOTS + 1, &mut live_roots);
    // The reaper hands the reopenable resources (LSP, database pools, config
    // watches, embedding model, compiled output filters) to a background
    // release; wait for it.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let released = evicted.iter().all(|(_, ctx)| {
            let ctx = ctx.upgrade().expect("an evicted root keeps its context");
            let watches = ctx.config_live();
            !watches.has_project_fallback_watch()
                && !watches.has_user_watch()
                && ctx.semantic_embedding_model().lock().is_none()
                && !ctx.filter_registry_loaded()
        });
        if released {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "evicted roots kept their config watches, embedding model or output filters"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    for (_, ctx) in &evicted {
        let ctx = ctx.upgrade().unwrap();
        assert!(ctx.search_index().read().unwrap().is_none());
        assert!(ctx.semantic_index().read().unwrap().is_none());
        assert!(ctx.callgraph_store().read().unwrap().is_none());
        assert_eq!(ctx.symbol_cache().read().unwrap().len(), 0);
        assert!(!ctx.watcher_runtime_active());
    }
    let (kept_threads, os_workqueue_threads) = settled_threads_started_since(&baseline_thread_ids);
    let evicted_heap = settled_heap();

    // Drop the evicted contexts: the heap that leaves with them is what each
    // evicted root was still holding.
    for (root, _) in &evicted {
        executor.remove_actor(root);
    }
    let dropped_heap = settled_heap();
    assert!(
        evicted.iter().all(|(_, ctx)| ctx.strong_count() == 0),
        "removed actors must drop their contexts"
    );
    let per_root = match (evicted_heap, dropped_heap) {
        (Some(evicted), Some(dropped)) => {
            Some(evicted.saturating_sub(dropped) / WORKED_ROOTS as u64)
        }
        _ => None,
    };
    println!(
        "retention: worked_roots={WORKED_ROOTS} baseline_threads={baseline_threads} \
         kept_threads={} os_workqueue_threads={} heap_baseline={baseline_heap:?} \
         heap_evicted={evicted_heap:?} heap_dropped={dropped_heap:?} \
         retained_per_evicted_root={per_root:?}{}{}",
        kept_threads.len(),
        os_workqueue_threads.len(),
        describe_threads(&kept_threads),
        describe_threads(&os_workqueue_threads),
    );
    assert!(
        kept_threads.len() <= 1,
        "{WORKED_ROOTS} evicted roots kept {} thread(s) started after the baseline:{}\n\
         (not counted: {} OS workqueue thread(s), which no root owns:{})",
        kept_threads.len(),
        describe_threads(&kept_threads),
        os_workqueue_threads.len(),
        describe_threads(&os_workqueue_threads),
    );
    match per_root {
        Some(per_root) => assert!(
            per_root < 256 * 1024,
            "an evicted root still holds {per_root} heap bytes"
        ),
        // Only glibc exposes the allocator statistics this check reads.
        None => println!(
            "retention: SKIPPED the per-root heap check: no allocator statistics on this platform"
        ),
    }
}

/// Make output that the builtin `make` filter rewrites: its directory lines
/// are stripped.
#[cfg(unix)]
const MAKE_OUTPUT: &str =
    "make: Entering directory '/work'\ncc -o app main.c\nmake: Leaving directory '/work'\n";

/// A root's background task output must compress the same before and after
/// the root is evicted for idleness. A running task keeps the root from
/// being evicted at all; once it has finished, the root can be evicted, and
/// a later render of the finished task's output still goes through the
/// root's filters.
#[cfg(unix)]
#[test]
fn background_task_output_compresses_the_same_after_idle_eviction() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = ProjectRootId::from_path(project.path()).unwrap();
    let mut config = Config {
        project_root: Some(root.as_path().to_path_buf()),
        storage_dir: Some(storage.path().to_path_buf()),
        experimental_bash_background: true,
        experimental_bash_compress: true,
        ..Config::default()
    };
    config.sandbox.profile.head = crate::config::SandboxMode::Off;
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    install_bash_compressor(&ctx);
    // Configure compiles the filters eagerly.
    ctx.reset_filter_registry();
    let compress = |ctx: &AppContext| {
        ctx.bash_background()
            .compress_output("make", MAKE_OUTPUT.to_string(), Some(0))
            .text
    };
    let before = compress(&ctx);
    assert_ne!(
        before, MAKE_OUTPUT,
        "the make filter must change this output"
    );

    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
    let launched = crate::bash_background::spawn(
        "req-idle-eviction",
        SESSION,
        "sleep 1",
        crate::bash_background::BashShell::Bash,
        PathBuf::from("/bin/sh"),
        None,
        None,
        crate::bash_background::HardKill::After(Duration::from_secs(60)),
        &ctx,
        true,
        false,
        false,
        false,
        24,
        80,
        Vec::new(),
        false,
        None,
        None,
    );
    assert!(launched.success, "launch failed: {:?}", launched.data);
    let task_id = launched.data["task_id"].as_str().unwrap().to_string();

    let mut live_roots = HashMap::from([(root.clone(), RootMeta::new(Instant::now()))]);
    quiesce_unbound_root(&root, &mut live_roots, &executor);
    live_roots.get_mut(&root).unwrap().last_touched =
        Instant::now() - IDLE_ROOT_TTL - Duration::from_secs(1);
    let metrics = DispatchPathMetrics::new();
    let reap = |live_roots: &mut HashMap<ProjectRootId, RootMeta>| {
        reap_idle_roots(
            Instant::now(),
            live_roots,
            &HashMap::new(),
            &HashMap::new(),
            &executor,
            &metrics,
        )
        .evicted
    };
    assert_eq!(reap(&mut live_roots), 0, "a running task holds the root");
    assert!(ctx.filter_registry_loaded());

    // Let the task finish, then evict.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let snapshot = ctx
            .bash_background()
            .observed_status(&task_id, SESSION, 4096)
            .expect("task is registered");
        if snapshot.info.status.is_terminal()
            && ctx.bash_background().try_has_running_tasks() == Some(false)
        {
            break;
        }
        assert!(Instant::now() < deadline, "the task never finished");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The background release takes the embedding model in the same step
    // that decides about the filters, so its absence marks the release done.
    let mut semantic = ctx.config().semantic.clone();
    semantic.backend = crate::config::SemanticBackend::OpenAiCompatible;
    semantic.base_url = Some("http://127.0.0.1:9".to_string());
    *ctx.semantic_embedding_model().lock() = Some(
        crate::semantic_index::EmbeddingModel::from_config(&semantic).expect("embedding model"),
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !live_roots[&root].idle_artifacts_evicted {
        assert!(
            Instant::now() < deadline,
            "the finished root was not evicted"
        );
        reap(&mut live_roots);
        std::thread::sleep(Duration::from_millis(20));
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while ctx.semantic_embedding_model().lock().is_some() {
        assert!(Instant::now() < deadline, "the idle release never ran");
        std::thread::sleep(Duration::from_millis(20));
    }

    assert_eq!(
        compress(&ctx),
        before,
        "the finished task's output must compress as it did before eviction"
    );
}

/// A root with background tasks enabled and the output filters compiled, as
/// configure leaves it.
#[cfg(unix)]
fn background_task_root(project: &Path, storage: &Path) -> (ProjectRootId, Arc<AppContext>) {
    let root = ProjectRootId::from_path(project).unwrap();
    let mut config = Config {
        project_root: Some(root.as_path().to_path_buf()),
        storage_dir: Some(storage.to_path_buf()),
        experimental_bash_background: true,
        experimental_bash_compress: true,
        ..Config::default()
    };
    config.sandbox.profile.head = crate::config::SandboxMode::Off;
    let ctx = Arc::new(AppContext::new(
        Box::new(crate::parser::TreeSitterProvider::new()),
        config,
    ));
    install_bash_compressor(&ctx);
    ctx.reset_filter_registry();
    (root, ctx)
}

/// Run `command` as a background task and wait until it has finished and
/// left the watchdog.
#[cfg(unix)]
fn run_background_task(ctx: &AppContext, command: &str) -> String {
    let launched = crate::bash_background::spawn(
        "req-idle-eviction",
        SESSION,
        command,
        crate::bash_background::BashShell::Bash,
        PathBuf::from("/bin/sh"),
        None,
        None,
        crate::bash_background::HardKill::After(Duration::from_secs(60)),
        ctx,
        true,
        false,
        false,
        false,
        24,
        80,
        Vec::new(),
        false,
        None,
        None,
    );
    assert!(launched.success, "launch failed: {:?}", launched.data);
    let task_id = launched.data["task_id"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let snapshot = ctx
            .bash_background()
            .observed_status(&task_id, SESSION, 4096)
            .expect("task is registered");
        if snapshot.info.status.is_terminal()
            && ctx.bash_background().try_has_running_tasks() == Some(false)
        {
            return task_id;
        }
        assert!(Instant::now() < deadline, "the task never finished");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Give the root a cached query embedding model. The idle release takes it in
/// the same step that decides about the filters, so its absence marks that a
/// release has run.
#[cfg(unix)]
fn install_release_probe(ctx: &AppContext) {
    let mut semantic = ctx.config().semantic.clone();
    semantic.backend = crate::config::SemanticBackend::OpenAiCompatible;
    semantic.base_url = Some("http://127.0.0.1:9".to_string());
    *ctx.semantic_embedding_model().lock() = Some(
        crate::semantic_index::EmbeddingModel::from_config(&semantic).expect("embedding model"),
    );
}

/// An evicted root that kept its filters for a finished task's record
/// releases them once cleanup has removed the last record, although the
/// reaper does not evict an evicted root again.
#[cfg(unix)]
#[test]
fn deferred_filter_release_runs_after_the_last_task_record_is_gone() {
    let project = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let (root, ctx) = background_task_root(project.path(), storage.path());
    let executor = Arc::new(Executor::new());
    assert!(executor.register_actor(root.clone(), Arc::clone(&ctx)));
    let task_id = run_background_task(&ctx, "true");

    let mut live_roots = HashMap::from([(root.clone(), RootMeta::new(Instant::now()))]);
    quiesce_unbound_root(&root, &mut live_roots, &executor);
    live_roots.get_mut(&root).unwrap().last_touched =
        Instant::now() - IDLE_ROOT_TTL - Duration::from_secs(1);
    let metrics = DispatchPathMetrics::new();
    let sweep_until = |live_roots: &mut HashMap<ProjectRootId, RootMeta>,
                       done: &dyn Fn(&HashMap<ProjectRootId, RootMeta>) -> bool,
                       what: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done(live_roots) {
            assert!(Instant::now() < deadline, "{what}");
            reap_idle_roots(
                Instant::now(),
                live_roots,
                &HashMap::new(),
                &HashMap::new(),
                &executor,
                &metrics,
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };

    install_release_probe(&ctx);
    sweep_until(
        &mut live_roots,
        &|live_roots| {
            live_roots[&root].idle_artifacts_evicted
                && ctx.semantic_embedding_model().lock().is_none()
        },
        "the root was not evicted and released",
    );
    assert!(
        ctx.filter_registry_loaded(),
        "a root holding a task record keeps its filters"
    );

    ctx.bash_background().forget_task_record_for_test(&task_id);
    install_release_probe(&ctx);
    // Wait on the filters themselves: the release clears the embedding slot
    // before it takes the filter registry, so the slot alone can read as done
    // while the filters are still loaded.
    sweep_until(
        &mut live_roots,
        &|_| !ctx.filter_registry_loaded(),
        "the deferred release never took the filters after the last task record went away",
    );
    assert!(
        ctx.semantic_embedding_model().lock().is_none(),
        "the deferred release must also release the embedding model"
    );
}
