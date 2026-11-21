//! Draft github_read wire contract pinned by Plexus's build.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const MAX_REPLY_BYTES: u64 = 1_048_576;

// Pagination; native renderers' content/run filters; ordinary repository REST
// filters. Query language, URL/method selection and header keys are not authority
// that a worker GET may delegate to the upstream provider.
#[cfg(unix)]
pub(crate) const WORKER_GET_QUERY_KEYS: &[&str] = &[
    // Shared list pagination.
    "page",
    "per_page",
    // Filters emitted by native content and workflow/run renderers.
    "ref",
    "branch",
    "head_sha",
    "event",
    "actor",
    "status",
    "check_suite_id",
    // Repository-local REST filters; none selects a host, method or principal.
    "state",
    "labels",
    "milestone",
    "assignee",
    "creator",
    "mentioned",
    "since",
    "sort",
    "direction",
    "base",
    "head",
    "path",
    "sha",
];
#[cfg(unix)]
const MAX_WORKER_QUERY_PAIRS: usize = 16;
#[cfg(unix)]
const MAX_WORKER_QUERY_VALUE_BYTES: usize = 4096;

#[derive(Debug)]
#[cfg(unix)]
pub(crate) struct WorkerReadRefusal {
    pub code: &'static str,
    pub key: Option<String>,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields, tag = "action")]
