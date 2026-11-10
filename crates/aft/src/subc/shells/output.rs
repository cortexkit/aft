//! Raw-file byte accounting, separate from masking and terminal presentation.
use super::wire::{OutputFrame, BACKFILL_BYTES};
use std::ops::Range;

#[derive(Clone, Copy, Debug)]
enum Escape {
    Start,
    Csi,
    Osc,
    OscEsc,
    String,
    StringEsc,
    Intermediate,
}

impl Escape {
    fn byte(self, byte: u8) -> Option<Self> {
        Some(match (self, byte) {
            (Self::Start, b'[') => Self::Csi,
            (Self::Start, b']') => Self::Osc,
            (Self::Start, b'P' | b'X' | b'^' | b'_') => Self::String,
            (Self::Start | Self::Intermediate, 0x20..=0x2f) => Self::Intermediate,
            (Self::Start | Self::Intermediate, _) => return None,
            (Self::Csi, 0x40..=0x7e) => return None,
            (Self::Csi, _) => Self::Csi,
            (Self::Osc | Self::OscEsc, 7) => return None,
            (Self::OscEsc | Self::StringEsc, b'\\') => return None,
            (Self::Osc | Self::OscEsc, 27) => Self::OscEsc,
            (Self::String | Self::StringEsc, 27) => Self::StringEsc,
            (Self::Osc | Self::OscEsc, _) => Self::Osc,
            (Self::String | Self::StringEsc, _) => Self::String,
        })
    }
}

struct Plan {
    consumed: usize,
    shown: Vec<Range<usize>>,
    dropping: Option<Escape>,
}

pub(super) struct Chunker {
    offset: u64,
    pending: Vec<u8>,
    max: usize,
    keep: bool,
    dropping: Option<Escape>,
}

impl Chunker {
    pub(super) fn new(offset: u64, max: usize, keep: bool) -> Self {
        Self {
            offset,
            pending: Vec::new(),
            max,
            keep,
            dropping: None,
        }
    }

    pub(super) fn offset(&self) -> u64 {
        self.offset
    }
    pub(super) fn read_offset(&self) -> u64 {
        self.offset + self.pending.len() as u64
    }
    pub(super) fn append(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }

    pub(super) fn resume_escape(&mut self, prefix: &[u8]) {
        let mut escape = None;
        for byte in prefix {
            if let Some(state) = escape {
                escape = Escape::byte(state, *byte);
            } else if *byte == 27 {
                escape = Some(Escape::Start);
            }
        }
        self.dropping = escape;
    }

    /// The mask callback reads surrounding bytes from the task's own file. Holding
    /// back the longest exact secret prevents a growing value from leaking
    /// before the halo contains its complete spelling.
    pub(super) fn next<E>(
        &mut self,
        stream: &str,
        terminal: bool,
        longest_secret: usize,
        mut mask: impl FnMut(u64, u64) -> Result<String, E>,
    ) -> Result<Option<OutputFrame>, E> {
        let available = if terminal {
            self.pending.len()
        } else {
            self.pending.len().saturating_sub(longest_secret)
        };
        let mut budget = self.max;
        loop {
            let plan = self.plan(available, terminal, budget);
            if plan.consumed == 0 {
                return Ok(None);
            }
            let mut text = String::new();
            for range in &plan.shown {
                text.push_str(&mask(
                    self.offset + range.start as u64,
                    self.offset + range.end as u64,
                )?);
            }
            if text.len() > self.max && budget > 1 {
                budget = (budget / 2).max(1);
                continue;
            }
            // A single UTF-8 character or masking placeholder fits even the
            // smallest admitted chunk. Mask expansion is accounted for above.
            debug_assert!(text.len() <= self.max);
            let offset = self.offset;
            self.offset += plan.consumed as u64;
            self.pending.drain(..plan.consumed);
            self.dropping = plan.dropping;
            return Ok(Some(OutputFrame::Chunk {
                stream: stream.into(),
                offset,
                end_offset: self.offset,
                text,
            }));
        }
    }

