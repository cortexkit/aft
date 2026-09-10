use std::fmt;
#[cfg(test)]
use std::ops::Range;

use serde_json::Value;

use super::blocks::{
    BlockBuildError, BlockBuilder, BlockReply, PageRequest, MAX_BLOCK_DEPTH, MAX_OFFSET,
    MAX_PUBLIC_TOP_K,
};
#[cfg(test)]
use super::blocks::{StabilityUnit, BLOCK_DEPTHS};

pub const DEFAULT_TOP_K: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedPageRequest {
    offset: usize,
    top_k: usize,
}

impl ValidatedPageRequest {
    pub fn offset(self) -> usize {
        self.offset
    }

    pub fn top_k(self) -> usize {
        self.top_k
    }

    pub fn interval_end(self) -> u64 {
        (self.offset as u64).saturating_add(self.top_k as u64)
    }

    fn as_block_request(self) -> PageRequest {
        PageRequest {
            offset: self.offset,
            top_k: self.top_k,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PagingValidationError {
    field: &'static str,
    message: String,
}

impl PagingValidationError {
    pub const fn code(&self) -> &'static str {
        "invalid_request"
    }

    pub const fn field(&self) -> &'static str {
        self.field
    }
}

impl fmt::Display for PagingValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for PagingValidationError {}

#[derive(Debug)]
pub enum PagingError {
    InvalidRequest(PagingValidationError),
    Build(BlockBuildError),
}

impl fmt::Display for PagingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(error) => error.fmt(formatter),
            Self::Build(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for PagingError {}

impl From<PagingValidationError> for PagingError {
    fn from(error: PagingValidationError) -> Self {
        Self::InvalidRequest(error)
    }
}

impl From<BlockBuildError> for PagingError {
    fn from(error: BlockBuildError) -> Self {
        Self::Build(error)
    }
}

fn invalid(field: &'static str, message: impl Into<String>) -> PagingValidationError {
    PagingValidationError {
        field,
        message: message.into(),
    }
}

fn parse_integer(
    value: Option<&Value>,
    field: &'static str,
    default: usize,
    minimum: usize,
    maximum: usize,
    maximum_name: &'static str,
) -> Result<usize, PagingValidationError> {
    let Some(value) = value else {
        return Ok(default);
    };
    let Some(integer) = value.as_i64() else {
        return Err(invalid(
            field,
            format!("{field} must be an integer between {minimum} and {maximum_name} ({maximum})"),
        ));
    };
    if integer < minimum as i64 || integer > maximum as i64 {
        return Err(invalid(
            field,
            format!("{field} must be between {minimum} and {maximum_name} ({maximum})"),
        ));
    }
    Ok(integer as usize)
}

/// Validates both public bounds before constructing a request that can compute an interval.
pub fn parse_public_page_request(
    params: &Value,
) -> Result<ValidatedPageRequest, PagingValidationError> {
    let offset = parse_integer(
        params.get("offset"),
        "offset",
        0,
        0,
        MAX_OFFSET,
        "MAX_OFFSET",
    )?;
    let top_k_value = match (params.get("topK"), params.get("top_k")) {
        (Some(_), Some(_)) => {
            return Err(invalid("topK", "topK and top_k cannot both be supplied"));
        }
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    };
    let top_k = parse_integer(
        top_k_value,
        "topK",
        DEFAULT_TOP_K,
        1,
        MAX_PUBLIC_TOP_K,
        "MAX_TOP_K",
    )?;
    Ok(ValidatedPageRequest { offset, top_k })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopState {
    S1MoreAtDepth,
    S2Exhausted,
    S3DepthCap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopConditions {
    pub interval_satisfied: bool,
    pub lanes_exhausted: bool,
    pub at_depth_cap: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnreachableLoopExit;

impl fmt::Display for UnreachableLoopExit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(
            "the escalation loop cannot exit below the depth cap while its interval is unsatisfied and candidates remain",
        )
    }
}

impl std::error::Error for UnreachableLoopExit {}

/// When conditions coincide, selects one deterministic state by prioritizing
/// exhaustion, then the depth cap, then interval satisfaction.
pub fn select_stop_state(conditions: StopConditions) -> Result<StopState, UnreachableLoopExit> {
    if conditions.lanes_exhausted {
        return Ok(StopState::S2Exhausted);
    }
    if conditions.at_depth_cap {
        return Ok(StopState::S3DepthCap);
    }
    if conditions.interval_satisfied {
        return Ok(StopState::S1MoreAtDepth);
    }
    Err(UnreachableLoopExit)
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchPage {
    pub reply: BlockReply,
    pub stop_state: StopState,
    pub stability_void: bool,
}

impl SearchPage {
    pub fn shown(&self) -> usize {
        self.reply.page.len()
    }

    pub fn total_at_stop(&self) -> usize {
        self.reply.canonical_list.len()
    }
}

pub fn serve_public_page(
    builder: &BlockBuilder,
    request: ValidatedPageRequest,
) -> Result<SearchPage, PagingError> {
    let interval_end = request.interval_end();
    let reply = builder.build_for_request(request.as_block_request())?;
    let stop_state = select_stop_state(StopConditions {
        interval_satisfied: (reply.canonical_list.len() as u64) >= interval_end,
        lanes_exhausted: reply.lanes_exhausted,
        at_depth_cap: reply.retrieval_depth == MAX_BLOCK_DEPTH,
    })
    .expect("BlockBuilder can only return at a valid escalation-loop exit");
    Ok(SearchPage {
        reply,
        stop_state,
        stability_void: false,
    })
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ReferenceList {
    pub stability_units: Vec<StabilityUnit>,
    pub retrieval_depth: usize,
    pub depth_tier: usize,
    pub lanes_exhausted: bool,
}

/// Builds a canonical prefix for internal comparison tests; unlike public requests,
/// this helper may process an interval containing more than MAX_TOP_K items.
#[cfg(test)]
pub(crate) fn build_l(
    builder: &BlockBuilder,
    interval: Range<u64>,
) -> Result<ReferenceList, BlockBuildError> {
    assert_eq!(interval.start, 0, "build_l accepts only canonical prefixes");
    let target = interval.end;
    let mut tier = BLOCK_DEPTHS
        .iter()
        .position(|depth| (*depth as u64) >= target)
        .unwrap_or(BLOCK_DEPTHS.len() - 1);

    loop {
        let reply = builder.build_at_depth(BLOCK_DEPTHS[tier])?;
        let enough = (reply.canonical_list.len() as u64) >= target;
        if enough || reply.lanes_exhausted || tier + 1 == BLOCK_DEPTHS.len() {
            return Ok(ReferenceList {
                stability_units: reply
                    .canonical_list
                    .stability_units()
                    .into_iter()
                    .take(target.min(usize::MAX as u64) as usize)
                    .collect(),
                retrieval_depth: reply.retrieval_depth,
                depth_tier: reply.depth_tier,
                lanes_exhausted: reply.lanes_exhausted,
            });
        }
        tier += 1;
    }
}
