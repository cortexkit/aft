//! Parsing and Phase-1 safety checks for the hashline patch language.
//!
//! This module deliberately separates a patch's *requested* coordinates from
//! execution. Every address is resolved against the retained snapshot before a
//! caller plans a mutation, and exact verification compares that snapshot with
//! one caller-owned baseline. That keeps later operations in the same patch
//! from silently renumbering the coordinates of earlier input.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::hashline::scan::{scan_bytes, RawLineRecord, Snapshot};
use crate::hashline::snapshot::{equivalent_snapshots, SnapshotLookupError, SnapshotStore};

/// The hashline edit entry point accepts exactly one argument field, `patch`,
/// while hashline validation is enabled.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HashlineRequest {
    pub patch: String,
}

/// Validate raw tool arguments before any legacy edit normalization can run.
///
/// The gate-on surface owns exactly one field. Rejecting unknown fields is
/// intentional: accepting a legacy `path`, `edits`, or `oldString` field would
/// make an agent believe that hashline verified input which the native engine
/// never parsed.
pub fn validate_raw_arguments(arguments: &Value) -> Result<HashlineRequest, HashlineRejection> {
    let Some(object) = arguments.as_object() else {
        return Err(HashlineRejection::parse(
            "hashline edit arguments must be an object containing only patch",
        ));
    };
    if object.len() != 1 || !object.contains_key("patch") {
        return Err(HashlineRejection::parse(
            "hashline edit arguments must contain only the patch field",
        ));
    }
    let Some(patch) = object.get("patch").and_then(Value::as_str) else {
        return Err(HashlineRejection::parse("patch must be a string"));
    };
    if patch.trim().is_empty() {
        return Err(HashlineRejection::parse("patch must not be empty"));
    }
    Ok(HashlineRequest {
        patch: patch.to_string(),
    })
}

/// A stable pre-execution rejection code.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HashlineRejectionCode {
    MissingTag,
    MalformedTag,
    UnknownTag,
    EvictedTag,
    AmbiguousTag,
    StaleTag,
    UnseenLine,
    BoundaryIneligible,
    UntaggablePath,
    RegisterOverflow,
    BackupUnavailable,
    ParseError,
}

impl HashlineRejectionCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingTag => "hashline_missing_tag",
            Self::MalformedTag => "hashline_malformed_tag",
            Self::UnknownTag => "hashline_unknown_tag",
            Self::EvictedTag => "hashline_evicted_tag",
            Self::AmbiguousTag => "hashline_ambiguous_tag",
            Self::StaleTag => "hashline_stale_tag",
            Self::UnseenLine => "hashline_unseen_line",
            Self::BoundaryIneligible => "hashline_boundary_ineligible",
            Self::UntaggablePath => "hashline_untaggable_path",
            Self::RegisterOverflow => "hashline_register_overflow",
            Self::BackupUnavailable => "hashline_backup_unavailable",
            Self::ParseError => "hashline_parse_error",
        }
    }
}

impl fmt::Display for HashlineRejectionCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The one canonical Phase-1 adjudication stage enum.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RejectionStage {
    Parse,
    Header,
    Path,
    Resolution,
    Eligibility,
    Verification,
    Recovery,
    Register,
    Baseline,
}

impl RejectionStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Parse => "parse",
            Self::Header => "header",
            Self::Path => "path",
            Self::Resolution => "resolution",
            Self::Eligibility => "eligibility",
            Self::Verification => "verification",
            Self::Recovery => "recovery",
            Self::Register => "register",
            Self::Baseline => "baseline",
        }
    }
}

impl fmt::Display for RejectionStage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Transport-neutral details for a Phase-1 rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HashlineRejection {
    pub code: HashlineRejectionCode,
    pub stage: RejectionStage,
    pub message: String,
    pub steering: String,
}

impl HashlineRejection {
    pub fn new(
        code: HashlineRejectionCode,
        stage: RejectionStage,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            stage,
            message: message.into(),
            steering: steering_for(code, stage).to_string(),
        }
    }

    pub fn parse(message: impl Into<String>) -> Self {
        Self::new(
            HashlineRejectionCode::ParseError,
            RejectionStage::Parse,
            message,
        )
    }

    pub fn missing_tag(message: impl Into<String>) -> Self {
        Self::new(
            HashlineRejectionCode::MissingTag,
            RejectionStage::Header,
            message,
        )
    }

    pub fn malformed_tag(message: impl Into<String>) -> Self {
        Self::new(
            HashlineRejectionCode::MalformedTag,
            RejectionStage::Header,
            message,
        )
    }

    pub fn resolution(code: HashlineRejectionCode, message: impl Into<String>) -> Self {
        debug_assert!(matches!(
            code,
            HashlineRejectionCode::UnknownTag
                | HashlineRejectionCode::EvictedTag
                | HashlineRejectionCode::AmbiguousTag
        ));
        Self::new(code, RejectionStage::Resolution, message)
    }

    pub fn eligibility(code: HashlineRejectionCode, message: impl Into<String>) -> Self {
        debug_assert!(matches!(
            code,
            HashlineRejectionCode::UnseenLine | HashlineRejectionCode::BoundaryIneligible
        ));
        Self::new(code, RejectionStage::Eligibility, message)
    }

    pub fn stale_verification(message: impl Into<String>) -> Self {
        Self::new(
            HashlineRejectionCode::StaleTag,
            RejectionStage::Verification,
            message,
        )
    }

    pub fn stale_recovery(message: impl Into<String>) -> Self {
        Self::new(
            HashlineRejectionCode::StaleTag,
            RejectionStage::Recovery,
            message,
        )
    }

    pub fn ambiguous_recovery(message: impl Into<String>) -> Self {
        Self::new(
            HashlineRejectionCode::AmbiguousTag,
            RejectionStage::Recovery,
            message,
        )
    }

    pub fn untaggable_path(message: impl Into<String>) -> Self {
        Self::new(
            HashlineRejectionCode::UntaggablePath,
            RejectionStage::Path,
            message,
        )
    }

    pub fn register_overflow(message: impl Into<String>) -> Self {
        Self::new(
            HashlineRejectionCode::RegisterOverflow,
            RejectionStage::Register,
            message,
        )
    }

    pub fn backup_unavailable(message: impl Into<String>) -> Self {
        Self::new(
            HashlineRejectionCode::BackupUnavailable,
            RejectionStage::Baseline,
            message,
        )
    }
}

impl fmt::Display for HashlineRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at {}: {}",
            self.code, self.stage, self.message
        )
    }
}

impl std::error::Error for HashlineRejection {}