    fn plan(&self, available: usize, terminal: bool, budget: usize) -> Plan {
        let bytes = &self.pending[..available];
        let mut at = 0;
        let mut shown = Vec::<Range<usize>>::new();
        let mut visible = 0;
        let mut dropping = self.dropping;
        while at < bytes.len() {
            if let Some(mut escape) = dropping {
                while at < bytes.len() {
                    let next = escape.byte(bytes[at]);
                    at += 1;
                    match next {
                        Some(next) => escape = next,
                        None => {
                            dropping = None;
                            break;
                        }
                    }
                }
                if dropping.is_some() {
                    dropping = Some(escape);
                }
                continue;
            }
            let start = at;
            let (end, display) = if bytes[at] == 27 {
                let mut escape = Some(Escape::Start);
                let mut end = at + 1;
                while end < bytes.len() && escape.is_some() {
                    escape = escape.unwrap().byte(bytes[end]);
                    end += 1;
                }
                if escape.is_some() && end - start <= self.max && !terminal {
                    break;
                }
                let display = self.keep && escape.is_none() && end - start <= self.max;
                if escape.is_some() && !terminal {
                    dropping = escape;
                }
                (end, display)
            } else {
                let width = match bytes[at] {
                    0xc2..=0xdf => 2,
                    0xe0..=0xef => 3,
                    0xf0..=0xf4 => 4,
                    _ => 1,
                };
                if at + width > bytes.len() && !terminal {
                    break;
                }
                let end = (at + width).min(bytes.len());
                let end = if std::str::from_utf8(&bytes[at..end]).is_ok() {
                    end
                } else {
                    at + 1
                };
                (end, true)
            };
            if display {
                if visible > 0 && visible + end - start > budget {
                    break;
                }
                visible += end - start;
                if let Some(last) = shown.last_mut().filter(|last| last.end == start) {
                    last.end = end;
                } else {
                    shown.push(start..end);
                }
            }
            at = end;
            if visible >= budget {
                break;
            }
        }
        Plan {
            consumed: at,
            shown,
            dropping,
        }
    }
}

