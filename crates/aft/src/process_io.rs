//! Process-wide I/O counters and snapshot payloads.
//!
//! Exposes cumulative disk and logical I/O counters measured from kernel
//! facilities (`proc_pid_rusage` on Darwin, `/proc/self/io` on Linux).

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Bytes {
    pub written: u64,
    pub logical: u64,
    pub read: u64,
}

impl Bytes {
    pub fn capture() -> Option<Self> {
        #[cfg(target_os = "macos")]
        {
            let mut usage = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
            // A successful kernel call initializes the versioned buffer.
            let rc = unsafe {
                libc::proc_pid_rusage(
                    libc::getpid(),
                    libc::RUSAGE_INFO_V4,
                    usage.as_mut_ptr().cast(),
                )
            };
            if rc != 0 {
                return None;
            }
            let usage = unsafe { usage.assume_init() };
            Some(Self {
                written: usage.ri_diskio_byteswritten,
                logical: usage.ri_logical_writes,
                read: usage.ri_diskio_bytesread,
            })
        }
        #[cfg(target_os = "linux")]
        {
            read_proc_self_io()
        }
        #[cfg(target_os = "windows")]
        {
            // On Windows, process I/O counters can be queried via `GetProcessIoCounters`
            // (IO_COUNTERS: ReadOperationCount, WriteOperationCount, ReadTransferCount, WriteTransferCount).
            // Until wired, Windows returns None (`available: false`).
            None
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            None
        }
    }

    pub fn delta(self, before: Self) -> Option<Self> {
        Some(Self {
            written: self.written.checked_sub(before.written)?,
            logical: self.logical.checked_sub(before.logical)?,
            read: self.read.checked_sub(before.read)?,
        })
    }

    pub fn add(self, other: Self) -> Self {
        Self {
            written: self.written + other.written,
            logical: self.logical + other.logical,
            read: self.read + other.read,
        }
    }

    pub fn fields(value: Option<Self>, prefix: &str) -> String {
        match value {
            Some(v) => format!(
                "{prefix}_physical_bytes_written={} {prefix}_logical_bytes_written={} {prefix}_bytes_read={}",
                v.written, v.logical, v.read
            ),
            None => format!(
                "{prefix}_physical_bytes_written=unknown {prefix}_logical_bytes_written=unknown {prefix}_bytes_read=unknown"
            ),
        }
    }
}

#[cfg(target_os = "linux")]
fn read_proc_self_io() -> Option<Bytes> {
    let content = std::fs::read_to_string("/proc/self/io").ok()?;
    parse_proc_self_io(&content)
}

/// Parse `/proc/[pid]/io` format into process I/O counters:
/// - `read_bytes` -> diskio_bytes_read (`read`)
/// - `write_bytes` -> diskio_bytes_written (`written`)
/// - `wchar` -> logical_bytes_written (`logical`)
pub fn parse_proc_self_io(content: &str) -> Option<Bytes> {
    let mut read_bytes = None;
    let mut write_bytes = None;
    let mut wchar = None;

    for line in content.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim().parse::<u64>().ok();
            match key {
                "read_bytes" => read_bytes = value,
                "write_bytes" => write_bytes = value,
                "wchar" => wchar = value,
                _ => {}
            }
        }
    }

    Some(Bytes {
        read: read_bytes?,
        written: write_bytes?,
        logical: wchar?,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProcessIoSnapshot {
    pub available: bool,
    pub sampled_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diskio_bytes_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diskio_bytes_written: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logical_bytes_written: Option<u64>,
}

impl ProcessIoSnapshot {
    pub fn from_sample(sample: Option<Bytes>, sampled_at_ms: u64) -> Self {
        match sample {
            Some(bytes) => Self {
                available: true,
                sampled_at_ms,
                diskio_bytes_read: Some(bytes.read),
                diskio_bytes_written: Some(bytes.written),
                logical_bytes_written: Some(bytes.logical),
            },
            None => Self {
                available: false,
                sampled_at_ms,
                diskio_bytes_read: None,
                diskio_bytes_written: None,
                logical_bytes_written: None,
            },
        }
    }

    pub fn capture() -> Self {
        let sampled_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0);
        Self::from_sample(Bytes::capture(), sampled_at_ms)
    }

    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("process_io serializes")
    }
}

#[cfg(test)]
#[cfg(target_os = "macos")]
pub fn assert_darwin_logical_bytes_observe_file_write() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let before = Bytes::capture().unwrap();
    let mut file = std::fs::File::create(dir.path().join("bytes")).unwrap();
    file.write_all(&vec![0x5a; 8 * 1024 * 1024]).unwrap();
    file.sync_all().unwrap();
    let delta = Bytes::capture().unwrap().delta(before).unwrap();
    assert!(
        delta.logical >= 8 * 1024 * 1024,
        "observed logical delta: {delta:?}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn process_io_snapshot_with_sample_includes_all_fields() {
        let sample = Some(Bytes {
            read: 1024,
            written: 2048,
            logical: 4096,
        });
        let snapshot = ProcessIoSnapshot::from_sample(sample, 42_000);
        let value = snapshot.to_value();
        assert_eq!(value["available"], Value::Bool(true));
        assert_eq!(value["sampled_at_ms"], Value::from(42_000u64));
        assert_eq!(value["diskio_bytes_read"], Value::from(1024u64));
        assert_eq!(value["diskio_bytes_written"], Value::from(2048u64));
        assert_eq!(value["logical_bytes_written"], Value::from(4096u64));
    }

    #[test]
    fn process_io_snapshot_unavailable_omits_numbers() {
        let snapshot = ProcessIoSnapshot::from_sample(None, 42_000);
        let value = snapshot.to_value();
        assert_eq!(value["available"], Value::Bool(false));
        assert_eq!(value["sampled_at_ms"], Value::from(42_000u64));
        assert!(
            value.get("diskio_bytes_read").is_none(),
            "diskio_bytes_read must be omitted when unavailable: {value}"
        );
        assert!(
            value.get("diskio_bytes_written").is_none(),
            "diskio_bytes_written must be omitted when unavailable: {value}"
        );
        assert!(
            value.get("logical_bytes_written").is_none(),
            "logical_bytes_written must be omitted when unavailable: {value}"
        );
    }

    #[test]
    fn linux_proc_self_io_parser_maps_counters() {
        let sample = "rchar: 123456\n\
                      wchar: 987654\n\
                      syscr: 10\n\
                      syscw: 20\n\
                      read_bytes: 111111\n\
                      write_bytes: 222222\n\
                      cancelled_write_bytes: 0\n";
        let bytes = parse_proc_self_io(sample).expect("parse proc self io");
        assert_eq!(bytes.read, 111111);
        assert_eq!(bytes.written, 222222);
        assert_eq!(bytes.logical, 987654);
    }

    #[test]
    fn linux_proc_self_io_parser_incomplete_returns_none() {
        let sample = "rchar: 123456\n\
                      wchar: 987654\n\
                      syscr: 10\n\
                      syscw: 20\n\
                      read_bytes: 111111\n";
        assert!(parse_proc_self_io(sample).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_logical_bytes_observe_file_write() {
        assert_darwin_logical_bytes_observe_file_write();
    }
}