fn steering_for(code: HashlineRejectionCode, stage: RejectionStage) -> &'static str {
    match (code, stage) {
        (HashlineRejectionCode::MissingTag | HashlineRejectionCode::MalformedTag, _) => {
            "run `read <path>` on the file, then include the four-hex tag it returns"
        }
        (
            HashlineRejectionCode::UnknownTag | HashlineRejectionCode::EvictedTag,
            RejectionStage::Resolution,
        ) => "run `read <path>` again to mint a fresh tag, then retry the edit",
        (HashlineRejectionCode::AmbiguousTag, RejectionStage::Resolution) => {
            "use apply_patch or another available non-hashline edit surface; re-reading preserves this colliding four-hex tag"
        }
        (HashlineRejectionCode::AmbiguousTag, RejectionStage::Recovery) => {
            "re-address the current tagged content; the stale span has multiple verbatim landings"
        }
        (HashlineRejectionCode::StaleTag, RejectionStage::Verification) => {
            "run `read <path>` over the addressed range because required boundary context changed"
        }
        (HashlineRejectionCode::StaleTag, RejectionStage::Recovery) => {
            "re-address the current tagged content; the stale span no longer occurs verbatim"
        }
        (HashlineRejectionCode::UnseenLine | HashlineRejectionCode::BoundaryIneligible, _) => {
            "re-read the file to mint a fresh tag that includes every addressed row and boundary, then retry the edit"
        }
        (HashlineRejectionCode::UntaggablePath, _) => {
            "choose a writable regular text file or use an available non-hashline surface"
        }
        (HashlineRejectionCode::RegisterOverflow, _) => {
            "reduce register contents before retrying the patch"
        }
        (HashlineRejectionCode::BackupUnavailable, _) => {
            "enable backups or use apply_patch for this destructive change"
        }
        (HashlineRejectionCode::ParseError, _) => {
            "submit only a hashline patch with tagged section headers and valid operations"
        }
        _ => "run `read <path>` again to mint a fresh tag, then retry the edit",
    }
}

/// A parsed patch. Section order is patch order and must be retained by later
/// planning and execution stages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Patch {
    pub sections: Vec<PatchSection>,
}

impl Patch {
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }
}

/// A per-file patch unit headed by the requested path and its snapshot tag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatchSection {
    pub header: SectionHeader,
    pub operations: Vec<Operation>,
    pub line: usize,
}

/// The lossless path spelling paired with a normalized tag.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SectionHeader {
    pub requested_path: String,
    pub tag: String,
}

impl SectionHeader {
    pub fn new(
        requested_path: impl Into<String>,
        tag: impl AsRef<str>,
    ) -> Result<Self, HashlineRejection> {
        let requested_path = requested_path.into();
        if requested_path.is_empty() {
            return Err(HashlineRejection::missing_tag(
                "a section header must name a path before its tag",
            ));
        }
        let tag = normalize_tag(tag.as_ref())?;
        Ok(Self {
            requested_path,
            tag,
        })
    }
}

/// One hashline operation. Body-bearing PUTs retain logical body lines so the
/// apply layer can select the target file's terminator policy after verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Operation {
    Put(PutOperation),
    Cut(CutOperation),
    Rem(RemOperation),
    Mv(MvOperation),
}

impl Operation {
    pub fn address(&self) -> Option<&Address> {
        match self {
            Self::Put(operation) => Some(&operation.address),
            Self::Cut(operation) => Some(&operation.address),
            Self::Rem(_) | Self::Mv(_) => None,
        }
    }