pub(super) fn backfill_start(length: u64, from: Option<u64>) -> u64 {
    from.unwrap_or_else(|| length.saturating_sub(BACKFILL_BYTES))
        .min(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunks(bytes: &[u8], keep: bool, max: usize) -> Vec<OutputFrame> {
        let mut chunker = Chunker::new(0, max, keep);
        chunker.append(bytes);
        let mut frames = Vec::new();
        while let Some(frame) = chunker
            .next("stdout", true, 0, |from, to| {
                Ok::<_, ()>(
                    String::from_utf8_lossy(&bytes[from as usize..to as usize]).into_owned(),
                )
            })
            .unwrap()
        {
            frames.push(frame);
        }
        frames
    }

    fn text(frames: &[OutputFrame]) -> String {
        frames
            .iter()
            .map(|frame| match frame {
                OutputFrame::Chunk { text, .. } => text.as_str(),
                _ => panic!(),
            })
            .collect()
    }

    #[test]
    fn shells_v1_output_utf8_escape_edges_and_exact_resume() {
        let raw = format!("{}€\x1b[32mgreen\x1b[0m", "x".repeat(61));
        let frames = chunks(raw.as_bytes(), true, 64);
        assert_eq!(text(&frames), raw);
        let OutputFrame::Chunk {
            offset,
            end_offset,
            text: first,
            ..
        } = &frames[0]
        else {
            panic!()
        };
        assert_eq!((*offset, *end_offset), (0, 64));
        assert!(first.ends_with('€'));
        let mut expected = 0;
        for frame in &frames {
            let OutputFrame::Chunk {
                offset,
                end_offset,
                text,
                ..
            } = frame
            else {
                panic!()
            };
            assert_eq!(*offset, expected);
            assert!(text.len() <= 64);
            expected = *end_offset;
        }
        assert_eq!(expected, raw.len() as u64);
        let resumed = chunks(&raw.as_bytes()[64..], true, 64);
        assert_eq!(format!("{first}{}", text(&resumed)), raw);
        assert_eq!(
            text(&chunks(raw.as_bytes(), false, 64)),
            format!("{}€green", "x".repeat(61))
        );
    }

    #[test]
    fn shells_v1_output_oversized_complete_osc_is_dropped_without_losing_offsets() {
        let raw = format!("before\x1b]0;{}\x07after", "x".repeat(100));
        let frames = chunks(raw.as_bytes(), true, 64);
        assert_eq!(text(&frames), "beforeafter");
        assert!(
            matches!(frames.last().unwrap(), OutputFrame::Chunk { end_offset, .. } if *end_offset == raw.len() as u64)
        );
    }

    #[test]
    fn shells_v1_output_unterminated_osc_closed_late_never_exposes_escape_bytes() {
        let raw = format!("\x1b]0;{}\x07after", "s".repeat(100));
        let mut chunker = Chunker::new(0, 64, true);
        chunker.append(&raw.as_bytes()[..70]);
        let first = chunker
            .next("stdout", false, 0, |_, _| {
                Ok::<_, ()>("must not mask dropped bytes".into())
            })
            .unwrap()
            .unwrap();
        assert!(
            matches!(first, OutputFrame::Chunk { offset:0, end_offset:70, ref text, .. } if text.is_empty())
        );
        chunker.append(&raw.as_bytes()[70..]);
        let last = chunker
            .next("stdout", true, 0, |from, to| {
                Ok::<_, ()>(raw[from as usize..to as usize].to_owned())
            })
            .unwrap()
            .unwrap();
        assert!(
            matches!(last, OutputFrame::Chunk { ref text, end_offset, .. } if text == "after" && end_offset == raw.len() as u64)
        );
    }

    #[test]
    fn shells_v1_output_unterminated_escape_at_end_advances_raw_offset() {
        let frames = chunks(b"ok\x1b]unclosed", true, 64);
        assert_eq!(text(&frames), "ok");
        assert!(matches!(
            frames.last().unwrap(),
            OutputFrame::Chunk { end_offset: 12, .. }
        ));
    }

    #[test]
    fn shells_v1_output_secret_straddling_arrivals_is_held_and_masked() {
        use std::ffi::OsStr;
        let secret = "fixture-opaque-value";
        let secrets = crate::log_redact::SecretValues::from_environment([(
            OsStr::new("TOKEN"),
            OsStr::new(secret),
        )]);
        let raw = format!("prefix {secret} suffix");
        let mut chunker = Chunker::new(0, 64, true);
        chunker.append(&raw.as_bytes()[..15]);
        let mask = |from: u64, to: u64| {
            let mut range = from as usize..to as usize;
            let text = secrets.redact_range(&raw, Some(&mut range));
            Ok::<_, ()>(text[range].to_owned())
        };
        assert!(chunker
            .next("stdout", false, secrets.longest_value_len(), mask)
            .unwrap()
            .is_none());
        chunker.append(&raw.as_bytes()[15..]);
        let mut frames = Vec::new();
        while let Some(frame) = chunker
            .next("stdout", true, secrets.longest_value_len(), mask)
            .unwrap()
        {
            frames.push(frame);
        }
        assert_eq!(
            text(&frames),
            format!("prefix {} suffix", crate::log_redact::SECRET_PLACEHOLDER)
        );
        assert!(!text(&frames).contains(secret));
    }

    #[test]
    fn shells_v1_output_default_backfill_is_last_64k_per_stream() {
        assert_eq!(backfill_start(100_000, None), 100_000 - 65_536);
        assert_eq!(backfill_start(100_000, Some(12)), 12);
        assert_eq!(backfill_start(7, None), 0);
    }

    #[test]
    fn shells_v1_output_secret_crossing_chunk_limit_never_appears() {
        use std::ffi::OsStr;
        let secret = "fixture-opaque-secret-value";
        let secrets = crate::log_redact::SecretValues::from_environment([(
            OsStr::new("TOKEN"),
            OsStr::new(secret),
        )]);
        let raw = format!("{}{secret} suffix", "x".repeat(61));
        let mut chunker = Chunker::new(0, 64, true);
        chunker.append(raw.as_bytes());
        let mut frames = Vec::new();
        while let Some(frame) = chunker
            .next("stdout", true, secrets.longest_value_len(), |from, to| {
                let mut range = from as usize..to as usize;
                let masked = secrets.redact_range(&raw, Some(&mut range));
                Ok::<_, ()>(masked[range].to_owned())
            })
            .unwrap()
        {
            frames.push(frame);
        }
        assert!(frames.len() > 1);
        assert_eq!(
            text(&frames),
            format!(
                "{}{} suffix",
                "x".repeat(61),
                crate::log_redact::SECRET_PLACEHOLDER
            )
        );
    }
}
