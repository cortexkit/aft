use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const DISK_LIMIT_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone)]
pub struct BgBuffer {
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    rotated: bool,
}

impl BgBuffer {
    pub fn new(stdout_path: PathBuf, stderr_path: PathBuf) -> Self {
        Self {
            stdout_path,
            stderr_path,
            rotated: false,
        }
    }

    pub fn stdout_path(&self) -> &Path {
        &self.stdout_path
    }

    pub fn stderr_path(&self) -> &Path {
        &self.stderr_path
    }

    pub fn read_tail(&self, max_bytes: usize) -> (String, bool) {
        let stdout = read_file_tail(&self.stdout_path, max_bytes);
        let stderr = read_file_tail(&self.stderr_path, max_bytes);
        match (stdout, stderr) {
            (Ok((stdout, stdout_truncated)), Ok((stderr, stderr_truncated))) => {
                let mut output = Vec::with_capacity(stdout.len().saturating_add(stderr.len()));
                output.extend_from_slice(&stdout);
                output.extend_from_slice(&stderr);
                if output.len() > max_bytes {
                    let keep_from = output.len().saturating_sub(max_bytes);
                    output.drain(..keep_from);
                }
                (
                    String::from_utf8_lossy(&output).into_owned(),
                    self.rotated
                        || stdout_truncated
                        || stderr_truncated
                        || output.len() >= max_bytes && (stdout.len() + stderr.len()) > max_bytes,
                )
            }
            (Ok((stdout, stdout_truncated)), Err(_)) => (
                String::from_utf8_lossy(&stdout).into_owned(),
                self.rotated || stdout_truncated,
            ),
            (Err(_), Ok((stderr, stderr_truncated))) => (
                String::from_utf8_lossy(&stderr).into_owned(),
                self.rotated || stderr_truncated,
            ),
            (Err(_), Err(_)) => (String::new(), self.rotated),
        }
    }

    pub fn read_for_token_count(&self, max_bytes_per_stream: usize) -> TokenCountInput {
        // Read up to `max_bytes_per_stream` bytes per stream rather than
        // refusing to tokenize anything when the file exceeds the cap.
        // `read_file_with_cap` returns `Ok(None)` for files over the cap,
        // which would mask large outputs from compression accounting
        // entirely — defeating the purpose of token tracking for the
        // tasks that benefit most from compression (huge logs, test
        // output, build noise). The tokenizer benchmark in
        // `crates/aft-tokenizer` shows ~7ms at 128KiB and scales
        // linearly, so reading the tail (most recent output) is safe
        // even for very large spills.
        let stdout = read_file_tail(&self.stdout_path, max_bytes_per_stream);
        let stderr = read_file_tail(&self.stderr_path, max_bytes_per_stream);
        match (stdout, stderr) {
            (Ok((stdout, _)), Ok((stderr, _))) => TokenCountInput::Text(combine_streams(
                String::from_utf8_lossy(&stdout).as_ref(),
                String::from_utf8_lossy(&stderr).as_ref(),
            )),
            // If either file is missing/unreadable, fall back to whatever
            // we could read. Truly missing both = skip (rare).
            (Ok((stdout, _)), Err(_)) => TokenCountInput::Text(combine_streams(
                String::from_utf8_lossy(&stdout).as_ref(),
                "",
            )),
            (Err(_), Ok((stderr, _))) => TokenCountInput::Text(combine_streams(
                "",
                String::from_utf8_lossy(&stderr).as_ref(),
            )),
            (Err(_), Err(_)) => TokenCountInput::Skipped,
        }
    }

    pub fn read_stream_tail(&self, stream: StreamKind, max_bytes: usize) -> (String, bool) {
        let path = match stream {
            StreamKind::Stdout => &self.stdout_path,
            StreamKind::Stderr => &self.stderr_path,
        };
        match read_file_tail(path, max_bytes) {
            Ok((bytes, truncated)) => (
                String::from_utf8_lossy(&bytes).into_owned(),
                self.rotated || truncated,
            ),
            Err(_) => (String::new(), self.rotated),
        }
    }

    /// Path to the stdout spill file (alias of `stdout_path` for backward compat).
    pub fn output_path(&self) -> Option<PathBuf> {
        Some(self.stdout_path.clone())
    }

    // stderr_path() already exists above returning &Path — no duplicate needed.

    pub fn enforce_terminal_cap(&mut self) {
        if truncate_front(&self.stdout_path, DISK_LIMIT_BYTES).unwrap_or(false) {
            self.rotated = true;
        }
        if truncate_front(&self.stderr_path, DISK_LIMIT_BYTES).unwrap_or(false) {
            self.rotated = true;
        }
    }

    pub fn cleanup(&self) {
        let _ = fs::remove_file(&self.stdout_path);
        let _ = fs::remove_file(&self.stderr_path);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenCountInput {
    Text(String),
    Skipped,
}

pub fn combine_streams(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_string(),
        (true, false) => stderr.to_string(),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

fn read_file_tail(path: &Path, max_bytes: usize) -> io::Result<(Vec<u8>, bool)> {
    if max_bytes == 0 {
        return Ok((
            Vec::new(),
            path.metadata()
                .map(|metadata| metadata.len() > 0)
                .unwrap_or(false),
        ));
    }

    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let read_len = len.min(max_bytes as u64);
    if read_len > 0 {
        file.seek(SeekFrom::End(-(read_len as i64)))?;
    }
    let mut bytes = Vec::with_capacity(read_len as usize);
    file.read_to_end(&mut bytes)?;
    Ok((bytes, len > max_bytes as u64))
}

fn truncate_front(path: &Path, retain_bytes: u64) -> io::Result<bool> {
    let len = match path.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if len <= retain_bytes {
        return Ok(false);
    }

    let mut file = File::open(path)?;
    file.seek(SeekFrom::End(-(retain_bytes as i64)))?;
    let mut tail = Vec::with_capacity(retain_bytes as usize);
    file.read_to_end(&mut tail)?;
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("out")
    ));
    fs::write(&tmp, tail)?;
    fs::rename(&tmp, path)?;
    Ok(true)
}