    fn append_body_line(&mut self, line: &str) -> bool {
        match self {
            Self::Put(PutOperation {
                source: PutSource::Text(lines),
                ..
            }) => {
                let Some(content) = line.strip_prefix('+') else {
                    return false;
                };
                lines.push(content.to_string());
                true
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PutOperation {
    pub address: Address,
    pub source: PutSource,
    pub line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PutSource {
    Text(Vec<String>),
    Register(RegisterRef),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CutOperation {
    pub address: Address,
    pub register: Option<RegisterRef>,
    pub line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemOperation {
    pub line: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MvOperation {
    pub destination: String,
    pub line: usize,
}

/// Named registers are durable only for the life of the session; the anonymous
/// register is represented explicitly so it cannot collide with an empty name.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum RegisterRef {
    Anonymous,
    Named(String),
}

/// A reference whose value is resolved solely from `Snapshot::total_lines`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LineReference {
    Absolute(usize),
    /// `$` is offset zero; `$-1` is the penultimate snapshot row.
    EofRelative(usize),
}

/// One parsed pre-request address.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Address {
    /// `0` and `<1` are both BOF insertion forms.
    Bof,
    Line(LineReference),
    Range {
        start: LineReference,
        end: LineReference,
    },
    Gap {
        side: GapSide,
        line: LineReference,
    },
    Block(LineReference),
    /// A gap adjacent to a syntactic block, such as `>8*`.
    BlockGap {
        side: GapSide,
        line: LineReference,
    },
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GapSide {
    Before,
    After,
}

/// Parse a complete patch after raw tool arguments selected the hashline path.
/// Accept an optional `*** Begin Patch` / `*** End Patch` envelope, but reject
/// any other preamble or trailing content instead of ignoring it.
pub fn parse_hashline_patch(patch: &str) -> Result<Patch, HashlineRejection> {
    if patch.trim().is_empty() {
        return Err(HashlineRejection::parse("patch must not be empty"));
    }

    let mut sections = Vec::new();
    let mut envelope_started = false;
    let mut envelope_ended = false;
    let mut lines = patch.split('\n').enumerate().peekable();
    while let Some((index, raw_line)) = lines.next() {
        let line_number = index + 1;
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        let trimmed = line.trim();

        // `split('\n')` yields one final empty item for a terminal newline. It
        // is not a text-body row, while a deliberate blank body row remains `+`.
        if raw_line.is_empty() && lines.peek().is_none() {
            continue;
        }

        if trimmed == "*** Begin Patch" {
            if envelope_started || !sections.is_empty() {
                return Err(HashlineRejection::parse(format!(
                    "unexpected patch envelope start at line {line_number}"
                )));
            }
            envelope_started = true;
            continue;
        }
        if trimmed == "*** End Patch" {
            if !envelope_started || envelope_ended {
                return Err(HashlineRejection::parse(format!(
                    "unexpected patch envelope end at line {line_number}"
                )));
            }
            envelope_ended = true;
            continue;
        }
        if envelope_ended {
            if !trimmed.is_empty() {
                return Err(HashlineRejection::parse(format!(
                    "content follows the patch envelope at line {line_number}"
                )));
            }
            continue;
        }

        if is_header_line(trimmed) {
            let header = parse_section_header(trimmed)?;
            sections.push(PatchSection {
                header,
                operations: Vec::new(),
                line: line_number,
            });
            continue;
        }

        if is_directive(line) {
            let section = sections.last_mut().ok_or_else(|| {
                HashlineRejection::parse(format!(
                    "operation at line {line_number} appears before a tagged section header"
                ))
            })?;
            section.operations.push(parse_operation(line, line_number)?);
            continue;
        }

        if trimmed.is_empty() && sections.is_empty() {
            continue;
        }

        let Some(section) = sections.last_mut() else {
            return Err(HashlineRejection::parse(format!(
                "expected a tagged section header at line {line_number}"
            )));
        };
        let Some(operation) = section.operations.last_mut() else {
            return Err(HashlineRejection::parse(format!(
                "expected an operation after section header at line {}",
                section.line
            )));
        };
        if !operation.append_body_line(line) {
            let message = match operation {
                Operation::Put(PutOperation {
                    source: PutSource::Register(_),
                    line: operation_line,
                    ..
                }) => format!(
                    "PUT at line {operation_line} copies a register and accepts no body; \
                     use `PUT <address>:` followed by `+` rows for text (unexpected content at line {line_number})"
                ),
                Operation::Put(PutOperation {
                    source: PutSource::Text(_),
                    ..
                }) => format!(
                    "invalid text PUT body row at patch line {line_number}; \
                     expected `+<text>` for content or bare `+` for a blank row, \
                     but the row does not begin with `+`"
                ),
                _ => format!(
                    "only `PUT <address>:` accepts body rows beginning with `+`; \
                     unexpected content at line {line_number}"
                ),
            };
            return Err(HashlineRejection::parse(message));
        }
    }

    if envelope_started && !envelope_ended {
        return Err(HashlineRejection::parse(
            "patch envelope is missing *** End Patch",
        ));
    }
    if sections.is_empty() {
        return Err(HashlineRejection::parse(
            "patch must contain at least one tagged section header",
        ));
    }
    if let Some(section) = sections
        .iter()
        .find(|section| section.operations.is_empty())
    {
        return Err(HashlineRejection::parse(format!(
            "section at line {} contains no operation",
            section.line
        )));
    }
    for section in &sections {
        validate_section_composition(section)?;
        for operation in &section.operations {
            if let Operation::Put(PutOperation {
                source: PutSource::Text(lines),
                line,
                ..
            }) = operation
            {
                if lines.is_empty() {
                    return Err(HashlineRejection::parse(format!(
                        "PUT at patch line {line} requires one or more body rows; \
                         expected `+<text>` for content or bare `+` for a blank row"
                    )));
                }
            }
        }
    }
    Ok(Patch { sections })
}

fn validate_section_composition(section: &PatchSection) -> Result<(), HashlineRejection> {
    let rem_count = section
        .operations
        .iter()
        .filter(|operation| matches!(operation, Operation::Rem(_)))
        .count();
    if rem_count > 0 && section.operations.len() != 1 {
        return Err(HashlineRejection::parse(format!(
            "REM at section line {} cannot be combined with line operations",
            section.line
        )));
    }
    let mv_positions: Vec<usize> = section
        .operations
        .iter()
        .enumerate()
        .filter_map(|(index, operation)| matches!(operation, Operation::Mv(_)).then_some(index))
        .collect();
    if mv_positions.len() > 1
        || mv_positions
            .first()
            .is_some_and(|index| *index + 1 != section.operations.len())
    {
        return Err(HashlineRejection::parse(format!(
            "MV at section line {} must occur once and after all line operations",
            section.line
        )));
    }
    Ok(())
}

/// Parse one recognizable header and distinguish absent and malformed tags.
pub fn parse_section_header(line: &str) -> Result<SectionHeader, HashlineRejection> {
    let trimmed = line.trim();
    if !is_header_line(trimmed) {
        return Err(HashlineRejection::parse(
            "section headers must use [requested-path#TAG]",
        ));
    }
    let inner = &trimmed[1..trimmed.len() - 1];
    let Some((path, tag)) = inner.split_once('#') else {
        return Err(HashlineRejection::missing_tag(
            "section header is missing its #TAG handle",
        ));
    };
    if path.is_empty() {
        return Err(HashlineRejection::missing_tag(
            "section header must name a path before #TAG",
        ));
    }
    if path.contains('#') || path.contains(['\r', '\n']) {
        return Err(HashlineRejection::parse(
            "section paths cannot contain #, carriage return, or newline",
        ));
    }
    SectionHeader::new(path, tag)
}

fn is_header_line(line: &str) -> bool {
    line.starts_with('[') && line.ends_with(']') && line.len() >= 2
}

fn is_directive(line: &str) -> bool {
    ["PUT", "CUT", "REM", "MV"].into_iter().any(|keyword| {
        line == keyword
            || line
                .strip_prefix(keyword)
                .is_some_and(|remainder| remainder.starts_with(char::is_whitespace))
    })
}

fn parse_operation(line: &str, line_number: usize) -> Result<Operation, HashlineRejection> {
    let trimmed = line.trim();
    if trimmed == "REM" {
        return parse_rem("", line_number);
    }
    let (keyword, remainder) = trimmed.split_once(char::is_whitespace).ok_or_else(|| {
        HashlineRejection::parse(format!(
            "operation at line {line_number} lacks required input"
        ))
    })?;
    let remainder = remainder.trim();
    match keyword {
        "PUT" => parse_put(remainder, line_number),
        "CUT" => parse_cut(remainder, line_number),
        "REM" => parse_rem(remainder, line_number),
        "MV" => parse_mv(remainder, line_number),
        _ => Err(HashlineRejection::parse(format!(
            "unknown operation {keyword:?} at line {line_number}"
        ))),
    }
}

fn parse_put(remainder: &str, line: usize) -> Result<Operation, HashlineRejection> {
    let Some(prefix) = remainder.strip_suffix(':') else {
        let mut parts = remainder.split_whitespace();
        let address = parts.next().ok_or_else(|| {
            HashlineRejection::parse(format!("PUT at line {line} lacks an address"))
        })?;
        let register = match parts.next() {
            Some(register) if address.ends_with(':') && !register.starts_with('@') => {
                return Err(HashlineRejection::parse(format!(
                    "invalid text PUT body row at patch line {line}; \
                     expected `+<text>` for content or bare `+` for a blank row, \
                     but the row carried its body inline; PUT bodies go on the following lines \
                     as `+TEXT` rows"
                )));
            }
            Some(register) => parse_register(register)?,
            // When no register is specified, use the anonymous register. This
            // supports `PUT >$` immediately after a bare CUT.
            None => RegisterRef::Anonymous,
        };
        if parts.next().is_some() {
            return Err(HashlineRejection::parse(format!(
                "PUT at line {line} accepts at most one register source"
            )));
        }
        return Ok(Operation::Put(PutOperation {
            address: parse_address(address)?,
            source: PutSource::Register(register),
            line,
        }));
    };
    let address = prefix.trim();
    if address.is_empty() {
        return Err(HashlineRejection::parse(format!(
            "PUT at line {line} lacks an address before :"
        )));
    }
    Ok(Operation::Put(PutOperation {
        address: parse_address(address)?,
        source: PutSource::Text(Vec::new()),
        line,
    }))
}

fn parse_cut(remainder: &str, line: usize) -> Result<Operation, HashlineRejection> {
    let mut parts = remainder.split_whitespace();
    let address = parts
        .next()
        .ok_or_else(|| HashlineRejection::parse(format!("CUT at line {line} lacks an address")))?;
    let register = match parts.next() {
        Some(register) => Some(parse_register(register)?),
        None => None,
    };
    if parts.next().is_some() {
        return Err(HashlineRejection::parse(format!(
            "CUT at line {line} has unexpected trailing input"
        )));
    }
    Ok(Operation::Cut(CutOperation {
        address: parse_address(address)?,
        register,
        line,
    }))
}

fn parse_rem(remainder: &str, line: usize) -> Result<Operation, HashlineRejection> {
    if !remainder.trim().is_empty() {
        return Err(HashlineRejection::parse(format!(
            "REM at line {line} removes the whole section file and accepts no address"
        )));
    }
    Ok(Operation::Rem(RemOperation { line }))
}

fn parse_mv(remainder: &str, line: usize) -> Result<Operation, HashlineRejection> {
    let destination = remainder.trim();
    if destination.is_empty() || destination.split_whitespace().count() != 1 {
        return Err(HashlineRejection::parse(format!(
            "MV at line {line} requires exactly one destination path"
        )));
    }
    Ok(Operation::Mv(MvOperation {
        destination: unquote(destination).to_string(),
        line,
    }))
}

fn parse_register(input: &str) -> Result<RegisterRef, HashlineRejection> {
    let Some(name) = input.strip_prefix('@') else {
        return Err(HashlineRejection::parse("registers must begin with @"));
    };
    if name.is_empty()
        || !name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '-'
        })
    {
        return Err(HashlineRejection::parse(
            "register names may contain only ASCII letters, digits, _, and -",
        ));
    }
    Ok(RegisterRef::Named(name.to_string()))
}

/// Parse line, range, gap, block, and EOF-relative forms without consulting a
/// live file. Resolution happens later against a snapshot's recorded line count.
pub fn parse_address(input: &str) -> Result<Address, HashlineRejection> {
    let input = input.trim();
    if input == "0" {
        return Ok(Address::Bof);
    }
    if let Some((side, line)) = input
        .strip_prefix('<')
        .map(|line| (GapSide::Before, line))
        .or_else(|| input.strip_prefix('>').map(|line| (GapSide::After, line)))
    {
        if let Some(block_line) = line.strip_suffix('*') {
            return Ok(Address::BlockGap {
                side,
                line: parse_line_reference(block_line)?,
            });
        }
        return Ok(Address::Gap {
            side,
            line: parse_line_reference(line)?,
        });
    }
    if let Some(line) = input.strip_suffix('*') {
        return Ok(Address::Block(parse_line_reference(line)?));
    }
    // The canonical range separator is `.=`, but these alternate forms retain
    // compatibility with existing hashline input.
    for separator in ["..=", ".=", ".."] {
        if let Some((start, end)) = input.split_once(separator) {
            return Ok(Address::Range {
                start: parse_line_reference(start)?,
                end: parse_line_reference(end)?,
            });
        }
    }
    Ok(Address::Line(parse_line_reference(input)?))
}

fn parse_line_reference(input: &str) -> Result<LineReference, HashlineRejection> {
    let input = input.trim();
    if input == "$" {
        return Ok(LineReference::EofRelative(0));
    }
    if let Some(offset) = input.strip_prefix("$-") {
        return Ok(LineReference::EofRelative(parse_positive_usize(offset)?));
    }
    let line = parse_positive_usize(input)?;
    Ok(LineReference::Absolute(line))
}

fn parse_positive_usize(input: &str) -> Result<usize, HashlineRejection> {
    let value = input.parse::<usize>().map_err(|_| {
        HashlineRejection::parse(format!("{input:?} is not a valid positive line number"))
    })?;
    if value == 0 {
        return Err(HashlineRejection::parse(
            "line number zero is only valid as the standalone BOF address",
        ));
    }
    Ok(value)
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn normalize_tag(tag: &str) -> Result<String, HashlineRejection> {
    if tag.len() != 4 || !tag.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(HashlineRejection::malformed_tag(
            "section tags must be exactly four hexadecimal digits",
        ));
    }
    Ok(tag.to_ascii_uppercase())
}

/// A fully resolved inclusive span in pre-request coordinates.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LineSpan {
    pub start: usize,
    pub end: usize,
}

impl LineSpan {
    pub fn new(start: usize, end: usize) -> Option<Self> {
        (start > 0 && start <= end).then_some(Self { start, end })
    }

    pub fn lines(self) -> impl Iterator<Item = usize> {
        self.start..=self.end
    }
}

/// A gap resolves to adjacent snapshot rows rather than a current live-file
/// offset. `None` anchors name BOF/EOF facts retained in the snapshot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ResolvedGap {
    pub before: Option<usize>,
    pub after: Option<usize>,
}

/// The result of resolving one parsed address against its retained snapshot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResolvedAddress {
    /// REM and MV address every record in their source section file.
    WholeFile,
    Span(LineSpan),
    Gap(ResolvedGap),
    /// Block parsing is owned here; a language-aware block resolver expands the
    /// anchor to a span before verification and execution.
    BlockAnchor(usize),
    /// A language-aware resolver must expand this block before the gap can be
    /// verified against its predecessor and successor anchors.
    BlockGapAnchor {
        side: GapSide,
        anchor: usize,
    },
}

impl ResolvedAddress {
    pub fn addressed_span(self) -> Option<LineSpan> {
        match self {
            Self::Span(span) => Some(span),
            Self::WholeFile | Self::Gap(_) | Self::BlockAnchor(_) | Self::BlockGapAnchor { .. } => {
                None
            }
        }
    }

