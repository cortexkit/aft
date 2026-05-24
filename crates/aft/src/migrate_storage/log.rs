use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::Harness;

pub(super) struct JsonLogger {
    file: Option<File>,
    harness: Harness,
    warned: bool,
}

impl JsonLogger {
    pub(super) fn open(path: &Path, harness: Harness) -> Self {
        let mut warned = false;
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).is_err() {
                eprintln!("log write failed; continuing migration");
                warned = true;
            }
        }
        let file = if warned {
            None
        } else {
            match OpenOptions::new().create(true).append(true).open(path) {
                Ok(file) => Some(file),
                Err(_) => {
                    if !warned {
                        eprintln!("log write failed; continuing migration");
                    }
                    warned = true;
                    None
                }
            }
        };
        Self {
            file,
            harness,
            warned,
        }
    }

    pub(super) fn write(&mut self, mut value: serde_json::Value) {
        if let serde_json::Value::Object(ref mut map) = value {
            map.insert("ts".to_string(), serde_json::json!(iso_timestamp_now()));
            map.entry("harness".to_string())
                .or_insert_with(|| serde_json::json!(self.harness.as_str()));
        }
        if let Some(file) = self.file.as_mut() {
            let result = serde_json::to_writer(&mut *file, &value)
                .map_err(io::Error::other)
                .and_then(|()| file.write_all(b"\n"));
            if result.is_err() {
                self.file = None;
                self.warn_once();
            }
        }
    }

    fn warn_once(&mut self) {
        if !self.warned {
            eprintln!("log write failed; continuing migration");
            self.warned = true;
        }
    }
}

pub(super) fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

pub(super) fn iso_timestamp_now() -> String {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    format_unix_millis(duration.as_secs() as i64, duration.subsec_millis())
}

fn format_unix_millis(seconds: i64, millis: u32) -> String {
    let days = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_epoch_formats_as_iso_utc() {
        assert_eq!(format_unix_millis(0, 0), "1970-01-01T00:00:00.000Z");
    }
}
