//! Encode and decode the draft github_read contract pinned by Plexus's build.

use super::*;

pub(super) fn execute_read(
    context: &ExecutionContext<'_>,
    request: &ReadRequest,
) -> Result<ReadReply, ReadError> {
    execute_read_with(context, request, &mut |request| {
        relay_client::read_request(context.relay, request)
    })
}

/// Only the ticket and a validated read request cross the daemon boundary.
/// Plexus chooses the bot from the stamped route, never from the request body.
pub(super) fn execute_read_with(
    context: &ExecutionContext<'_>,
    request: &ReadRequest,
    send: &mut impl FnMut(&Value) -> Result<Value, String>,
) -> Result<ReadReply, ReadError> {
    if context.manifest.manifest_version < 20 {
        return Err(ReadError::Refused(
            "governed reads require manifest v20".into(),
        ));
    }
    let wire = wire_request(&context.binding.repo, request).map_err(ReadError::Refused)?;
    let value =
        serde_json::to_value(&wire).map_err(|error| ReadError::Refused(error.to_string()))?;
    let reply = send(&value)
        .map_err(|error| ReadError::Refused(format!("governed read transport failed: {error}")))?;
    decode_reply(&wire, &reply)
}

pub(super) fn wire_request(repository: &str, request: &ReadRequest) -> Result<WireRequest, String> {
    let repository = repository.to_string();
    let wire = match request {
        ReadRequest::Get { path, accept } => {
            let (endpoint, encoded_query) = path.split_once('?').unwrap_or((path, ""));
            let root = format!("/repos/{repository}");
            let relative = if endpoint == root {
                ""
            } else {
                let prefix = format!("{root}/");
                endpoint.strip_prefix(&prefix).ok_or_else(|| format!("worker API reads are bound to {root}; no other repository or URL is allowed"))?
            };
            // A trailing slash is not the root spelling. Only the exact root
            // endpoint becomes the empty path admitted by the read contract.
            if relative.is_empty() && endpoint != root {
                return Err("a lone / is not a repository-relative path".into());
            }
            let mut query = BTreeMap::new();
            for (key, value) in url::form_urlencoded::parse(encoded_query.as_bytes()) {
                if key.is_empty() || query.insert(key.into_owned(), value.into_owned()).is_some() {
                    return Err(
                        "worker GET query must be a flat object with unique non-empty keys".into(),
                    );
                }
            }
            let accept = match accept.as_deref() {
                None => Accept::Json,
                Some("raw") => Accept::Raw,
                Some("diff") => Accept::Diff,
                Some("patch") => Accept::Patch,
                _ => return Err("unsupported worker read Accept type".into()),
            };
            WireRequest::Get {
                repository,
                args: GetArgs {
                    path: relative.into(),
                    query,
                    accept,
                },
            }
        }
        ReadRequest::Search {
            action,
            query,
            page,
            per_page,
        } => {
            let args = SearchArgs {
                q: query.clone(),
                page: Some(*page),
                per_page: Some(*per_page),
            };
            match action.as_str() {
                "github.search.code" => WireRequest::Code { repository, args },
                "github.search.issues" => WireRequest::Issues { repository, args },
                _ => return Err(format!("unsupported worker search action {action}")),
            }
        }
        ReadRequest::JobLog { job_id, range } => WireRequest::JobLog {
            repository,
            args: LogArgs {
                job_id: *job_id,
                offset: range.as_ref().map(|range| range.offset),
                length: range.as_ref().map(|range| range.length),
            },
        },
    };
    wire.validate()?;
    Ok(wire)
}

fn malformed(code: &str, reason: impl std::fmt::Display) -> ReadError {
    ReadError::Refused(format!(
        "malformed Plexus github_read reply (code {code}): {reason}"
    ))
}