    pub fn required_anchors(self) -> Vec<usize> {
        match self {
            Self::Gap(gap) => [gap.before, gap.after].into_iter().flatten().collect(),
            Self::WholeFile
            | Self::Span(_)
            | Self::BlockAnchor(_)
            | Self::BlockGapAnchor { .. } => Vec::new(),
        }
    }
}

/// Resolve an address against the snapshot that minted its handle. This function
/// never reads filesystem state, including for `$` and EOF-relative forms.
pub fn resolve_address(
    address: &Address,
    snapshot: &Snapshot,
) -> Result<ResolvedAddress, HashlineRejection> {
    let total_lines = snapshot.total_lines;
    match address {
        Address::Bof => Ok(ResolvedAddress::Gap(ResolvedGap {
            before: None,
            after: (total_lines > 0).then_some(1),
        })),
        Address::Line(reference) => {
            let line = resolve_line_reference(*reference, total_lines)?;
            Ok(ResolvedAddress::Span(LineSpan {
                start: line,
                end: line,
            }))
        }
        Address::Range { start, end } => {
            let start = resolve_line_reference(*start, total_lines)?;
            let end = resolve_line_reference(*end, total_lines)?;
            let Some(span) = LineSpan::new(start, end) else {
                return Err(HashlineRejection::eligibility(
                    HashlineRejectionCode::BoundaryIneligible,
                    "range start occurs after range end in the retained snapshot",
                ));
            };
            Ok(ResolvedAddress::Span(span))
        }
        Address::Gap { side, line } => {
            let line = resolve_line_reference(*line, total_lines)?;
            let gap = match side {
                GapSide::Before => ResolvedGap {
                    before: line.checked_sub(1),
                    after: Some(line),
                },
                GapSide::After => ResolvedGap {
                    before: Some(line),
                    after: (line < total_lines).then_some(line + 1),
                },
            };
            Ok(ResolvedAddress::Gap(gap))
        }
        Address::Block(reference) => Ok(ResolvedAddress::BlockAnchor(resolve_line_reference(
            *reference,
            total_lines,
        )?)),
        Address::BlockGap { side, line } => Ok(ResolvedAddress::BlockGapAnchor {
            side: *side,
            anchor: resolve_line_reference(*line, total_lines)?,
        }),
    }
}

fn resolve_line_reference(
    reference: LineReference,
    total_lines: usize,
) -> Result<usize, HashlineRejection> {
    let line = match reference {
        LineReference::Absolute(line) if line <= total_lines => Some(line),
        LineReference::EofRelative(offset) if offset < total_lines => Some(total_lines - offset),
        LineReference::Absolute(_) | LineReference::EofRelative(_) => None,
    };
    line.ok_or_else(|| {
        HashlineRejection::eligibility(
            HashlineRejectionCode::BoundaryIneligible,
            "address falls outside the retained snapshot boundary",
        )
    })
}

/// Substitute an authoritative language-aware block span for a parsed block
/// anchor. The supplied span is still constrained to pre-request coordinates.
pub fn expand_block(
    resolved: ResolvedAddress,
    span: LineSpan,
    snapshot: &Snapshot,
) -> Result<ResolvedAddress, HashlineRejection> {
    let (side, anchor) = match resolved {
        ResolvedAddress::BlockAnchor(anchor) => (None, anchor),
        ResolvedAddress::BlockGapAnchor { side, anchor } => (Some(side), anchor),
        ResolvedAddress::WholeFile | ResolvedAddress::Span(_) | ResolvedAddress::Gap(_) => {
            return Err(HashlineRejection::parse(
                "only a parsed block address can be expanded as a block",
            ));
        }
    };
    if !span.lines().any(|line| line == anchor) || span.end > snapshot.total_lines {
        return Err(HashlineRejection::eligibility(
            HashlineRejectionCode::BoundaryIneligible,
            "block resolver returned a span outside the retained snapshot",
        ));
    }
    match side {
        None => Ok(ResolvedAddress::Span(span)),
        Some(GapSide::Before) => Ok(ResolvedAddress::Gap(ResolvedGap {
            before: span.start.checked_sub(1),
            after: Some(span.start),
        })),
        Some(GapSide::After) => Ok(ResolvedAddress::Gap(ResolvedGap {
            before: Some(span.end),
            after: (span.end < snapshot.total_lines).then_some(span.end + 1),
        })),
    }
}

/// Prove that the retained snapshot authorizes every record and boundary fact
/// an address will use. This is deliberately separate from exact verification:
/// an unread row is never eligible merely because current bytes happen to match.
pub fn check_eligibility(
    snapshot: &Snapshot,
    address: ResolvedAddress,
) -> Result<(), HashlineRejection> {
    match address {
        ResolvedAddress::WholeFile => {
            if snapshot.records.len() != snapshot.total_lines
                || (1..=snapshot.total_lines).any(|line| !snapshot.is_seen(line))
            {
                return Err(HashlineRejection::eligibility(
                    HashlineRejectionCode::UnseenLine,
                    "REM and MV require a tagged read that retained the whole source file",
                ));
            }
        }
        ResolvedAddress::Span(span) => {
            for line in span.lines() {
                if !snapshot.is_seen(line) {
                    return Err(HashlineRejection::eligibility(
                        HashlineRejectionCode::UnseenLine,
                        format!("line {line} was not retained by the tagged read"),
                    ));
                }
            }
        }
        ResolvedAddress::BlockAnchor(line)
        | ResolvedAddress::BlockGapAnchor { anchor: line, .. } => {
            if !snapshot.is_seen(line) {
                return Err(HashlineRejection::eligibility(
                    HashlineRejectionCode::UnseenLine,
                    format!("block anchor line {line} was not retained by the tagged read"),
                ));
            }
        }
        ResolvedAddress::Gap(gap) => {
            if gap.before.is_none() && gap.after.is_none() {
                if !snapshot.boundary.empty_file {
                    return Err(HashlineRejection::eligibility(
                        HashlineRejectionCode::BoundaryIneligible,
                        "empty-file gap has no retained empty-file boundary evidence",
                    ));
                }
                return Ok(());
            }
            for line in [gap.before, gap.after].into_iter().flatten() {
                if !snapshot.is_seen(line) {
                    return Err(HashlineRejection::eligibility(
                        HashlineRejectionCode::UnseenLine,
                        format!("gap boundary line {line} was not retained by the tagged read"),
                    ));
                }
            }
            if gap.before.is_none() && !snapshot.boundary.bof_observed {
                return Err(HashlineRejection::eligibility(
                    HashlineRejectionCode::BoundaryIneligible,
                    "BOF boundary evidence was not retained",
                ));
            }
            if gap.after.is_none() && !snapshot.boundary.eof_observed {
                return Err(HashlineRejection::eligibility(
                    HashlineRejectionCode::BoundaryIneligible,
                    "EOF boundary evidence was not retained",
                ));
            }
        }
    }
    Ok(())
}

/// Resolve all sections against their snapshot handles before any later apply
/// stage can mutate a path. A canonical path may appear in several sections,
/// but all of those sections must use equivalent verification evidence.
#[derive(Clone, Debug)]
pub struct ResolvedPatchSection {
    pub section_index: usize,
    pub canonical_path: PathBuf,
    pub snapshot: Snapshot,
    pub operations: Vec<ResolvedOperation>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedOperation {
    pub operation_index: usize,
    /// REM and MV use `WholeFile`; line operations retain their parsed address.
    pub address: ResolvedAddress,
}

pub fn resolve_patch_sections<F>(
    store: &mut SnapshotStore,
    patch: &Patch,
    mut canonicalize: F,
) -> Result<Vec<ResolvedPatchSection>, HashlineRejection>
where
    F: FnMut(&str) -> Result<PathBuf, HashlineRejection>,
{
    let mut views: BTreeMap<PathBuf, Snapshot> = BTreeMap::new();
    let mut resolved = Vec::with_capacity(patch.sections.len());
    for (section_index, section) in patch.sections.iter().enumerate() {
        let canonical_path = canonicalize(&section.header.requested_path)?;
        let snapshot = resolve_snapshot(store, &canonical_path, &section.header.tag)?;
        if let Some(existing) = views.get(&canonical_path) {
            if !equivalent_snapshots(existing, &snapshot) {
                return Err(HashlineRejection::resolution(
                    HashlineRejectionCode::AmbiguousTag,
                    "sections for one canonical path selected different retained verification evidence",
                ));
            }
        } else {
            views.insert(canonical_path.clone(), snapshot.clone());
        }

        let mut operations = Vec::with_capacity(section.operations.len());
        for (operation_index, operation) in section.operations.iter().enumerate() {
            let address = match operation.address() {
                Some(address) => resolve_address(address, &snapshot)?,
                None => ResolvedAddress::WholeFile,
            };
            check_eligibility(&snapshot, address)?;
            operations.push(ResolvedOperation {
                operation_index,
                address,
            });
        }
        resolved.push(ResolvedPatchSection {
            section_index,
            canonical_path,
            snapshot,
            operations,
        });
    }
    Ok(resolved)
}

/// Normalize and resolve one case-insensitive four-hex snapshot tag using the
/// snapshot store's recorded result for the requested canonical path.
pub fn resolve_snapshot(
    store: &mut SnapshotStore,
    canonical_path: impl AsRef<Path>,
    tag: &str,
) -> Result<Snapshot, HashlineRejection> {
    let tag = normalize_tag(tag)?;
    store
        .lookup(canonical_path, &tag)
        .map_err(|error| match error {
            SnapshotLookupError::UnknownTag => HashlineRejection::resolution(
                HashlineRejectionCode::UnknownTag,
                "the tagged snapshot is not resident for this path",
            ),
            SnapshotLookupError::EvictedTag => HashlineRejection::resolution(
                HashlineRejectionCode::EvictedTag,
                "the tagged snapshot was evicted from this session",
            ),
            SnapshotLookupError::AmbiguousTag => HashlineRejection::resolution(
                HashlineRejectionCode::AmbiguousTag,
                "the four-hex tag collides across different normalized file content",
            ),
        })
}

/// A complete Phase-1 baseline owned by one patch and one canonical path.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Baseline {
    pub bytes: Vec<u8>,
    pub snapshot: Snapshot,
}

impl Baseline {
    pub fn from_bytes(bytes: impl Into<Vec<u8>>) -> Self {
        let bytes = bytes.into();
        Self {
            snapshot: scan_bytes(&bytes),
            bytes,
        }
    }

