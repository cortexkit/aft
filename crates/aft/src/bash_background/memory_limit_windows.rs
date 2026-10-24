use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::JobObjects::*;
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE};

// WinNT.h message code; windows-sys exposes the job APIs but not this macro.
const JOB_OBJECT_MSG_JOB_MEMORY_LIMIT: u32 = 10;
use windows_sys::Win32::System::IO::{CreateIoCompletionPort, GetQueuedCompletionStatus};

pub(super) struct Job {
    handle: HANDLE,
    port: HANDLE,
    limit: usize,
}

// Handles are owned here and accessed only under the registry's monitor mutex.
unsafe impl Send for Job {}

impl Job {
    pub fn new(mib: u64, task_id: &str) -> Result<Self, String> {
        unsafe {
            let name: Vec<u16> = format!("Local\\AFT-memory-{task_id}\0")
                .encode_utf16()
                .collect();
            let handle = CreateJobObjectW(std::ptr::null(), name.as_ptr());
            let existed = GetLastError() == ERROR_ALREADY_EXISTS;
            if handle.is_null() {
                return Err(error());
            }
            let port = CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1);
            let limit = usize::try_from(mib * 1024 * 1024)
                .map_err(|_| "memory cap exceeds platform address space")?;
            let job = Self {
                handle,
                port,
                limit,
            };
            if port.is_null() {
                return Err(error());
            }
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_JOB_MEMORY;
            limits.JobMemoryLimit = limit;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            ) == 0
            {
                return Err(error());
            }
            let association = JOBOBJECT_ASSOCIATE_COMPLETION_PORT {
                CompletionKey: 1_usize as *mut _,
                CompletionPort: port,
            };
            if !existed
                && SetInformationJobObject(
                    handle,
                    JobObjectAssociateCompletionPortInformation,
                    (&association as *const JOBOBJECT_ASSOCIATE_COMPLETION_PORT).cast(),
                    std::mem::size_of_val(&association) as u32,
                ) == 0
            {
                return Err(error());
            }
            Ok(job)
        }
    }

    pub fn attach(&self, pid: u32) -> Result<(), String> {
        unsafe {
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() {
                return Err(error());
            }
            let assigned = AssignProcessToJobObject(self.handle, process);
            CloseHandle(process);
            if assigned == 0 {
                return Err(error());
            }
        }
        Ok(())
    }

    pub fn exceeded(&self) -> bool {
        // Limit the notification drain so a fork-heavy command cannot monopolize a tick.
        for _ in 0..64 {
            let mut message = 0;
            let mut key = 0;
            let mut overlapped = std::ptr::null_mut();
            if unsafe {
                GetQueuedCompletionStatus(self.port, &mut message, &mut key, &mut overlapped, 0)
            } == 0
            {
                break;
            }
            if message == JOB_OBJECT_MSG_JOB_MEMORY_LIMIT {
                return true;
            }
        }
        // Job-memory notifications are best effort. Peak accounting remains queryable
        // while this handle is held, even after the last process in the job exits.
        self.peak().is_some_and(|peak| peak >= self.limit)
    }

    fn peak(&self) -> Option<usize> {
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            QueryInformationJobObject(
                self.handle,
                JobObjectExtendedLimitInformation,
                (&mut limits as *mut JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
                std::ptr::null_mut(),
            )
        };
        (ok != 0).then_some(limits.PeakJobMemoryUsed)
    }

    pub fn outcome_note(&self) -> Option<&'static str> {
        self.peak().is_none_or(|peak| peak < self.limit).then_some(
            "Windows job memory accounting did not confirm whether the memory cap was hit.",
        )
    }

    pub fn terminate(&self) {
        unsafe {
            TerminateJobObject(self.handle, 137);
        }
    }
}

fn error() -> String {
    format!(
        "memory cap job-object setup failed: {}",
        std::io::Error::last_os_error()
    )
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.port);
            CloseHandle(self.handle);
        }
    }
}
