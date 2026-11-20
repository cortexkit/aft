use serde::{Deserialize, Serialize};

/// Maximum total count before render-time display clamping (R23).
pub const R23_MAX_DISPLAY_TOTAL: usize = 999_999_999;
/// Threshold at or above which total count is clamped to `≥999999999` (R23).
pub const R23_CLAMP_THRESHOLD: usize = 1_000_000_000;

/// Reason why a list-shaped surface was cut short.
/// Precedence ordering: TimeLimit > Walk > Depth > Budget > Cap > Kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// Entries were counted instead of listed because of what kind of file
    /// holds them (grep counts lockfile and log lines, for example).
    Kind,
    Cap,
    Budget,
    Depth,
    Walk,
    TimeLimit,
}

impl Reason {
    /// Numerical precedence rank: higher value means higher precedence.
    pub const fn precedence(self) -> u8 {
        match self {
            Self::TimeLimit => 5,
            Self::Walk => 4,
            Self::Depth => 3,
            Self::Budget => 2,
            Self::Cap => 1,
            Self::Kind => 0,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TimeLimit => "time limit",
            Self::Walk => "walk",
            Self::Depth => "depth",
            Self::Budget => "budget",
            Self::Cap => "cap",
            Self::Kind => "kind",
        }
    }
}

/// Permitted unit word for a list-shaped surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    Sites,
    Paths,
    Files,
    Results,
    Rows,
    Lines,
    Items,
    Directories,
    Hops,
}

impl Unit {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sites => "sites",
            Self::Paths => "paths",
            Self::Files => "files",
            Self::Results => "results",
            Self::Rows => "rows",
            Self::Lines => "lines",
            Self::Items => "items",
            Self::Directories => "directories",
            Self::Hops => "hops",
        }
    }
}

impl std::fmt::Display for Unit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Total count of items, either exact or a proved lower bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Total {
    Exact(usize),
    AtLeast(usize),
}

impl Total {
    pub fn value(&self) -> usize {
        match *self {
            Total::Exact(v) | Total::AtLeast(v) => v,
        }
    }

    pub fn is_exact(&self) -> bool {
        matches!(self, Total::Exact(_))
    }

    pub fn is_at_least(&self) -> bool {
        matches!(self, Total::AtLeast(_))
    }
}

/// Common truncation envelope returned beside list-shaped replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListEnvelope {
    pub shown: usize,
    pub total: Total,
    pub unit: Unit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    pub causes: Vec<Reason>,
    pub narrow: Vec<String>,
}

impl ListEnvelope {
    pub fn new(
        shown: usize,
        total: Total,
        unit: Unit,
        mut causes: Vec<Reason>,
        narrow: &[&str],
    ) -> Self {
        causes.sort_by(|a, b| b.precedence().cmp(&a.precedence()));
        causes.dedup();
        let reason = causes.first().copied();
        Self {
            shown,
            total,
            unit,
            reason,
            causes,
            narrow: narrow.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Format a total count in the envelope's form: bare integer for Exact,
/// prefixed with `≥` for AtLeast, and clamped to `≥999999999` if >= 1e9 (R23).
pub fn render_total(total: &Total) -> String {
    if total.value() >= R23_CLAMP_THRESHOLD {
        format!("≥{R23_MAX_DISPLAY_TOTAL}")
    } else {
        match *total {
            Total::Exact(v) => format!("{v}"),
            Total::AtLeast(v) => format!("≥{v}"),
        }
    }
}

/// Render the trailer text for a list envelope.
///
/// Trailer grammar (pinned):
/// `shown <n> of <total> <unit> (<reason>) · narrow: <a>, <b>`
/// - Decimal integers, no separators
/// - `Exact` bare, `AtLeast` prefixed `≥`
/// - No trailing punctuation
/// - Narrow clause omitted only for surfaces with `narrow: []`
/// - Emitted iff `reason.is_some()`
/// - Clamped to `≥999999999` at render time if `total.value >= 1e9`
pub fn render_trailer(envelope: &ListEnvelope) -> Option<String> {
    let reason = envelope.reason?;
    let shown = envelope.shown;

    let total_str = render_total(&envelope.total);

    let unit_str = envelope.unit.as_str();
    let reason_str = reason.as_str();

    let base = format!("shown {shown} of {total_str} {unit_str} ({reason_str})");
    if envelope.narrow.is_empty() {
        Some(base)
    } else {
        Some(format!("{base} · narrow: {}", envelope.narrow.join(", ")))
    }
}

/// Measure the rendered trailer byte length without exposing trailer text to producers (R13).
pub fn measure_trailer_len(envelope: &ListEnvelope) -> usize {
    render_trailer(envelope).map(|s| s.len()).unwrap_or(0)
}

/// Derive the wire key for serializing a list envelope per R14/R21.
/// - For array lists: `<last-id-segment>_list_envelope` beside the array in its owning object.
/// - For text surfaces: `<full-id-with-_-replacing-.>_list_envelope` at the reply root.
pub fn derive_wire_key(list_id: &str, is_text_surface: bool) -> String {
    if is_text_surface {
        format!("{}_list_envelope", list_id.replace('.', "_"))
    } else {
        let last_segment = list_id.rsplit('.').next().unwrap_or(list_id);
        format!("{last_segment}_list_envelope")
    }
}