    pub fn raw_record(&self, line: usize) -> Option<&RawLineRecord> {
        self.snapshot.raw_record(line)
    }
}

/// A per-patch cache that makes the one-baseline-per-canonical-path contract
/// explicit. Calling `load_once` again returns the original byte buffer rather
/// than silently replacing the evidence used by another section.
#[derive(Clone, Debug, Default)]
pub struct BaselineCache {
    baselines: BTreeMap<PathBuf, Baseline>,
}

impl BaselineCache {
    pub fn load_once(
        &mut self,
        canonical_path: impl Into<PathBuf>,
        bytes: impl Into<Vec<u8>>,
    ) -> &Baseline {
        let path = canonical_path.into();
        self.baselines
            .entry(path)
            .or_insert_with(|| Baseline::from_bytes(bytes))
    }

    pub fn get(&self, canonical_path: impl AsRef<Path>) -> Option<&Baseline> {
        self.baselines.get(canonical_path.as_ref())
    }

    pub fn len(&self) -> usize {
        self.baselines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.baselines.is_empty()
    }
}

/// The result of exact verification. An addressed-span mismatch is deliberately
/// not a rejection: the recovery planner receives its exact old records. An
/// anchor mismatch remains a loud verification-stage rejection because moving a
/// gap or boundary insertion would change its meaning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerificationOutcome {
    Exact,
    RecoveryRequired(RecoveryPlan),
    Rejected(HashlineRejection),
    BlockNeedsResolution { anchor: usize },
}

