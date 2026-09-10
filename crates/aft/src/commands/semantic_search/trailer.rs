use std::fmt;

use crate::list_envelope::{ListEnvelope, Reason, Total, Unit};

use super::paging::{SearchPage, StopState};

pub const SEARCH_NARROW_FIELDS: &[&str] = &["offset", "topK", "path", "includeTests"];

pub const fn stop_reason_word(stop_state: StopState) -> &'static str {
    match stop_state {
        StopState::S1MoreAtDepth => "more at greater depth",
        StopState::S2Exhausted => "exhausted",
        StopState::S3DepthCap => "depth cap",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchTotal {
    Exact(usize),
    AtLeast(usize),
}

impl SearchTotal {
    pub const fn campaign_suffix(self) -> &'static str {
        match self {
            Self::Exact(_) => "",
            Self::AtLeast(_) => "+",
        }
    }

    fn shared_total(self) -> Total {
        match self {
            Self::Exact(value) => Total::Exact(value),
            Self::AtLeast(value) => Total::AtLeast(value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExactPassState<'a> {
    Complete,
    Bounded {
        files: usize,
        reason: &'a str,
        disclosure_lines: &'a [&'a str],
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingBoundedExactPassDisclosure {
    expected_line: String,
}

impl fmt::Display for MissingBoundedExactPassDisclosure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "an exhausted exact pass must include `{}` in the same reply",
            self.expected_line
        )
    }
}

impl std::error::Error for MissingBoundedExactPassDisclosure {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchTrailer {
    pub shown: usize,
    pub total: SearchTotal,
    pub stop_state: StopState,
}

impl SearchTrailer {
    pub fn from_page(
        page: &SearchPage,
        exact_pass: ExactPassState<'_>,
    ) -> Result<Self, MissingBoundedExactPassDisclosure> {
        if page.stop_state == StopState::S2Exhausted {
            if let ExactPassState::Bounded {
                files,
                reason,
                disclosure_lines,
            } = exact_pass
            {
                let expected_line = format!("exact pass: bounded ({files} files, {reason})");
                if !disclosure_lines
                    .iter()
                    .any(|line| line.starts_with(&expected_line))
                {
                    return Err(MissingBoundedExactPassDisclosure { expected_line });
                }
            }
        }

        let total = match page.stop_state {
            StopState::S2Exhausted => SearchTotal::Exact(page.total_at_stop()),
            StopState::S1MoreAtDepth | StopState::S3DepthCap => {
                SearchTotal::AtLeast(page.total_at_stop())
            }
        };
        Ok(Self {
            shown: page.shown(),
            total,
            stop_state: page.stop_state,
        })
    }

    /// Projects search totals and narrow fields into the shared list-envelope grammar.
    /// The stop reason word remains explicit so the surface registry can preserve
    /// search-specific semantics without duplicating the envelope format.
    pub fn shared_envelope_projection(&self) -> ListEnvelope {
        let reason = match self.stop_state {
            StopState::S1MoreAtDepth => Reason::Cap,
            StopState::S2Exhausted => Reason::Walk,
            StopState::S3DepthCap => Reason::Depth,
        };
        ListEnvelope::new(
            self.shown,
            self.total.shared_total(),
            Unit::Results,
            vec![reason],
            SEARCH_NARROW_FIELDS,
        )
    }

    pub fn render(&self) -> String {
        let shared =
            crate::subc_format::render_envelope_trailer(&self.shared_envelope_projection())
                .expect("every search stop state has a list-envelope reason");
        let shared_reason = match self.stop_state {
            StopState::S1MoreAtDepth => "cap",
            StopState::S2Exhausted => "walk",
            StopState::S3DepthCap => "depth",
        };
        let mut rendered = shared.replace(
            &format!("({shared_reason})"),
            &format!("({})", stop_reason_word(self.stop_state)),
        );
        if let SearchTotal::AtLeast(value) = self.total {
            rendered = rendered.replace(&format!("≥{value}"), &format!("{value}+"));
        }
        rendered
    }
}