pub(super) fn decode_reply(request: &WireRequest, value: &Value) -> Result<ReadReply, ReadError> {
    let is_error = value["isError"]
        .as_bool()
        .ok_or_else(|| malformed("missing", "isError must be a boolean"))?;
    if is_error {
        if value.get("result").is_some() {
            return Err(malformed("ambiguous", "error reply also contains result"));
        }
        let error = value
            .get("error")
            .ok_or_else(|| malformed("missing", "error member is absent"))?;
        let code = error["code"]
            .as_str()
            .ok_or_else(|| malformed("missing", "error.code must be a string"))?;
        let transmission = error["transmission"]
            .as_str()
            .ok_or_else(|| malformed(code, "transmission is missing"))?;
        let message = error["message"]
            .as_str()
            .ok_or_else(|| malformed(code, "message must be a string"))?;
        let known_refusal = matches!(
            code,
            "read_budget_exhausted"
                | "repository_not_visible"
                | "endpoint_not_allowed"
                | "response_too_large"
                | "agent_identity_required"
        );
        if (known_refusal && transmission != "not_sent")
            || (code == "vendor_error" && transmission != "sent")
        {
            return Err(malformed(code, "unexpected transmission"));
        }
        return Err(match code {
            "read_budget_exhausted" => ReadError::Budget { retry_after_ms: error["retry_after_ms"].as_u64().ok_or_else(|| malformed(code, "retry_after_ms must be an unsigned integer"))? },
            "repository_not_visible" => ReadError::Refused(format!("repository is not visible to the scoped bot; the read was not sent: {message}")),
            "endpoint_not_allowed" => {
                let field = error["field"].as_str().ok_or_else(|| malformed(code, "field must be a string"))?;
                ReadError::Refused(format!("worker read endpoint or field {field} is not allowed; the read was not sent: {message}"))
            }
            "response_too_large" => ReadError::Refused(format!("worker read response exceeds 1 MiB; narrow the request: {message}")),
            "agent_identity_required" => ReadError::Refused(format!("worker read requires a bot identity on the stamped route; restart the scoped session: {message}")),
            "vendor_error" => {
                let status = error["status"].as_u64().filter(|status| (100..=599).contains(status) && !(200..=299).contains(status)).ok_or_else(|| malformed(code, "invalid HTTP status"))?;
                ReadError::Vendor { status: status as u16, message: message.into() }
            }
            _ => malformed(code, "unknown error code"),
        });
    }
    if value.get("error").is_some() {
        return Err(malformed("ambiguous", "success reply also contains error"));
    }
    let result = value
        .get("result")
        .ok_or_else(|| malformed("missing", "result member is absent"))?;
    let decode_error = |error| malformed("success", error);
    let reply = match request {
        WireRequest::Get { args, .. } => {
            require_next_page(result)?;
            let reply: GetReply = serde_json::from_value(result.clone()).map_err(decode_error)?;
            if !(200..=299).contains(&reply.status) {
                return Err(malformed("success", "GET success status is not 2xx"));
            }
            if args.accept != Accept::Json && !reply.body.is_string() {
                return Err(malformed("success", "raw/diff/patch body must be a string"));
            }
            ReadReply::Get(reply)
        }
        WireRequest::Code { args, .. } | WireRequest::Issues { args, .. } => {
            require_next_page(result)?;
            let reply: SearchReply =
                serde_json::from_value(result.clone()).map_err(decode_error)?;
            if reply.items.len() as u64 > args.per_page.unwrap_or(30) {
                return Err(malformed("success", "search returned more than per_page"));
            }
            ReadReply::Search(reply)
        }
        WireRequest::JobLog { args, .. } => {
            let reply: LogReply = serde_json::from_value(result.clone()).map_err(decode_error)?;
            // `offset`, `length` and `total_bytes` count raw log bytes in
            // storage, while `body` is display text: Plexus strips ANSI escape
            // codes, redacts the storage URL and credential, and decodes UTF-8
            // lossily (a multibyte character split by the range edge becomes
            // U+FFFD). So `body.len()` may be smaller or larger than `length`
            // and is never compared with it; only the raw byte range is checked
            // here. The body itself stays bounded by the REPLY_BYTES check below.
            if reply.length > args.length.unwrap_or(read_contract::MAX_REPLY_BYTES)
                || args.offset.is_some_and(|offset| offset != reply.offset)
                || reply
                    .offset
                    .checked_add(reply.length)
                    .is_none_or(|end| end > reply.total_bytes)
            {
                return Err(malformed(
                    "success",
                    "job log byte range does not match its request",
                ));
            }
            ReadReply::JobLog(reply)
        }
    };
    let size = reply.bytes().map_err(decode_error)?.len();
    if size > REPLY_BYTES {
        return Err(ReadError::Refused(format!(
            "governed read response is {size} bytes; cap is {REPLY_BYTES} bytes (1 MiB); narrow the request"
        )));
    }
    Ok(reply)
}

fn require_next_page(result: &Value) -> Result<(), ReadError> {
    if !result
        .get("next_page")
        .is_some_and(|page| page.is_null() || page.as_u64().is_some_and(|page| page > 0))
    {
        return Err(malformed(
            "success",
            "next_page must be a positive integer or null",
        ));
    }
    Ok(())
}