/// The exact records from the old addressed span that a recovery plan can use
/// to remap the operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryPlan {
    pub old_span: LineSpan,
    pub expected_records: Vec<RawLineRecord>,
}

/// Verify an already-resolved address against one common baseline. Anchor rows
/// are checked before addressed rows so a stale gap cannot be auto-remapped.
pub fn verify_exact(
    snapshot: &Snapshot,
    baseline: &Baseline,
    address: ResolvedAddress,
) -> VerificationOutcome {
    if let Err(rejection) = check_eligibility(snapshot, address) {
        return VerificationOutcome::Rejected(rejection);
    }
    match address {
        ResolvedAddress::WholeFile => {
            if snapshot.total_lines != baseline.snapshot.total_lines
                || snapshot.records != baseline.snapshot.records
            {
                VerificationOutcome::Rejected(HashlineRejection::stale_verification(
                    "the whole-file source changed since the tagged read",
                ))
            } else {
                VerificationOutcome::Exact
            }
        }
        ResolvedAddress::BlockAnchor(anchor) | ResolvedAddress::BlockGapAnchor { anchor, .. } => {
            VerificationOutcome::BlockNeedsResolution { anchor }
        }
        ResolvedAddress::Gap(gap) => {
            for line in [gap.before, gap.after].into_iter().flatten() {
                if snapshot.raw_record(line) != baseline.raw_record(line) {
                    return VerificationOutcome::Rejected(HashlineRejection::stale_verification(
                        format!("required gap anchor line {line} changed since the tagged read"),
                    ));
                }
            }
            VerificationOutcome::Exact
        }
        ResolvedAddress::Span(span) => {
            let expected_records: Vec<RawLineRecord> = span
                .lines()
                .filter_map(|line| snapshot.raw_record(line).cloned())
                .collect();
            let changed = span
                .lines()
                .any(|line| snapshot.raw_record(line) != baseline.raw_record(line));
            if changed {
                VerificationOutcome::RecoveryRequired(RecoveryPlan {
                    old_span: span,
                    expected_records,
                })
            } else {
                VerificationOutcome::Exact
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::scan::{scan_bytes_with_request, CoverageInput, ScanRequest};
    use crate::hashline::snapshot::MAX_SNAPSHOT_PATHS;

    fn snapshot(bytes: &[u8], lines: impl IntoIterator<Item = usize>) -> Snapshot {
        scan_bytes_with_request(bytes, ScanRequest::new(CoverageInput::lines(lines)))
            .snapshot
            .expect("in-memory snapshots reach EOF")
    }

    #[test]
    fn raw_arguments_allow_only_a_nonempty_patch_string() {
        let request = validate_raw_arguments(&serde_json::json!({
            "patch": "[src/lib.rs#cafe]\nREM"
        }))
        .expect("hashline request");
        assert!(request.patch.contains("REM"));

        for value in [
            serde_json::json!({"patch": "", "path": "src/lib.rs"}),
            serde_json::json!({"patch": 3}),
            serde_json::json!({"path": "src/lib.rs", "edits": []}),
        ] {
            let rejection = validate_raw_arguments(&value).expect_err("legacy shape is rejected");
            assert_eq!(rejection.code, HashlineRejectionCode::ParseError);
            assert_eq!(rejection.stage, RejectionStage::Parse);
        }
    }

    #[test]
    fn headers_distinguish_missing_and_malformed_tags() {
        let missing = parse_section_header("[src/lib.rs]").expect_err("missing tag");
        assert_eq!(missing.code, HashlineRejectionCode::MissingTag);
        assert_eq!(missing.stage, RejectionStage::Header);

        for header in ["[src/lib.rs#]", "[src/lib.rs#abc]", "[src/lib.rs#ABCDE]"] {
            let malformed = parse_section_header(header).expect_err("malformed tag");
            assert_eq!(malformed.code, HashlineRejectionCode::MalformedTag);
            assert_eq!(malformed.stage, RejectionStage::Header);
        }
        assert_eq!(
            parse_section_header("[src/lib.rs#cAfE]")
                .expect("valid header")
                .tag,
            "CAFE"
        );
    }

    #[test]
    fn parser_retains_multisection_operations_and_put_body() {
        let patch = parse_hashline_patch(
            "*** Begin Patch\n[a.rs#cafe]\nPUT <2:\n+first\n+second\nCUT 3 @copied\n[b.rs#BEEF]\nPUT $ @copied\nMV c.rs\n*** End Patch",
        )
        .expect("patch parses");
        assert_eq!(patch.sections.len(), 2);
        assert_eq!(patch.sections[0].operations.len(), 2);
        let Operation::Put(put) = &patch.sections[0].operations[0] else {
            panic!("first operation is PUT");
        };
        assert_eq!(
            put.source,
            PutSource::Text(vec!["first".into(), "second".into()])
        );
        assert!(matches!(
            patch.sections[1].operations[0],
            Operation::Put(PutOperation {
                source: PutSource::Register(RegisterRef::Named(_)),
                ..
            })
        ));
    }

    #[test]
    fn parser_accepts_all_pre_request_address_forms() {
        assert_eq!(parse_address("0").unwrap(), Address::Bof);
        assert!(matches!(
            parse_address("2.=4").unwrap(),
            Address::Range { .. }
        ));
        assert!(matches!(
            parse_address("2..=4").unwrap(),
            Address::Range { .. }
        ));
        assert!(matches!(parse_address("<1").unwrap(), Address::Gap { .. }));
        assert!(matches!(
            parse_address(">$-1").unwrap(),
            Address::Gap { .. }
        ));
        assert!(matches!(
            parse_address(">8*").unwrap(),
            Address::BlockGap {
                side: GapSide::After,
                ..
            }
        ));
        assert!(matches!(parse_address("$*").unwrap(), Address::Block(_)));
    }

    #[test]
    fn parser_guides_text_put_bodies_and_accepts_a_terminal_newline() {
        let register_put = parse_hashline_patch("[a.rs#CAFE]\nPUT 1\n+replacement")
            .expect_err("register PUT cannot accept a text body");
        assert_eq!(register_put.stage, RejectionStage::Parse);
        assert!(register_put.message.contains("PUT <address>:"));
        assert!(register_put.message.contains("accepts no body"));

        let body = parse_hashline_patch("[a.rs#CAFE]\nPUT 1:\nreplacement")
            .expect_err("body rows require the + marker");
        assert!(body.message.contains("`+`"));
        assert!(body.message.contains("blank row"));

        assert!(parse_hashline_patch("[a.rs#CAFE]\nPUT 1:\n+replacement\n").is_ok());
    }

    #[test]
    fn inline_put_body_probes_report_the_body_row_rule() {
        let inline = parse_hashline_patch("[example.txt#CAFE]\nPUT 2: INLINE")
            .expect_err("inline PUT body must be rejected");
        assert_eq!(
            inline.message,
            "invalid text PUT body row at patch line 2; expected `+<text>` for content or bare `+` for a blank row, but the row carried its body inline; PUT bodies go on the following lines as `+TEXT` rows"
        );
        assert!(!inline.message.contains("registers must begin with @"));

        let unmarked = parse_hashline_patch("[example.txt#CAFE]\nPUT 2:\nplain line without plus")
            .expect_err("unmarked PUT body row must be rejected");
        assert_eq!(
            unmarked.message,
            "invalid text PUT body row at patch line 3; expected `+<text>` for content or bare `+` for a blank row, but the row does not begin with `+`"
        );

        let attached = parse_hashline_patch("[example.txt#CAFE]\nPUT 2:INLINE_TEXT")
            .expect_err("attached text remains part of the invalid address");
        assert_eq!(
            attached.message,
            "\"2:INLINE_TEXT\" is not a valid positive line number"
        );

        parse_hashline_patch("[example.txt#CAFE]\nPUT 2:\n+BETA_REPLACED")
            .expect("a following +TEXT row is a valid PUT body");
    }

    #[test]
    fn text_put_body_rows_match_pinned_oracle_edges() {
        for patch in [
            "[a.py#CAFE]\nPUT 1:\n+",
            "[a.py#CAFE]\nPUT 1:\n+\n",
            "[a.py#CAFE]\r\nPUT 1:\r\n+\r\n++leading-plus\r\n+tail\r\n",
        ] {
            let parsed = parse_hashline_patch(patch).expect("pinned PUT body edge must parse");
            let Operation::Put(PutOperation {
                source: PutSource::Text(lines),
                ..
            }) = &parsed.sections[0].operations[0]
            else {
                panic!("expected text PUT");
            };
            assert_eq!(lines.first().map(String::as_str), Some(""));
            if lines.len() > 1 {
                assert_eq!(
                    lines,
                    &["".to_string(), "+leading-plus".into(), "tail".into()]
                );
            }
        }

        for patch in [
            "[a.py#CAFE]\nPUT 1:\n",
            "[a.py#CAFE]\nPUT 1:\n+content\n\nCUT 2",
            "[a.py#CAFE]\r\nPUT 1:\r\ncontent\r\n",
        ] {
            let rejection = parse_hashline_patch(patch).expect_err("unmarked body row must fail");
            assert_eq!(rejection.code, HashlineRejectionCode::ParseError);
            assert!(rejection.message.contains("expected `+<text>`"));
            assert!(rejection.message.contains("bare `+`"));
            assert!(rejection.message.contains("patch line"));
        }
    }

    #[test]
    fn parser_rejects_noncanonical_file_operations() {
        assert!(parse_hashline_patch("[a.rs#CAFE]\nREM 1").is_err());
        assert!(parse_hashline_patch("[a.rs#CAFE]\nMV 1 -> b.rs").is_err());
        assert!(parse_hashline_patch("[a.rs#CAFE]\nREM\nPUT 1:\n+x").is_err());
        assert!(parse_hashline_patch("[a.rs#CAFE]\nMV b.rs\nCUT 1").is_err());
    }

    #[test]
    fn snapshot_lookup_maps_all_store_outcomes_to_resolution_stage() {
        let mut store = SnapshotStore::new();
        let unknown = resolve_snapshot(&mut store, "/not-known", "CAFE").expect_err("unknown");
        assert_eq!(unknown.code, HashlineRejectionCode::UnknownTag);
        assert_eq!(unknown.stage, RejectionStage::Resolution);

        let path = PathBuf::from("/same-tag");
        let first = snapshot(b"shared\none\n", [1]);
        let mut collision = snapshot(b"shared\nother\n", [1]);
        let tag = first.tag.clone();
        collision.tag = tag.clone();
        store.publish(&path, first);
        store.publish(&path, collision);
        let ambiguous = resolve_snapshot(&mut store, &path, &tag).expect_err("collision");
        assert_eq!(store.snapshot_count(), 2);
        assert_eq!(
            store.lookup_state(&path, &tag),
            crate::hashline::snapshot::SnapshotLookup::Ambiguous
        );
        assert_eq!(ambiguous.code, HashlineRejectionCode::AmbiguousTag);
        assert_eq!(ambiguous.stage, RejectionStage::Resolution);
        assert_eq!(
            ambiguous.steering,
            "use apply_patch or another available non-hashline edit surface; re-reading preserves this colliding four-hex tag"
        );
    }

    #[test]
    fn evicted_handles_keep_their_distinct_resolution_code() {
        let mut store = SnapshotStore::new();
        let first_path = PathBuf::from("/evicted-0");
        let first = snapshot(b"row\n", [1]);
        let tag = first.tag.clone();
        store.publish(&first_path, first);
        for index in 1..=MAX_SNAPSHOT_PATHS {
            store.publish(
                PathBuf::from(format!("/evicted-{index}")),
                snapshot(b"row\n", [1]),
            );
        }
        let rejection = resolve_snapshot(&mut store, &first_path, &tag).expect_err("evicted");
        assert_eq!(rejection.code, HashlineRejectionCode::EvictedTag);
        assert_eq!(rejection.stage, RejectionStage::Resolution);
    }

    #[test]
    fn equivalent_candidates_resolve_despite_different_provenance() {
        let mut store = SnapshotStore::new();
        let path = PathBuf::from("/equivalent");
        let first = snapshot(b"one\ntwo\n", [1]);
        let mut reread = snapshot(b"one\ntwo\n", [1]);
        reread.provenance = reread.provenance.with_label("read", "second");
        let tag = first.tag.clone();
        store.publish(&path, first);
        store.publish(&path, reread);
        assert!(resolve_snapshot(&mut store, &path, &tag).is_ok());
    }

    #[test]
    fn same_path_sections_are_resolved_in_pre_request_coordinates() {
        let mut store = SnapshotStore::new();
        let path = PathBuf::from("/pre-request");
        let captured = snapshot(b"one\ntwo\nthree\n", [1, 2, 3]);
        let tag = captured.tag.clone();
        store.publish(&path, captured);
        let patch = parse_hashline_patch(&format!(
            "[/pre-request#{tag}]\nCUT 1\n[/pre-request#{tag}]\nCUT 3"
        ))
        .expect("patch");
        let sections =
            resolve_patch_sections(&mut store, &patch, |requested| Ok(PathBuf::from(requested)))
                .expect("both sections resolve before mutation");
        assert_eq!(sections.len(), 2);
        assert_eq!(
            sections[1].operations[0].address,
            ResolvedAddress::Span(LineSpan { start: 3, end: 3 })
        );
    }

    #[test]
    fn whole_file_operations_require_full_retained_coverage() {
        let mut store = SnapshotStore::new();
        let path = PathBuf::from("/whole-file");
        let partial = snapshot(b"one\ntwo\n", [1]);
        let tag = partial.tag.clone();
        store.publish(&path, partial);
        let patch = parse_hashline_patch(&format!("[/whole-file#{tag}]\nREM")).expect("patch");
        let rejection =
            resolve_patch_sections(&mut store, &patch, |requested| Ok(PathBuf::from(requested)))
                .expect_err("partial read cannot authorize whole-file deletion");
        assert_eq!(rejection.code, HashlineRejectionCode::UnseenLine);
        assert_eq!(rejection.stage, RejectionStage::Eligibility);
    }

    #[test]
    fn unseen_line_rejection_names_fresh_tag_remedy() {
        let contents = (1..=130)
            .map(|line| format!("line_{line} = {line}\n"))
            .collect::<String>();
        let retained = snapshot(contents.as_bytes(), [1, 2]);
        let address = resolve_address(&parse_address("16").unwrap(), &retained).unwrap();
        let rejection =
            check_eligibility(&retained, address).expect_err("line 16 was not retained");

        assert_eq!(rejection.code, HashlineRejectionCode::UnseenLine);
        assert_eq!(
            rejection.message,
            "line 16 was not retained by the tagged read"
        );
        assert_eq!(
            rejection.steering,
            "re-read the file to mint a fresh tag that includes every addressed row and boundary, then retry the edit"
        );
    }

    #[test]
    fn addresses_resolve_against_snapshot_not_live_baseline_length() {
        let retained = snapshot(b"one\ntwo\nthree\n", [2, 3]);
        let resolved = resolve_address(&parse_address("$-1").unwrap(), &retained).unwrap();
        assert_eq!(
            resolved,
            ResolvedAddress::Span(LineSpan { start: 2, end: 2 })
        );

        let baseline = Baseline::from_bytes(b"one\ntwo\nthree\nfour\n".to_vec());
        assert!(matches!(
            verify_exact(&retained, &baseline, resolved),
            VerificationOutcome::Exact
        ));
    }

    #[test]
    fn gaps_require_their_retained_boundary_rows() {
        let retained = snapshot(b"one\ntwo\nthree\n", [2]);
        let gap = resolve_address(&parse_address("<2").unwrap(), &retained).unwrap();
        let rejection = check_eligibility(&retained, gap).expect_err("line one was unseen");
        assert_eq!(rejection.code, HashlineRejectionCode::UnseenLine);
        assert_eq!(rejection.stage, RejectionStage::Eligibility);

        let eof = resolve_address(&parse_address(">$").unwrap(), &retained).unwrap();
        let rejection = check_eligibility(&retained, eof).expect_err("line three was unseen");
        assert_eq!(rejection.code, HashlineRejectionCode::UnseenLine);
    }

    #[test]
    fn one_baseline_is_retained_for_every_section_of_a_canonical_path() {
        let mut cache = BaselineCache::default();
        let path = PathBuf::from("/canonical");
        let first = cache
            .load_once(path.clone(), b"before\n".to_vec())
            .bytes
            .clone();
        let second = cache
            .load_once(path.clone(), b"after\n".to_vec())
            .bytes
            .clone();
        assert_eq!(first, b"before\n");
        assert_eq!(second, b"before\n");
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn anchor_mismatch_is_verification_but_span_mismatch_requests_recovery() {
        let retained = snapshot(b"one\ntwo\nthree\n", [1, 2, 3]);
        let span = resolve_address(&parse_address("2").unwrap(), &retained).unwrap();
        let changed_span = Baseline::from_bytes(b"one\nTWO\nthree\n".to_vec());
        let VerificationOutcome::RecoveryRequired(plan) =
            verify_exact(&retained, &changed_span, span)
        else {
            panic!("addressed record drift must be planned for recovery");
        };
        assert_eq!(plan.old_span, LineSpan { start: 2, end: 2 });

        let gap = resolve_address(&parse_address("<2").unwrap(), &retained).unwrap();
        let changed_anchor = Baseline::from_bytes(b"ONE\ntwo\nthree\n".to_vec());
        let VerificationOutcome::Rejected(rejection) =
            verify_exact(&retained, &changed_anchor, gap)
        else {
            panic!("anchor drift must reject before recovery");
        };
        assert_eq!(rejection.code, HashlineRejectionCode::StaleTag);
        assert_eq!(rejection.stage, RejectionStage::Verification);
    }

    #[test]
    fn exact_verification_compares_terminators_and_unnormalized_bytes() {
        let retained = snapshot(b"one  \r\ntwo\n", [1, 2]);
        let span = resolve_address(&parse_address("1").unwrap(), &retained).unwrap();
        let trailing_space_drift = Baseline::from_bytes(b"one\r\ntwo\n".to_vec());
        assert!(matches!(
            verify_exact(&retained, &trailing_space_drift, span),
            VerificationOutcome::RecoveryRequired(_)
        ));
    }
}