pub(crate) enum Request {
    #[serde(rename = "github.repo.get")]
    Get { repository: String, args: GetArgs },
    #[serde(rename = "github.search.code")]
    Code {
        repository: String,
        args: SearchArgs,
    },
    #[serde(rename = "github.search.issues")]
    Issues {
        repository: String,
        args: SearchArgs,
    },
    #[serde(rename = "github.actions.job_log")]
    JobLog { repository: String, args: LogArgs },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
pub(crate) enum Accept {
    #[default]
    #[serde(rename = "application/vnd.github+json")]
    Json,
    #[serde(rename = "application/vnd.github.raw+json")]
    Raw,
    #[serde(rename = "application/vnd.github.diff")]
    Diff,
    #[serde(rename = "application/vnd.github.patch")]
    Patch,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetArgs {
    pub path: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub query: BTreeMap<String, String>,
    #[serde(default)]
    pub accept: Accept,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchArgs {
    pub q: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub per_page: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogArgs {
    pub job_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<u64>,
}

impl Request {
    /// Decode once at the worker boundary. Keep a slash-containing reference in
    /// its one API segment; no other encoded separator or second decode is valid.
    #[cfg(unix)]
    pub(crate) fn canonicalize_worker_path(&mut self) -> Result<(), WorkerReadRefusal> {
        let Self::Get { args, .. } = self else {
            return Ok(());
        };
        let invalid = |message: &str| WorkerReadRefusal {
            code: "worker_path_invalid",
            key: None,
            message: message.into(),
        };
        if args.path.is_empty() {
            return Ok(());
        }
        let mut parts = Vec::new();
        for part in args.path.split('/') {
            let mut bytes = Vec::new();
            let mut input = part.as_bytes().iter().copied();
            while let Some(byte) = input.next() {
                if byte == b'%' {
                    let high = input.next().and_then(|byte| (byte as char).to_digit(16));
                    let low = input.next().and_then(|byte| (byte as char).to_digit(16));
                    let (Some(high), Some(low)) = (high, low) else {
                        return Err(invalid("worker path has malformed percent encoding"));
                    };
                    bytes.push((high * 16 + low) as u8);
                } else {
                    bytes.push(byte);
                }
            }
            let decoded = String::from_utf8(bytes)
                .map_err(|_| invalid("worker path must decode as UTF-8"))?;
            if decoded.contains('%') || decoded.chars().any(char::is_control) {
                return Err(invalid(
                    "worker path forbids double encoding and control characters",
                ));
            }
            parts.push(decoded);
        }
        if parts[0].eq_ignore_ascii_case("graphql") {
            return Err(WorkerReadRefusal {
                code: "worker_graphql_not_supported",
                key: None,
                message: "worker reads do not support GraphQL paths".into(),
            });
        }
        let names: Vec<_> = parts.iter().map(String::as_str).collect();
        let reference = match names.as_slice() {
            ["branches", _]
            | ["branches", _, "protection"]
            | ["branches", _, "protection", "required_status_checks"]
            | ["commits", _]
            | ["commits", _, "status" | "statuses" | "check-runs"] => Some(1),
            ["releases", "tags", _] => Some(2),
            _ => None,
        };
        for (index, part) in parts.iter().enumerate() {
            if part.split('/').any(|part| matches!(part, "" | "." | ".."))
                || (part.contains('/') && reference != Some(index))
            {
                return Err(invalid(
                    "worker path separators and dot segments cannot change endpoint structure",
                ));
            }
        }
        let canonical = parts
            .iter()
            .map(|part| part.replace('/', "%2F"))
            .collect::<Vec<_>>()
            .join("/");
        validate_path(&canonical).map_err(|message| invalid(&message))?;
        args.path = canonical;
        Ok(())
    }

    /// Extra policy for the new worker broker only. Existing head/operator
    /// executor paths retain their query behavior and call validate instead.
    #[cfg(unix)]
    pub(crate) fn validate_worker_query(&self) -> Result<(), WorkerReadRefusal> {
        let Self::Get { args, .. } = self else {
            return Ok(());
        };
        if args
            .path
            .split('/')
            .next()
            .is_some_and(|part| part.eq_ignore_ascii_case("graphql"))
        {
            return Err(WorkerReadRefusal {
                code: "worker_graphql_not_supported",
                key: None,
                message: "worker reads do not support GraphQL paths".into(),
            });
        }
        if args.query.len() > MAX_WORKER_QUERY_PAIRS {
            return Err(WorkerReadRefusal {
                code: "worker_query_too_many_pairs",
                key: None,
                message: format!("worker GET permits at most {MAX_WORKER_QUERY_PAIRS} query pairs"),
            });
        }
        for (key, value) in &args.query {
            if !WORKER_GET_QUERY_KEYS.contains(&key.as_str()) {
                return Err(WorkerReadRefusal {
                    code: "worker_query_key_not_allowed",
                    key: Some(key.clone()),
                    message: format!(
                        "worker GET query key {key:?} is not allowed. Allowed keys: {}",
                        WORKER_GET_QUERY_KEYS.join(", ")
                    ),
                });
            }
            if value.len() > MAX_WORKER_QUERY_VALUE_BYTES {
                return Err(WorkerReadRefusal { code:"worker_query_value_too_long", key:Some(key.clone()), message:format!("worker GET query value for {key:?} exceeds {MAX_WORKER_QUERY_VALUE_BYTES} bytes") });
            }
            if value.chars().any(char::is_control) {
                return Err(WorkerReadRefusal {
                    code: "worker_query_value_invalid",
                    key: Some(key.clone()),
                    message: format!(
                        "worker GET query value for {key:?} contains control characters"
                    ),
                });
            }
            // The upstream provider constructs the final URL. Do not depend on
            // query escaping to prevent a worker value from adding/truncating
            // pairs, including a second decode of an encoded delimiter.
            if value.contains(['&', '=', '#', '%']) {
                return Err(WorkerReadRefusal {code:"worker_query_value_ambiguous",key:Some(key.clone()),message:format!("worker GET query value for {key:?} must not contain &, =, # or percent escapes")});
            }
        }
        Ok(())
    }

    pub(crate) fn repository(&self) -> &str {
        match self {
            Self::Get { repository, .. }
            | Self::Code { repository, .. }
            | Self::Issues { repository, .. }
            | Self::JobLog { repository, .. } => repository,
        }
    }
    pub(crate) fn validate(&self) -> Result<(), String> {
        let repository = match self {
            Self::Get { repository, .. }
            | Self::Code { repository, .. }
            | Self::Issues { repository, .. }
            | Self::JobLog { repository, .. } => repository,
        };
        if super::canonical_repository_key(repository).as_deref() != Some(repository.as_str()) {
            return Err("repository must be canonical owner/name".into());
        }
        match self {
            Self::Get { args, .. } => {
                let path = &args.path;
                validate_path(path)?;
                if matches!(args.accept, Accept::Diff | Accept::Patch) {
                    let parts: Vec<_> = path.split('/').collect();
                    if parts.len() != 2
                        || !((parts[0] == "pulls"
                            && parts[1].parse::<u64>().is_ok_and(|id| id > 0))
                            || (parts[0] == "compare" && !parts[1].is_empty()))
                    {
                        return Err(
                            "diff/patch Accept is limited to numbered pulls and compare endpoints"
                                .into(),
                        );
                    }
                }
            }
            Self::Code { args, .. } | Self::Issues { args, .. } => {
                validate_search(&args.q)?;
                if args.page == Some(0) || args.per_page.is_some_and(|size| size == 0 || size > 100)
                {
                    return Err("search page must be positive and per_page must be 1..100".into());
                }
            }
            Self::JobLog { args, .. } => {
                if args.job_id == 0
                    || args
                        .length
                        .is_some_and(|size| size == 0 || size > MAX_REPLY_BYTES)
                    || args
                        .offset
                        .unwrap_or(0)
                        .checked_add(args.length.unwrap_or(0))
                        .is_none()
                {
                    return Err(
                        "job log needs a positive job_id and a byte range of at most 1048576 bytes"
                            .into(),
                    );
                }
            }
        }
        Ok(())
    }
}

pub(super) fn validate_path(path: &str) -> Result<(), String> {
    let lower = path.to_ascii_lowercase();
    let parts: Vec<_> = path.split('/').collect();
    // Only these endpoints interpret one segment as a branch, tag or commit
    // ref. Encoded slashes cannot change endpoint structure or repository scope.
    let name_index = match parts.as_slice() {
        ["branches", _]
        | ["branches", _, "protection"]
        | ["branches", _, "protection", "required_status_checks"]
        | ["commits", _]
        | ["commits", _, "status" | "statuses" | "check-runs"] => Some(1),
        ["releases", "tags", _] => Some(2),
        _ => None,
    };
    if !path.is_empty()
        && (path.starts_with('/')
            || path.contains("//")
            || path.contains(['\\', '?', '#', ':'])
            || path.split('/').any(|part| matches!(part, "" | "." | ".."))
            || path.bytes().any(|byte| byte.is_ascii_control())
            || parts.iter().enumerate().any(|(index, part)| {
                let part = part.to_ascii_lowercase();
                part.contains("%2f")
                    && (name_index != Some(index)
                        || part
                            .replace("%2f", "/")
                            .split('/')
                            .any(|part| matches!(part, "" | "." | "..")))
            })
            || ["%5c", "%2e", "%3a", "%3f", "%23", "%25"]
                .iter()
                .any(|encoded| lower.contains(encoded)))
    {
        return Err(
            "path must be repository-relative; only the empty string names the repository root"
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn validate_search(q: &str) -> Result<(), String> {
    let qualifiers = regex::Regex::new(r"(?i)(?:^|[^a-z0-9_])(?:repo|org|user)\s*:").unwrap();
    if q.trim().is_empty() || qualifiers.is_match(q) {
        return Err("search requires a query without repository/org/user qualifiers; Plexus adds the bound repository".into());
    }
    let boolean = regex::Regex::new(r"(?i)\b(?:OR|AND|NOT)\b").unwrap();
    if q.contains(['(', ')']) || boolean.is_match(q) {
        return Err("worker search does not allow boolean operators or parentheses; Plexus appends a repository constraint".into());
    }
    if q.contains('\\') {
        return Err("worker search does not allow backslashes: GitHub search syntaxes have different escape rules".into());
    }
    let mut quoted = false;
    for character in q.chars() {
        if character == '"' {
            quoted = !quoted;
        }
    }
    if quoted {
        return Err("worker search requires balanced double quotes before the appended repository constraint".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GetReply {
    pub status: u16,
    pub body: Value,
    pub next_page: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchReply {
    pub total_count: u64,
    pub incomplete_results: bool,
    pub items: Vec<Value>,
    pub next_page: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct LogReply {
    pub body: String,
    pub offset: u64,
    pub length: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Reply {
    Get(GetReply),
    Search(SearchReply),
    JobLog(LogReply),
}

impl Reply {
    pub(crate) fn bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        match self {
            Self::Get(GetReply {
                body: Value::String(text),
                ..
            })
            | Self::JobLog(LogReply { body: text, .. }) => Ok(text.as_bytes().to_vec()),
            Self::Get(reply) => serde_json::to_vec(&reply.body),
            Self::Search(reply) => serde_json::to_vec(reply),
        }
    }
    pub(crate) fn next_page(&self) -> Option<u64> {
        match self {
            Self::Get(reply) => reply.next_page,
            Self::Search(reply) => reply.next_page,
            Self::JobLog(_) => None,
        }
    }
}
