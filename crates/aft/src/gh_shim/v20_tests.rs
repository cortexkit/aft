use super::*;

fn args(raw: &[&str]) -> Vec<OsString> {
    raw.iter().map(OsString::from).collect()
}
fn manifest() -> Manifest {
    let manifest: Manifest = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v20-manifest.json"
    ))
    .unwrap();
    manifest.validate().unwrap();
    manifest
}
fn planned(raw: &[&str]) -> Plan {
    plan(&args(raw), "cortexkit/aft").unwrap()
}

struct TestJqPathGuard {
    original_path: Option<OsString>,
    #[cfg(windows)]
    original_pathext: Option<OsString>,
}

impl TestJqPathGuard {
    fn prepend(directory: &std::path::Path) -> Self {
        let original_path = std::env::var_os("PATH");
        let mut paths = vec![directory.to_path_buf()];
        if let Some(path) = &original_path {
            paths.extend(std::env::split_paths(path));
        }
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());

        #[cfg(windows)]
        let original_pathext = {
            let original = std::env::var_os("PATHEXT");
            std::env::set_var("PATHEXT", ".COM;.EXE;.BAT;.CMD");
            original
        };

        Self {
            original_path,
            #[cfg(windows)]
            original_pathext,
        }
    }
}

impl Drop for TestJqPathGuard {
    fn drop(&mut self) {
        match &self.original_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        #[cfg(windows)]
        match &self.original_pathext {
            Some(pathext) => std::env::set_var("PATHEXT", pathext),
            None => std::env::remove_var("PATHEXT"),
        }
    }
}

fn write_jq_fixture(directory: &std::path::Path) {
    #[cfg(windows)]
    {
        // Model text-mode jq output and an older jq that lacks the binary flag.
        std::fs::write(directory.join("output.bin"), b"A\r\r\nB\r\n").unwrap();
        std::fs::write(
            directory.join("jq.cmd"),
            "@echo off\r\nif \"%~1\"==\"--binary\" exit /b 2\r\nmore >nul\r\ntype \"%~dp0\\output.bin\"\r\n",
        )
        .unwrap();
    }

    #[cfg(unix)]
    {
        let jq = directory.join("jq");
        crate::executable_cache::write_executable(
            &jq,
            b"#!/bin/sh\ncat >/dev/null\nprintf 'A\\r\\nB\\n'\n",
        )
        .unwrap();
    }
}
fn reply(value: Value) -> ReadReply {
    ReadReply::Get(GetReply {
        status: 200,
        body: value,
        next_page: None,
    })
}

#[cfg(unix)]
#[test]
fn v20_worker_diff_reader_closing_after_first_line_is_quiet() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    struct FirstLineWriter {
        stream: UnixStream,
        closed: std::sync::mpsc::Receiver<()>,
        writes: usize,
    }
    impl Write for FirstLineWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.writes == 1 {
                let end = bytes.iter().position(|byte| *byte == b'\n').unwrap() + 1;
                self.stream.write_all(&bytes[..end])?;
                self.closed.recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(end)
            } else {
                self.stream.write(bytes)
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }

    let (writer, reader) = UnixStream::pair().unwrap();
    let (closed, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        drop(reader);
        closed.send(()).unwrap();
        line
    });
    let mut output = FirstLineWriter {
        stream: writer,
        closed: receive,
        writes: 0,
    };
    let result = render::run(
        &planned(&["pr", "diff", "285"]),
        &mut |_| {
            Ok(reply(json!(
                "diff --git a/file b/file\n--- a/file\n+++ b/file\n"
            )))
        },
        &mut output,
    );
    assert_eq!(reader.join().unwrap(), "diff --git a/file b/file\n");
    assert_eq!(output.writes, 2, "stop at the first failed write");
    assert!(
        matches!(result, Ok(0)),
        "closed reader must be quiet: {result:?}"
    );
}

#[test]
fn v20_worker_diff_real_output_errors_are_reported() {
    struct FullDisk;
    impl Write for FullDisk {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let error = render::run(
        &planned(&["pr", "diff", "285"]),
        &mut |_| Ok(reply(json!("diff\n"))),
        &mut FullDisk,
    )
    .unwrap_err();
    assert_eq!(error.status(), 1);
    assert_eq!(error.message("bot"), "disk full");
}

#[test]
fn v20_worker_read_oversize_names_size_and_cap() {
    let wire = executor::wire_request(
        "cortexkit/aft",
        &ReadRequest::Get {
            path: "/repos/cortexkit/aft/pulls/434".into(),
            accept: Some("diff".into()),
        },
    )
    .unwrap();
    let error = executor::decode_reply(
        &wire,
        &json!({"isError":false,"result":{
            "status":200,"body":"x".repeat(1_048_577),"next_page":null
        }}),
    )
    .unwrap_err();
    assert_eq!(error.status(), 86);
    assert_eq!(
        error.message("bot"),
        "governed read response is 1048577 bytes; cap is 1048576 bytes (1 MiB); narrow the request"
    );
}
fn log_reply(body: &[u8], offset: u64, total_bytes: u64) -> ReadReply {
    ReadReply::JobLog(LogReply {
        body: String::from_utf8(body.to_vec()).unwrap(),
        offset,
        length: body.len() as u64,
        total_bytes,
    })
}
fn run_json(raw: &[&str], responses: Vec<Value>) -> (Value, Vec<ReadRequest>, i32) {
    let plan = planned(raw);
    let mut responses = responses.into_iter();
    let mut calls = Vec::new();
    let mut output = Vec::new();
    let status = render::run(
        &plan,
        &mut |request| {
            calls.push(request.clone());
            let value = responses.next().expect("unexpected request");
            let result = match request {
                ReadRequest::Search { .. } => json!({"total_count":value["items"].as_array().unwrap().len(),"incomplete_results":false,"items":value["items"],"next_page":null}),
                _ => json!({"status":200,"body":value,"next_page":null}),
            };
            let wire = executor::wire_request("cortexkit/aft", request).unwrap();
            executor::decode_reply(&wire, &json!({"result":result,"isError":false}))
        },
        &mut output,
    )
    .unwrap();
    assert!(responses.next().is_none());
    (serde_json::from_slice(&output).unwrap(), calls, status)
}

#[test]
fn v20_worker_reads_admit_the_reviewed_verbs() {
    let reviewed = [
        "api",
        "run list",
        "run view",
        "pr list",
        "pr view",
        "pr diff",
        "pr files",
        "pr checks",
        "issue list",
        "issue view",
        "issue comments",
        "workflow list",
        "workflow view",
        "release list",
        "release view",
        "repo view",
        "search code",
        "search issues",
    ];
    assert_eq!(TUPLES.len(), reviewed.len());
    assert_eq!(
        TUPLES
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>(),
        reviewed
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
    );
    for command in reviewed {
        let mut raw: Vec<_> = command.split_whitespace().collect();
        match command {
            "api" => raw.push("repos/cortexkit/aft/issues"),
            "run view" | "pr view" | "pr diff" | "pr files" | "pr checks" | "issue view"
            | "issue comments" => raw.push("42"),
            "workflow view" => raw.push("ci.yml"),
            "search code" | "search issues" => raw.push("needle"),
            _ => {}
        }
        assert!(plan(&args(&raw), "cortexkit/aft").is_ok(), "{raw:?}");
    }
}

#[test]
fn v20_get_only_guard_refuses_implicit_and_explicit_writes() {
    for suffix in [
        vec!["-f", "title=x"],
        vec!["-F", "title=x"],
        vec!["--field=title=x"],
        vec!["--raw-field", "title=x"],
        vec!["-X", "POST"],
        vec!["-XPUT"],
        vec!["--method=PATCH"],
        vec!["-X", "DELETE"],
        vec!["-X", "HEAD"],
    ] {
        let mut raw = vec!["api"];
        raw.extend(suffix);
        raw.push("repos/cortexkit/aft/issues");
        let error = plan(&args(&raw), "cortexkit/aft").unwrap_err();
        assert!(error.contains("GET only"), "{raw:?}: {error}");
    }
    let plan = planned(&[
        "api",
        "-X",
        "GET",
        "-f",
        "title=x",
        "repos/cortexkit/aft/issues",
    ]);
    assert_eq!(
        plan.api,
        Some(ReadRequest::Get {
            path: "/repos/cortexkit/aft/issues?title=x".into(),
            accept: None
        })
    );
}

#[test]
fn v20_bound_repo_guard_refuses_other_repositories_hosts_and_graphql() {
    for endpoint in [
        "repos/cortexkit/other/issues",
        "/repos/elsewhere/aft/issues",
        "/repos/cortexkit/aft2/issues",
        "https://api.github.com/repos/cortexkit/aft/issues",
        "https://evil.example/repos/cortexkit/aft/issues",
        "//api.github.com/repos/cortexkit/aft/issues",
        "user",
        "/orgs/cortexkit/repos",
        "graphql",
        "/graphql",
        "/repos/cortexkit/aft/../other/issues",
        "/repos/cortexkit/aft/%2e%2e/other",
        "/repos/cortexkit/aft/issues#fragment",
        "/repos/cortexkit/aft/issues\\other",
    ] {
        assert!(
            plan(&args(&["api", endpoint]), "cortexkit/aft").is_err(),
            "{endpoint}"
        );
    }
    for endpoint in [
        "repos/cortexkit/aft/issues",
        "/repos/cortexkit/aft/pulls/42",
        "/repos/cortexkit/aft/actions/runs?per_page=5",
    ] {
        assert!(
            plan(&args(&["api", endpoint]), "cortexkit/aft").is_ok(),
            "{endpoint}"
        );
    }
}

#[test]
fn v20_input_and_unsafe_headers_are_always_refused() {
    for suffix in [
        vec!["--input", "payload.json"],
        vec!["--input=-"],
        vec!["-XGET", "--input=x"],
        vec!["--hostname", "github.example"],
        vec!["--hostname", "evil.example", "--hostname", "api.github.com"],
        vec!["-H", "Authorization: token fake"],
        vec!["-H", "Accept: application/vnd.github.raw"],
        vec!["-H", "Accept: diff"],
        vec!["-XGET", "-F", "file=@secret"],
    ] {
        let mut raw = vec!["api", "repos/cortexkit/aft/issues"];
        raw.extend(suffix);
        assert!(plan(&args(&raw), "cortexkit/aft").is_err(), "{raw:?}");
    }
}

#[test]
fn v20_diff_accept_is_limited_to_pulls_and_compare() {
    for path in [
        "repos/cortexkit/aft/pulls/42",
        "/repos/cortexkit/aft/compare/main...topic",
    ] {
        for format in ["diff", "patch"] {
            let header = format!("Accept: application/vnd.github.{format}");
            assert!(
                matches!(planned(&["api", path, "-H", &header]).api, Some(ReadRequest::Get { accept: Some(found), .. }) if found == format)
            );
        }
    }
    for path in [
        "repos/cortexkit/aft/pulls",
        "repos/cortexkit/aft/pulls/42/files",
        "repos/cortexkit/aft/compare",
        "repos/cortexkit/aft/issues/42",
    ] {
        assert!(plan(
            &args(&["api", path, "-H", "Accept: patch"]),
            "cortexkit/aft"
        )
        .is_err());
    }
}

#[test]
fn v20_unqualified_search_uses_named_actions_without_adding_repo_in_the_shim() {
    for (verb, action) in [
        ("code", "github.search.code"),
        ("issues", "github.search.issues"),
    ] {
        let (value, calls, _) = run_json(
            &["search", verb, "needle", "--json", "url", "--limit", "1"],
            vec![json!({"items":[{"html_url":"https://github.com/cortexkit/aft/issues/42"}]})],
        );
        assert_eq!(
            value,
            json!([{"url":"https://github.com/cortexkit/aft/issues/42"}])
        );
        let ReadRequest::Search {
            action: found,
            query,
            ..
        } = &calls[0]
        else {
            panic!("not a named search");
        };
        assert_eq!(found, action);
        assert!(!query.contains("repo:"));
    }
    for query in [
        "needle repo:elsewhere/repo",
        "repo:cortexkit/aft needle",
        "needle OR org:other",
        "needle user:other",
        "(REPO:other/repo)",
        "\"repo:other/repo\"",
        "-repo:other/repo",
    ] {
        assert!(
            plan(&args(&["search", "code", query]), "cortexkit/aft").is_err(),
            "{query}"
        );
    }
}

#[test]
fn v20_three_megabyte_job_log_pages_into_one_byte_exact_stream() {
    let mut log = Vec::with_capacity(3 * REPLY_BYTES);
    for index in 0..3 * REPLY_BYTES {
        log.push(b'a' + (index % 26) as u8);
    }
    let window = render::LOG_WINDOW_BYTES;
    let plan = planned(&["api", "repos/cortexkit/aft/actions/jobs/42/logs"]);
    let mut ranges = Vec::new();
    let mut output = Vec::new();
    let status = render::run(
        &plan,
        &mut |request| {
            let ReadRequest::JobLog {
                job_id,
                range: Some(range),
            } = request
            else {
                panic!("full logs must not request Plexus's no-range tail");
            };
            assert_eq!(*job_id, 42);
            assert_eq!(range.length, window);
            ranges.push(range.clone());
            let offset = range.offset as usize;
            let end = (offset + window as usize).min(log.len());
            Ok(log_reply(&log[offset..end], range.offset, log.len() as u64))
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(status, 0);
    // 3 MiB does not divide into windows evenly, so a short fourth page follows.
    assert_eq!(
        ranges.iter().map(|range| range.offset).collect::<Vec<_>>(),
        vec![0, window, 2 * window, 3 * window]
    );
    assert_eq!(output, log);
}

/// Decode a job-log reply through the real wire contract, so these paging tests
/// exercise the same range checks that a Plexus reply goes through.
fn decoded_log_reply(
    request: &ReadRequest,
    body: &str,
    offset: u64,
    length: u64,
    total_bytes: u64,
) -> Result<ReadReply, ReadError> {
    let wire = executor::wire_request("cortexkit/aft", request).unwrap();
    executor::decode_reply(
        &wire,
        &json!({"isError":false,"result":{"body":body,"offset":offset,"length":length,"total_bytes":total_bytes}}),
    )
}

// The cursor is raw log bytes; bodies are display text (ANSI stripped, split
// characters as U+FFFD). The second page must start at the first reply's
// offset + length, not at the first body's length.
#[test]
fn v20_job_log_pages_resume_at_raw_offset_plus_length_not_body_length() {
    // Page one raw bytes: ESC[32m ok ESC[0m \n plus the first byte of "é".
    let first_raw: &[u8] = b"\x1b[32mok\x1b[0m\n\xc3";
    // Page two raw bytes: the last byte of "é", then ESC[31m bad ESC[0m \n.
    let second_raw: &[u8] = b"\xa9\x1b[31mbad\x1b[0m\n";
    let pages = [
        (first_raw.len() as u64, "ok\n\u{FFFD}"),
        (second_raw.len() as u64, "\u{FFFD}bad\n"),
    ];
    let total = (first_raw.len() + second_raw.len()) as u64;
    let plan = planned(&["api", "repos/cortexkit/aft/actions/jobs/42/logs"]);
    let mut offsets = Vec::new();
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |request| {
            let ReadRequest::JobLog {
                range: Some(range), ..
            } = request
            else {
                panic!("full logs must request a byte range");
            };
            offsets.push(range.offset);
            let (length, body) = pages[offsets.len() - 1];
            decoded_log_reply(request, body, range.offset, length, total)
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(offsets, vec![0, first_raw.len() as u64]);
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "ok\n\u{FFFD}\u{FFFD}bad\n"
    );
}

// A page made only of colour codes has an empty display body but still
// consumes raw bytes, so paging continues instead of reporting a stall.
#[test]
fn v20_job_log_page_of_only_colour_codes_still_advances() {
    let colour_only: &[u8] = b"\x1b[32m\x1b[0m";
    let pages = [(colour_only.len() as u64, ""), (3, "ok\n")];
    let total = colour_only.len() as u64 + 3;
    let plan = planned(&["api", "repos/cortexkit/aft/actions/jobs/42/logs"]);
    let mut offsets = Vec::new();
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |request| {
            let ReadRequest::JobLog {
                range: Some(range), ..
            } = request
            else {
                panic!("full logs must request a byte range");
            };
            offsets.push(range.offset);
            let (length, body) = pages[offsets.len() - 1];
            decoded_log_reply(request, body, range.offset, length, total)
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(offsets, vec![0, colour_only.len() as u64]);
    assert_eq!(output, b"ok\n");
}

// A full window whose edges both split a 4-byte character grows by U+FFFD
// replacements (+6 at the start, +2 at the end). The window headroom keeps that
// page under the 1 MiB decoded-reply bound.
#[test]
fn v20_full_log_window_with_split_characters_at_both_edges_is_accepted() {
    let window = render::LOG_WINDOW_BYTES;
    // Three ASCII bytes misalign the 4-byte emoji, so every window boundary
    // falls one byte into a character.
    let mut log = b"abc".to_vec();
    while log.len() < 2 * window as usize + 64 {
        log.extend_from_slice("\u{1F600}".as_bytes());
    }
    let total = log.len() as u64;
    let plan = planned(&["api", "repos/cortexkit/aft/actions/jobs/42/logs"]);
    let mut offsets = Vec::new();
    let mut bodies = Vec::new();
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |request| {
            let ReadRequest::JobLog {
                range: Some(range), ..
            } = request
            else {
                panic!("full logs must request a byte range");
            };
            offsets.push(range.offset);
            let start = range.offset as usize;
            let end = (start + range.length as usize).min(log.len());
            let body = String::from_utf8_lossy(&log[start..end]).into_owned();
            bodies.push(body.clone());
            decoded_log_reply(request, &body, range.offset, (end - start) as u64, total)
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(offsets, vec![0, window, 2 * window]);
    // The middle page is split at both edges: three lone continuation bytes at
    // the start and one lead byte at the end.
    assert_eq!(bodies[1].len() as u64, window + 8);
    assert!(bodies[1].len() <= REPLY_BYTES);
    assert_eq!(output, bodies.concat().into_bytes());
}

#[test]
fn v20_log_failed_whole_job_fallback_is_complete_across_page_boundaries() {
    let plan = planned(&["run", "view", "8", "--log-failed"]);
    let log = b"2026-10-19T10:00:01Z ok\n2026-10-19T10:00:03Z bad\n";
    let mut requests = Vec::new();
    let mut output = Vec::new();
    render::run(&plan, &mut |request| {
        requests.push(request.clone());
        match request {
            ReadRequest::Get { path, .. } => { assert!(path.starts_with("/repos/cortexkit/aft/actions/runs/8/jobs?")); Ok(reply(json!({"jobs":[{"id":42,"name":"build","conclusion":"failure","steps":[{"name":"setup","started_at":"2026-10-19T10:00:00Z","completed_at":"2026-10-19T10:00:02Z","conclusion":"success"},{"name":"test","started_at":"2026-10-19T10:00:02Z","completed_at":"2026-10-19T10:00:04Z","conclusion":"failure"}]},{"id":43,"conclusion":"success"}]}))) }
            ReadRequest::JobLog { job_id, range: Some(range) } => {
                assert_eq!(*job_id, 42);
                let start = range.offset as usize;
                let end = (start + 13).min(log.len());
                Ok(log_reply(&log[start..end], range.offset, log.len() as u64))
            }
            _ => panic!("unexpected request"),
        }
    }, &mut output).unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "build\tUNKNOWN STEP\t2026-10-19T10:00:01Z ok\nbuild\tUNKNOWN STEP\t2026-10-19T10:00:03Z bad\n"
    );
    assert_eq!(requests.len(), 5);
}

fn landing_policy_job() -> Value {
    json!({"id":120842790454_u64,"name":"Landing policy","conclusion":"failure","steps":[
        {"name":"Run actions/checkout@v4","number":2,"started_at":"2026-10-24T09:57:11Z","completed_at":"2026-10-24T09:57:22Z","conclusion":"success"},
        {"name":"Original PR commit identity and agent attribution","number":3,"started_at":"2026-10-24T09:57:22Z","completed_at":"2026-10-24T09:57:22Z","conclusion":"failure"},
        {"name":"No ignored files tracked under .cortexkit","number":4,"started_at":"2026-10-24T09:57:22Z","completed_at":"2026-10-24T09:57:22Z","conclusion":"skipped"},
        {"name":"Post Run actions/checkout@v4","number":22,"started_at":"2026-10-24T09:57:22Z","completed_at":"2026-10-24T09:57:22Z","conclusion":"success"},
        {"name":"Complete job","number":23,"started_at":"2026-10-24T09:57:22Z","completed_at":"2026-10-24T09:57:22Z","conclusion":"success"}
    ]})
}

fn render_failed_log(body: &str, job: Value) -> (Result<i32, ReadError>, Vec<u8>) {
    let plan = planned(&["run", "view", "40083893167", "--log-failed"]);
    let mut output = Vec::new();
    let result = render::run(
        &plan,
        &mut |request| match request {
            ReadRequest::Get { .. } => Ok(reply(json!({"jobs":[job.clone()]}))),
            ReadRequest::JobLog {
                range: Some(range), ..
            } => {
                assert_eq!(range.offset, 0);
                // Like the Plexus display-text vector, raw storage contains colour
                // escapes that are absent from the decoded display body.
                let raw_length = body.len() as u64 + 9;
                decoded_log_reply(request, body, 0, raw_length, raw_length)
            }
            _ => panic!("unexpected request"),
        },
        &mut output,
    );
    (result, output)
}

#[test]
fn v20_log_failed_real_same_second_steps_print_whole_job_as_unknown_step() {
    // The companion native fixture preserves the operator's per-step archive;
    // this excerpt is from the worker's whole-job display-text response.
    let body = include_str!("../../tests/fixtures/gh_shim/run-40083893167-job-display.txt");
    let (result, output) = render_failed_log(body, landing_policy_job());
    assert_eq!(result.unwrap(), 0);
    let expected = concat!(
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:21.4042950Z ##[group]Checking out the ref\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:21.4043428Z [command]/usr/bin/git checkout --progress --force 4bfc477d907853824afb483e0f2e95f99d9b5cd8\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0036415Z HEAD is now at 4bfc477d9 mason: defer glob edit write checks until content matches\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0073427Z ##[endgroup]\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0128480Z [command]/usr/bin/git log -1 --format=%H\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0158498Z 4bfc477d907853824afb483e0f2e95f99d9b5cd8\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0377683Z ##[group]Run python3 scripts/landing-policy.py --event \"$GITHUB_EVENT_NAME\" --event-path \"$GITHUB_EVENT_PATH\"\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0378545Z python3 scripts/landing-policy.py --event \"$GITHUB_EVENT_NAME\" --event-path \"$GITHUB_EVENT_PATH\"\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0617811Z shell: /usr/bin/bash -e {0}\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0618129Z env:\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0618375Z   SCCACHE_IDLE_TIMEOUT: 0\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0618678Z   SCCACHE_GHA_RW_MODE: READ_ONLY\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0621822Z   GH_TOKEN: ***\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.0622087Z ##[endgroup]\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.1681605Z Landing policy: REFUSED: 7e98a53e8e8d856b919fe52ad0ea8d6662fafc8f: land/storage-floor agent commit missing alfonso-aft co-author trailer\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.1783209Z ##[error]Process completed with exit code 1.\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.1960957Z Post job cleanup.\n",
        "Landing policy\tUNKNOWN STEP\t2026-10-24T09:57:22.2803101Z [command]/usr/bin/git version\n",
    );
    assert_eq!(String::from_utf8(output).unwrap(), expected);
}

#[test]
fn v20_log_failed_empty_and_unattributed_logs_follow_whole_job_fallback() {
    for body in [
        "",
        "not a timestamp or a step boundary\n",
        "2026-10-24T09:57:23.1Z outside all steps\n",
        "2026-10-24T09:57:22.1Z could be checkout, failed step, or cleanup\n",
        "\u{feff}last line without newline",
    ] {
        let (result, output) = render_failed_log(body, landing_policy_job());
        assert_eq!(result.unwrap(), 0);
        let expected = if body.is_empty() {
            String::new()
        } else {
            format!(
                "Landing policy\tUNKNOWN STEP\t{}\n",
                body.trim_end_matches('\n')
            )
        };
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }
}

#[test]
fn v20_log_failed_accepts_native_gh_failure_states_in_job_order() {
    let plan = planned(&["run", "view", "8", "--log-failed"]);
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |request| match request {
            ReadRequest::Get { .. } => Ok(reply(json!({"jobs":[
                {"id":4,"name":"required / review","conclusion":"action_required"},
                {"id":3,"name":"startup","conclusion":"startup_failure"},
                {"id":2,"name":"timeout","conclusion":"timed_out"},
                {"id":1,"name":"failed","conclusion":"failure"}
            ]}))),
            ReadRequest::JobLog { .. } => Ok(log_reply(b"line\r\n", 0, 6)),
            _ => panic!("unexpected request"),
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        concat!(
            "required / review\tUNKNOWN STEP\tline\n",
            "startup\tUNKNOWN STEP\tline\n",
            "timeout\tUNKNOWN STEP\tline\n",
            "failed\tUNKNOWN STEP\tline\n"
        )
    );
}

#[test]
fn v20_log_failed_skips_successful_jobs() {
    let plan = planned(&["run", "view", "40083893167", "--log-failed"]);
    let mut output = Vec::new();
    let mut log_ids = Vec::new();
    let result = render::run(
        &plan,
        &mut |request| match request {
            ReadRequest::Get { .. } => Ok(reply(json!({"jobs":[
                {"id":1,"name":"successful job","conclusion":"success"},
                landing_policy_job(),
                {"id":2,"name":"skipped job","conclusion":"skipped"}
            ]}))),
            ReadRequest::JobLog {
                job_id,
                range: Some(range),
            } => {
                log_ids.push(*job_id);
                assert_eq!(range.offset, 0);
                decoded_log_reply(request, "failed job log\n", 0, 15, 15)
            }
            _ => panic!("unexpected request"),
        },
        &mut output,
    );
    assert_eq!(result.unwrap(), 0);
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "Landing policy\tUNKNOWN STEP\tfailed job log\n"
    );
    assert_eq!(log_ids, vec![120842790454]);
}

#[test]
fn v20_log_protocol_rejects_missing_total_stalls_and_oversized_replies() {
    for response in [
        reply(json!("wrong reply type")),
        log_reply(b"", 0, 3),
        log_reply(&vec![b'a'; REPLY_BYTES + 1], 0, 3_000_000),
        log_reply(b"too long", 0, 2),
        log_reply(b"offset mismatch", 1, 100),
    ] {
        let plan = planned(&["api", "repos/cortexkit/aft/actions/jobs/42/logs"]);
        let mut response = Some(response);
        assert!(render::run(
            &plan,
            &mut |_| Ok(response.take().unwrap()),
            &mut Vec::new()
        )
        .is_err());
    }
}

#[test]
fn v20_budget_refusal_names_bot_rounds_minutes_and_is_not_retried() {
    let plan = planned(&["api", "repos/cortexkit/aft/issues"]);
    let mut calls = 0;
    let error = render::run(
        &plan,
        &mut |_| {
            calls += 1;
            Err(ReadError::Budget {
                retry_after_ms: 60_001,
            })
        },
        &mut Vec::new(),
    )
    .unwrap_err();
    assert_eq!(calls, 1);
    assert_eq!(error.status(), REFUSAL_EXIT_STATUS);
    assert_eq!(
        error.message("aft-bot"),
        "read budget for aft-bot used up; retry in 2 min"
    );
}

#[test]
fn v20_head_and_older_manifests_keep_mechanical_classification() {
    let _lock = crate::test_env::process_env_lock();
    let manifest = manifest();
    let raw = args(&["api", "repos/cortexkit/aft/issues"]);
    assert!(matches!(
        classify(&raw, &manifest, "linux"),
        Classification::Mechanical
    ));
    assert!(
        plan_for_session(&raw, &manifest, "linux", false, "cortexkit/aft")
            .unwrap()
            .is_none()
    );
    assert!(
        plan_for_session(&raw, &manifest, "linux", true, "cortexkit/aft")
            .unwrap()
            .is_some()
    );
    let mut previous = manifest.clone();
    previous.manifest_version = 19;
    assert!(
        plan_for_session(&raw, &previous, "linux", true, "cortexkit/aft")
            .unwrap()
            .is_none()
    );
    let mut undeclared = manifest.clone();
    undeclared.worker_reads.clear();
    assert!(
        plan_for_session(&raw, &undeclared, "linux", true, "cortexkit/aft")
            .unwrap()
            .is_none()
    );
    assert!(
        plan_for_session(&raw, &manifest, "windows", true, "cortexkit/aft")
            .unwrap()
            .is_none()
    );
    let relay = RelayContext {
        worker_broker: None,
        connection_file: None,
        ticket: None,
        transient_delays: [Duration::ZERO; 2],
        relay_budget: Duration::ZERO,
    };
    assert_eq!(relay_client::worker_session(&relay).unwrap(), None);
}

#[test]
fn v20_signed_read_class_cannot_add_unknown_rows_or_change_v19_writes() {
    let manifest = manifest();
    let previous: Manifest = serde_json::from_str(include_str!(
        "../../tests/fixtures/gh_shim/v19-manifest.json"
    ))
    .unwrap();
    for raw in [
        vec!["pr", "ready", "42"],
        vec!["issue", "comment", "42", "--body", "hello"],
        vec!["workflow", "run", "ci.yml"],
        vec!["pr", "merge", "42"],
        vec![
            "api",
            "-X",
            "PATCH",
            "repos/cortexkit/aft/issues/comments/42",
            "-f",
            "body=hello",
        ],
        vec![
            "api",
            "-X",
            "PUT",
            "repos/cortexkit/aft/private-vulnerability-reporting",
        ],
    ] {
        assert_eq!(
            format!("{:?}", classify(&args(&raw), &manifest, "linux")),
            format!("{:?}", classify(&args(&raw), &previous, "linux"))
        );
        assert!(
            !candidate(&args(&raw), &manifest, "linux"),
            "write intercepted: {raw:?}"
        );
    }
    let mut invalid = manifest.clone();
    invalid
        .worker_reads
        .push(TupleDecl::Name("pr merge".into()));
    assert!(invalid.validate().unwrap_err().contains("unreviewed"));
    invalid = manifest.clone();
    invalid.worker_reads.push(invalid.worker_reads[0].clone());
    assert!(invalid.validate().unwrap_err().contains("duplicate"));
    invalid = manifest.clone();
    invalid.manifest_version = 19;
    assert!(invalid.validate().unwrap_err().contains("v20"));
    let mut current = serde_json::to_value(&manifest).unwrap();
    let mut previous = serde_json::to_value(&previous).unwrap();
    for field in ["worker_reads", "manifest_version", "issued_at_unix_secs"] {
        current.as_object_mut().unwrap().remove(field);
        previous.as_object_mut().unwrap().remove(field);
    }
    assert_eq!(current, previous);
}

#[test]
fn v20_native_json_aliases_and_jq_render_from_constrained_gets() {
    let _env_lock = crate::test_env::process_env_lock();
    let jq_dir = tempfile::tempdir().unwrap();
    write_jq_fixture(jq_dir.path());
    let _jq_path = TestJqPathGuard::prepend(jq_dir.path());

    let (value, requests, _) = run_json(
        &[
            "run",
            "list",
            "--branch",
            "main",
            "--status",
            "failure",
            "--json",
            "databaseId,displayTitle,headBranch,workflowName",
        ],
        vec![
            json!({"workflow_runs":[{"id":9,"display_title":"Build","head_branch":"main","name":"CI"}]}),
        ],
    );
    assert_eq!(
        value,
        json!([{"databaseId":9,"displayTitle":"Build","headBranch":"main","workflowName":"CI"}])
    );
    assert!(
        matches!(&requests[0], ReadRequest::Get { path, .. } if path == "/repos/cortexkit/aft/actions/runs?branch=main&status=failure&per_page=20&page=1")
    );
    let (value, requests, _) = run_json(
        &[
            "pr",
            "view",
            "42",
            "--json",
            "number,title,state,headRefName,baseRefName,comments",
        ],
        vec![
            json!({"number":42,"title":"A change","state":"closed","merged_at":"2026-10-19","head":{"ref":"topic"},"base":{"ref":"main"}}),
            json!([{"body":"hello","user":{"login":"bot"}}]),
        ],
    );
    assert_eq!(value["state"], "MERGED");
    assert_eq!(value["headRefName"], "topic");
    assert_eq!(value["comments"][0]["body"], "hello");
    assert_eq!(requests.len(), 2);
    let (value, _, _) = run_json(
        &["issue", "view", "42", "--json", "number,title,state"],
        vec![json!({"number":42,"title":"A bug","state":"open"})],
    );
    assert_eq!(value, json!({"number":42,"title":"A bug","state":"OPEN"}));
    let plan = planned(&[
        "pr",
        "view",
        "42",
        "--json",
        "number,title",
        "--jq",
        ".title",
    ]);
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |_| Ok(reply(json!({"number":42,"title":"A\r\nB"}))),
        &mut output,
    )
    .unwrap();
    assert_eq!(output, b"A\r\nB\n");
}

#[test]
fn v20_graphql_only_fields_refuse_by_name_before_execution() {
    for field in [
        "reviewDecision",
        "projectItems",
        "autoMergeRequest",
        "statusCheckRollup",
    ] {
        let error = plan(
            &args(&["pr", "view", "42", "--json", field]),
            "cortexkit/aft",
        )
        .unwrap_err();
        assert_eq!(
            error,
            format!("{field} isn't available to workers: it needs GraphQL, which v20 excludes")
        );
    }
    for raw in [
        vec!["issue", "view", "42", "--web"],
        vec!["pr", "checks", "42", "--watch"],
        vec!["run", "view", "42", "--json", "notAField"],
        vec!["pr", "view", "42", "--jq", ".title"],
        vec!["pr", "view", "https://github.com/other/repo/pull/42"],
        vec!["api", "--repo", "other/repo", "repos/cortexkit/aft/issues"],
        vec!["workflow", "view"],
    ] {
        assert!(plan(&args(&raw), "cortexkit/aft").is_err(), "{raw:?}");
    }
}

#[test]
fn v20_diff_files_comments_workflows_releases_and_repo_use_gets() {
    let plan = planned(&["pr", "diff", "42", "--patch"]);
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |request| {
            assert_eq!(
                *request,
                ReadRequest::Get {
                    path: "/repos/cortexkit/aft/pulls/42".into(),
                    accept: Some("patch".into())
                }
            );
            Ok(reply(json!("From abc\npatch\n")))
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(output, b"From abc\npatch\n");
    for raw in [
        vec!["pr", "files", "42"],
        vec!["pr", "diff", "42", "--name-only"],
    ] {
        let mut output = Vec::new();
        render::run(&planned(&raw), &mut |request| {
            assert!(matches!(request, ReadRequest::Get { path, accept: None } if path.starts_with("/repos/cortexkit/aft/pulls/42/files?")));
            Ok(reply(json!([{"filename":"src/lib.rs"},{"filename":"README.md"}])))
        }, &mut output).unwrap();
        assert_eq!(output, b"src/lib.rs\nREADME.md\n");
    }
    let mut output = Vec::new();
    render::run(
        &planned(&["issue", "comments", "42"]),
        &mut |_| Ok(reply(json!([{"body":"hello","user":{"login":"bot"}}]))),
        &mut output,
    )
    .unwrap();
    assert_eq!(output, b"bot\thello\n");
    let (value, _, _) = run_json(
        &["workflow", "list", "--json", "id,name,state"],
        vec![
            json!({"workflows":[{"id":7,"name":"CI","state":"active"},{"id":8,"state":"disabled_manually"}]}),
        ],
    );
    assert_eq!(value, json!([{"id":7,"name":"CI","state":"active"}]));
    let mut responses = vec![
        json!({"path":".github/workflows/ci.yml"}),
        json!({"content":"bmFtZTogQ0kK\n"}),
    ]
    .into_iter();
    let mut calls = Vec::new();
    let mut output = Vec::new();
    render::run(
        &planned(&["workflow", "view", "ci.yml", "--yaml", "--ref", "topic"]),
        &mut |request| {
            calls.push(request.clone());
            Ok(reply(responses.next().unwrap()))
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(output, b"name: CI\n");
    assert!(
        matches!(&calls[1], ReadRequest::Get { path, .. } if path == "/repos/cortexkit/aft/contents/.github/workflows/ci.yml?ref=topic")
    );
    let (value, _, _) = run_json(
        &[
            "release",
            "view",
            "v1.2.3",
            "--json",
            "tagName,isDraft,body",
        ],
        vec![json!({"tag_name":"v1.2.3","draft":false,"body":"notes"})],
    );
    assert_eq!(
        value,
        json!({"tagName":"v1.2.3","isDraft":false,"body":"notes"})
    );
    let (value, _, _) = run_json(
        &[
            "repo",
            "view",
            "--json",
            "nameWithOwner,description,isPrivate",
        ],
        vec![json!({"full_name":"cortexkit/aft","description":"tools","private":true})],
    );
    assert_eq!(
        value,
        json!({"nameWithOwner":"cortexkit/aft","description":"tools","isPrivate":true})
    );
}

#[test]
fn v20_checks_render_and_preserve_failure_and_pending_exit_codes() {
    for (state, expected_status, bucket) in [
        ("success", 0, "pass"),
        ("failure", 1, "fail"),
        ("pending", 8, "pending"),
    ] {
        let (value, requests, status) = run_json(
            &["pr", "checks", "42", "--json", "name,state,bucket,link"],
            vec![
                json!({"head":{"sha":"abc"}}),
                json!({"check_runs":[{"name":"CI","status":"completed","conclusion":state,"html_url":"https://github.com/check/7"}]}),
                json!({"statuses":[]}),
            ],
        );
        assert_eq!(status, expected_status);
        assert_eq!(value[0]["bucket"], bucket);
        assert_eq!(requests.len(), 3);
    }
}

#[test]
fn review_list_paging_keeps_size_and_trims_without_duplicate_rows() {
    let plan = planned(&["run", "list", "--limit", "150", "--json", "databaseId"]);
    let mut calls = 0;
    let mut output = Vec::new();
    render::run(&plan, &mut |request| {
        calls += 1;
        let wire = executor::wire_request("cortexkit/aft", request).unwrap();
        let WireRequest::Get { args, .. } = wire else { panic!("not a list") };
        let page: usize = args.query["page"].parse().unwrap();
        let size: usize = args.query["per_page"].parse().unwrap();
        let start = (page - 1) * size;
        Ok(ReadReply::Get(GetReply {
            status: 200,
            body: json!({"workflow_runs": (start..start + size).map(|id| json!({"id":id})).collect::<Vec<_>>()}),
            next_page: Some(page as u64 + 1),
        }))
    }, &mut output).unwrap();
    let rows: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        rows,
        json!((0..150)
            .map(|id| json!({"databaseId":id}))
            .collect::<Vec<_>>())
    );
    assert_eq!(calls, 2);
}

#[test]
fn review_search_paging_keeps_size_and_trims_without_duplicate_rows() {
    for verb in ["code", "issues"] {
        let plan = planned(&["search", verb, "needle", "--limit", "150", "--json", "url"]);
        let mut calls = 0;
        let mut output = Vec::new();
        render::run(
            &plan,
            &mut |request| {
                calls += 1;
                let ReadRequest::Search { page, per_page, .. } = request else {
                    panic!("not a search")
                };
                let start = (page - 1) * per_page;
                Ok(ReadReply::Search(read_contract::SearchReply {
                    total_count: 300,
                    incomplete_results: false,
                    items: (start..start + per_page)
                        .map(|id| json!({"html_url":format!("https://github.com/check/{id}")}))
                        .collect(),
                    next_page: Some(page + 1),
                }))
            },
            &mut output,
        )
        .unwrap();
        let rows: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            rows,
            json!((0..150)
                .map(|id| json!({"url":format!("https://github.com/check/{id}")}))
                .collect::<Vec<_>>()),
            "{verb}"
        );
        assert_eq!(calls, 2);
    }
}

#[test]
fn review_checks_include_failure_or_pending_status_from_later_page() {
    for (state, expected) in [("failure", 1), ("pending", 8)] {
        let plan = planned(&["pr", "checks", "42", "--json", "name,bucket"]);
        let mut calls = 0;
        let mut output = Vec::new();
        let status = render::run(
            &plan,
            &mut |request| {
                calls += 1;
                let ReadRequest::Get { path, .. } = request else {
                    panic!("not a GET")
                };
                match calls {
                    1 => Ok(reply(json!({"head":{"sha":"abc"}}))),
                    2 => Ok(reply(json!({"check_runs":[]}))),
                    3 => Ok(ReadReply::Get(GetReply {
                        status: 200,
                        body: json!({"statuses":[{"context":"first","state":"success"}]}),
                        next_page: Some(2),
                    })),
                    4 => {
                        assert!(
                            path.ends_with("/commits/abc/status?per_page=100&page=2"),
                            "{path}"
                        );
                        Ok(reply(
                            json!({"statuses":[{"context":"later","state":state}]}),
                        ))
                    }
                    _ => panic!("unexpected request"),
                }
            },
            &mut output,
        )
        .unwrap();
        assert_eq!(status, expected, "lost page 2 {state} status");
        let rows: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 2);
        assert_eq!(rows[1]["name"], "later");
        assert_eq!(calls, 4);
    }
}

#[test]
fn review_failed_log_whole_job_fallback_does_not_infer_from_precise_timestamps() {
    let plan = planned(&["run", "view", "8", "--log-failed"]);
    let log = b"2026-10-19T10:00:02.1000000Z passing\n2026-10-19T10:00:02.2000000Z boundary failure\n2026-10-19T10:00:02.3000000Z failure\n2026-10-19T10:00:02.4000000Z after failure\n";
    let mut output = Vec::new();
    render::run(&plan, &mut |request| match request {
        ReadRequest::Get { .. } => Ok(reply(json!({"jobs":[{"id":42,"name":"build","conclusion":"failure","steps":[
            {"name":"setup","started_at":"2026-10-19T10:00:02Z","completed_at":"2026-10-19T10:00:02.2Z","conclusion":"success"},
            {"name":"test","started_at":"2026-10-19T10:00:02.20Z","completed_at":"2026-10-19T10:00:02.4Z","conclusion":"failure"},
            {"name":"cleanup","started_at":"2026-10-19T10:00:02.400Z","completed_at":"2026-10-19T10:00:03Z","conclusion":"success"}
        ]}]}))),
        ReadRequest::JobLog { range: Some(range), .. } => {
            let start = range.offset as usize;
            let end = (start + 13).min(log.len());
            Ok(log_reply(&log[start..end], range.offset, log.len() as u64))
        }
        _ => panic!("unexpected request"),
    }, &mut output).unwrap();
    assert_eq!(String::from_utf8(output).unwrap(), "build\tUNKNOWN STEP\t2026-10-19T10:00:02.1000000Z passing\nbuild\tUNKNOWN STEP\t2026-10-19T10:00:02.2000000Z boundary failure\nbuild\tUNKNOWN STEP\t2026-10-19T10:00:02.3000000Z failure\nbuild\tUNKNOWN STEP\t2026-10-19T10:00:02.4000000Z after failure\n");
}

#[test]
fn review_named_branch_and_release_tag_allow_encoded_slashes() {
    let (value, _, status) = run_json(
        &["release", "view", "release/v1", "--json", "tagName"],
        vec![json!({"tag_name":"release/v1"})],
    );
    assert_eq!(status, 0);
    assert_eq!(value["tagName"], "release/v1");
    let (value, _, status) = run_json(
        &["pr", "checks", "42", "--required", "--json", "name"],
        vec![
            json!({"head":{"sha":"abc"},"base":{"ref":"release/1.x"}}),
            json!({"check_runs":[]}),
            json!({"statuses":[{"context":"CI","state":"success"}]}),
            json!({"contexts":["CI"]}),
        ],
    );
    assert_eq!(status, 0);
    assert_eq!(value, json!([{"name":"CI"}]));
}

#[test]
fn v20_api_pagination_follows_integer_pages_without_changing_repository() {
    let plan = planned(&[
        "api",
        "repos/cortexkit/aft/issues?per_page=5",
        "--paginate",
        "--slurp",
    ]);
    let mut calls = Vec::new();
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |request| {
            calls.push(request.clone());
            Ok(ReadReply::Get(GetReply {
                status: 200,
                body: json!([calls.len()]),
                next_page: (calls.len() == 1).then_some(2),
            }))
        },
        &mut output,
    )
    .unwrap();
    assert_eq!(output, b"[[1],[2]]\n");
    assert_eq!(
        calls[1],
        ReadRequest::Get {
            path: "/repos/cortexkit/aft/issues?page=2&per_page=5".into(),
            accept: None
        }
    );
    let error = render::run(
        &plan,
        &mut |_| {
            Ok(ReadReply::Get(GetReply {
                status: 200,
                body: json!([]),
                next_page: Some(1),
            }))
        },
        &mut Vec::new(),
    )
    .unwrap_err();
    assert_eq!(error.status(), REFUSAL_EXIT_STATUS);
}

#[test]
fn v20_missing_ticket_refuses_without_delegation_or_credentials() {
    let binding = AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "aft-bot".into(),
    };
    let manifest = manifest();
    let relay = RelayContext {
        worker_broker: None,
        connection_file: None,
        ticket: None,
        transient_delays: [Duration::ZERO; 2],
        relay_budget: Duration::ZERO,
    };
    let context = ExecutionContext {
        binding: &binding,
        manifest: &manifest,
        relay: &relay,
    };
    let error = execute_read(
        &context,
        &ReadRequest::Get {
            path: "/repos/cortexkit/aft/issues".into(),
            accept: None,
        },
    )
    .unwrap_err();
    assert!(error.message("aft-bot").contains("no live command ticket"));
    assert_eq!(error.status(), REFUSAL_EXIT_STATUS);
}

#[test]
fn v20_preparer_pins_test_only_fixture_and_preserves_every_v19_row() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fixture = root.join("crates/aft/tests/fixtures/gh_shim/v19-manifest.json");
    let raw = fs::read(&fixture).unwrap();
    // This fixture digest is pinned for this test, not used as signing material.
    assert_eq!(
        format!("{:x}", Sha256::digest(&raw)),
        "12c14ca1c8779b358998b000ef6210c75147ceec1f8c318fd5e7be42799a3573"
    );
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("v20.json");
    let run = |input: &Path, issued: &str| {
        Command::new("python3")
            .arg(root.join("scripts/prepare-gh-shim-v20.py"))
            .arg(input)
            .arg(&destination)
            .args(["--issued-at-unix-secs", issued])
            .output()
            .unwrap()
    };
    let output = run(&fixture, "1792420000");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = fs::read(&destination).unwrap();
    let digest = format!("{:x}", Sha256::digest(&payload));
    assert!(String::from_utf8(output.stdout).unwrap().contains(&digest));
    assert!(fs::read_to_string(destination.with_extension("sha256"))
        .unwrap()
        .starts_with(&digest));
    let mut current: Value = serde_json::from_slice(&payload).unwrap();
    assert_eq!(current["manifest_version"], 20);
    assert_eq!(current["issued_at_unix_secs"], 1_792_420_000_u64);
    let mut previous: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!(
        current["worker_reads"].as_array().unwrap().len(),
        TUPLES.len()
    );
    for field in ["worker_reads", "manifest_version", "issued_at_unix_secs"] {
        current.as_object_mut().unwrap().remove(field);
        previous.as_object_mut().unwrap().remove(field);
    }
    assert_eq!(current, previous);
}

#[test]
fn v20_status_records_the_last_lookup_outcome_and_reason() {
    let _lock = crate::test_env::process_env_lock();
    let dir = tempfile::tempdir().unwrap();
    let paths = StatePaths::from_root(dir.path().to_path_buf());
    crate::gh_shim::tests::write_signed_manifest(&paths, manifest(), unix_seconds());
    for (result, outcome, reason) in [
        (Ok(Some(true)), "worker", Value::Null),
        (Ok(Some(false)), "head", Value::Null),
        (
            Ok(None),
            "unscoped",
            json!("no authenticated scoped command session"),
        ),
        (
            Err("daemon offline".into()),
            "unavailable",
            json!("daemon offline"),
        ),
    ] {
        record_lookup(&paths, &result);
        let value = serde_json::to_value(last_lookup(&paths).unwrap()).unwrap();
        assert_eq!(value["outcome"], outcome);
        assert_eq!(value["reason"], reason);
        let status: Value = serde_json::from_str(&render_self_report(&paths).unwrap()).unwrap();
        assert_eq!(status["last_read_lookup"], value);
    }
}

#[test]
fn v20_native_modifiers_are_not_silently_ignored() {
    for raw in [
        vec!["pr", "diff", "42", "--color", "always"],
        vec!["workflow", "view", "ci.yml", "--ref", "main"],
        vec!["pr", "list", "--author", "@me"],
    ] {
        let plan = plan(&args(&raw), "cortexkit/aft");
        if raw[0] == "pr" && raw[1] == "list" {
            assert!(render::run(
                &plan.unwrap(),
                &mut |_| panic!("@me must refuse before a request"),
                &mut Vec::new()
            )
            .is_err());
        } else {
            assert!(plan.is_err(), "{raw:?}");
        }
    }
    let (value, requests, _) = run_json(
        &[
            "run",
            "list",
            "--workflow",
            "old.yml",
            "--all",
            "--json",
            "databaseId",
        ],
        vec![
            json!({"id":8,"state":"disabled_manually"}),
            json!({"workflow_runs":[{"id":9}]}),
        ],
    );
    assert_eq!(value, json!([{"databaseId":9}]));
    assert!(
        matches!(&requests[1], ReadRequest::Get { path, .. } if path.starts_with("/repos/cortexkit/aft/actions/workflows/8/runs?"))
    );
    let plan = planned(&["run", "view", "42", "--verbose"]);
    let mut responses = vec![json!({"id":42}), json!({"jobs":[{"id":8,"name":"build","steps":[{"name":"test step","conclusion":"success"}]}]})].into_iter();
    let mut output = Vec::new();
    render::run(
        &plan,
        &mut |_| Ok(reply(responses.next().unwrap())),
        &mut output,
    )
    .unwrap();
    assert!(String::from_utf8(output)
        .unwrap()
        .contains("  success test step\n"));
    let (value, _, _) = run_json(
        &["pr", "checks", "42", "--json", "event,workflow"],
        vec![
            json!({"head":{"sha":"abc"}}),
            json!({"check_runs":[{"app":{"slug":"github-actions"},"check_suite":{"id":7}}]}),
            json!({"statuses":[]}),
            json!({"workflow_runs":[{"event":"pull_request","name":"CI"}]}),
        ],
    );
    assert_eq!(value, json!([{"event":"pull_request","workflow":"CI"}]));
}

#[test]
fn v20_rest_backed_json_preserves_native_aliases() {
    let (value, _, _) = run_json(
        &["run", "list", "--json", "startedAt"],
        vec![json!({"workflow_runs":[{"id":9,"run_started_at":"2026-10-19T12:00:00Z"}]})],
    );
    assert_eq!(value, json!([{"startedAt":"2026-10-19T12:00:00Z"}]));

    let (value, _, _) = run_json(
        &["release", "view", "v1", "--json", "author"],
        vec![json!({"author":{"node_id":"user-id","login":"bot","type":"Bot"}})],
    );
    assert_eq!(value["author"]["id"], "user-id");
    assert_eq!(value["author"]["is_bot"], true);
    let (value, _, _) = run_json(
        &["pr", "view", "42", "--json", "files"],
        vec![
            json!({"number":42}),
            json!([{"filename":"src/lib.rs","additions":2,"deletions":1}]),
        ],
    );
    assert_eq!(value["files"][0]["path"], "src/lib.rs");
}

#[test]
fn v20_executor_uses_pinned_plexus_repo_get_request_and_success() {
    let binding = AgentBinding {
        repo: "cortexkit/aft".into(),
        agent_id: "not-sent-as-identity".into(),
    };
    let manifest = manifest();
    let relay = RelayContext {
        worker_broker: None,
        connection_file: None,
        ticket: Some("test-ticket".into()),
        transient_delays: [Duration::ZERO; 2],
        relay_budget: Duration::from_secs(1),
    };
    let context = ExecutionContext {
        binding: &binding,
        manifest: &manifest,
        relay: &relay,
    };
    let request = ReadRequest::Get {
        path: "/repos/cortexkit/aft/pulls/7/files?a=b&per_page=10".into(),
        accept: None,
    };
    let mut calls = Vec::new();
    let _result = execute_read_with(&context, &request, &mut |request| {
        calls.push(request.clone());
        Ok(json!({"result":{"status":200,"body":[{"filename":"src/lib.rs"}],"next_page":2},"content":[{"type":"text","text":"never parse this field"}],"isError":false,"structuredContent":{"status":200,"body":[{"filename":"src/lib.rs"}],"next_page":2}}))
    }).expect("draft github_read success must be decoded");
    assert_eq!(
        calls,
        vec![
            json!({"action":"github.repo.get","repository":"cortexkit/aft","args":{"path":"pulls/7/files","query":{"a":"b","per_page":"10"},"accept":"application/vnd.github+json"}})
        ]
    );
}

#[test]
fn audit_candidate_excludes_payloads_methods_graphql_and_malformed_argv() {
    let _lock = crate::test_env::process_env_lock();
    let manifest = manifest();
    for suffix in [
        vec!["-f", "a=b"],
        vec!["-F", "a=b"],
        vec!["--field=a=b"],
        vec!["--raw-field", "a=b"],
        vec!["--input", "f"],
        vec!["--input=f"],
        vec!["-X", "POST"],
        vec!["-X", "GET", "-f", "a=b"],
        vec!["-XPOST", "-XGET"],
        vec!["--method"],
        vec!["--unknown"],
    ] {
        let mut raw = vec!["api", "repos/cortexkit/aft/issues"];
        raw.extend(suffix);
        assert!(!candidate(&args(&raw), &manifest, "linux"), "{raw:?}");
    }
    for endpoint in [
        "graphql",
        "GRAPHQL",
        "/GraphQL",
        "/graphql/",
        "https://api.github.com/GRAPHQL",
    ] {
        assert!(
            !candidate(&args(&["api", endpoint]), &manifest, "linux"),
            "{endpoint}"
        );
    }
    assert!(candidate(
        &args(&["api", "-X", "GET", "repos/cortexkit/aft/issues"]),
        &manifest,
        "linux"
    ));
    // Invalid read targets still reach read planning, not ambient gh passthrough.
    assert!(candidate(
        &args(&["api", "https://evil.example/repos/cortexkit/aft/issues"]),
        &manifest,
        "linux"
    ));
}

#[cfg(unix)]
#[test]
fn audit_candidate_rejects_non_utf8_native_arguments() {
    use std::os::unix::ffi::OsStringExt;
    let _lock = crate::test_env::process_env_lock();
    let manifest = manifest();
    for mut argv in [
        args(&["run", "view", "7"]),
        args(&["api", "-X", "POST", "repos/cortexkit/aft/issues"]),
    ] {
        argv.push(OsString::from_vec(vec![0xff]));
        assert!(!candidate(&argv, &manifest, "linux"));
    }
}

#[test]
fn audit_authorization_header_guard_has_a_discriminating_accept_value() {
    let error = plan(
        &args(&[
            "api",
            "repos/cortexkit/aft/pulls/7",
            "-H",
            "Authorization: application/vnd.github.diff",
        ]),
        "cortexkit/aft",
    )
    .unwrap_err();
    assert!(error.contains("only an Accept"), "{error}");
    assert!(plan(
        &args(&[
            "api",
            "repos/cortexkit/aft/pulls/7",
            "-H",
            "Accept: application/vnd.github.diff"
        ]),
        "cortexkit/aft"
    )
    .is_ok());
}

#[test]
fn audit_jq_child_receives_no_operator_credential_environment() {
    let _lock = crate::test_env::process_env_lock();
    let key = "GH_TOKEN";
    let old = std::env::var_os(key);
    std::env::set_var(key, "not-a-real-token");
    let mut output = Vec::new();
    let result = render::run(
        &planned(&["api", "repos/cortexkit/aft/issues", "--jq", "env"]),
        &mut |_| Ok(reply(json!([]))),
        &mut output,
    );
    match old {
        Some(value) => std::env::set_var(key, value),
        None => std::env::remove_var(key),
    }
    result.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&output).unwrap(), json!({}));
}

#[test]
fn audit_checkout_binding_cannot_be_retargeted_by_repo_flag() {
    let _lock = crate::test_env::process_env_lock();
    let dir = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .arg(dir.path())
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args([
            "remote",
            "add",
            "origin",
            "https://github.com/cortexkit/aft.git"
        ])
        .status()
        .unwrap()
        .success());
    let manifest = manifest();
    assert!(manifest.bindings.contains_key("cortexkit/common-auth"));
    let old = std::env::var_os("GH_REPO");
    std::env::set_var("GH_REPO", "cortexkit/aft");
    let denied = ticket_bound_repository(
        &args(&["repo", "view", "--repo", "cortexkit/common-auth"]),
        &manifest,
        "cortexkit/aft",
    );
    let control = ticket_bound_repository(
        &args(&["repo", "view", "--repo", "cortexkit/aft"]),
        &manifest,
        "cortexkit/aft",
    );
    match old {
        Some(value) => std::env::set_var("GH_REPO", value),
        None => std::env::remove_var("GH_REPO"),
    }
    assert!(control.is_ok());
    assert!(denied.is_err());
}

#[test]
fn audit_checkout_binding_cannot_be_retargeted_by_gh_repo() {
    let _lock = crate::test_env::process_env_lock();
    let dir = tempfile::tempdir().unwrap();
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .arg(dir.path())
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args([
            "remote",
            "add",
            "origin",
            "https://github.com/cortexkit/aft.git"
        ])
        .status()
        .unwrap()
        .success());
    let old = std::env::var_os("GH_REPO");
    std::env::set_var("GH_REPO", "cortexkit/common-auth");
    let result = ticket_bound_repository(&args(&["repo", "view"]), &manifest(), "cortexkit/aft");
    std::env::set_var("GH_REPO", "cortexkit/aft");
    let control = ticket_bound_repository(&args(&["repo", "view"]), &manifest(), "cortexkit/aft");
    match old {
        Some(value) => std::env::set_var("GH_REPO", value),
        None => std::env::remove_var("GH_REPO"),
    }
    assert!(control.is_ok());
    assert!(result.is_err());
}

fn preparer_probe(mode: &str) -> std::process::Output {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = tempfile::tempdir().unwrap();
    let code = r#"
import hashlib, importlib.util, json, pathlib, sys
sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location('prepare_v20', sys.argv[1])
m = importlib.util.module_from_spec(spec); spec.loader.exec_module(m)
raw = pathlib.Path(sys.argv[2]).read_bytes()
mode = sys.argv[4]
if mode == 'sha':
    value = json.loads(raw); value['issued_at_unix_secs'] += 1
    bad = m.encode(value).encode('utf-8')
elif mode == 'format':
    bad = raw + b' '
    m.BASELINE_SHA256 = hashlib.sha256(bad).hexdigest()
else:
    bad = raw
path = pathlib.Path(sys.argv[3]) / 'input.json'; path.write_bytes(bad)
issued = 1792326319 if mode == 'timestamp' else 1792420000
m.prepare(path, pathlib.Path(sys.argv[3]) / 'output.json', issued)
"#;
    Command::new("python3")
        .args(["-c", code])
        .arg(root.join("scripts/prepare-gh-shim-v20.py"))
        .arg(root.join("crates/aft/tests/fixtures/gh_shim/v19-manifest.json"))
        .arg(dir.path())
        .arg(mode)
        .output()
        .unwrap()
}

#[test]
fn audit_preparer_sha_guard_uses_canonically_formatted_changed_bytes() {
    let output = preparer_probe("sha");
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("expected reviewed v19 baseline sha256")
    );
}

#[test]
fn audit_preparer_format_guard_is_independent_of_the_hash_guard() {
    // preparer_probe imports scripts/prepare-gh-shim-v20.py in a child Python
    // interpreter, appends a space to the test-only v19 manifest, and replaces
    // that module's BASELINE_SHA256 with the new byte hash. Only the formatting
    // check can then refuse; neither the script nor its pinned digest is edited.
    let output = preparer_probe("format");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("baseline formatting differs"));
}

#[test]
fn audit_preparer_timestamp_guard_has_a_valid_reviewed_baseline() {
    let output = preparer_probe("timestamp");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be later"));
}

#[test]
fn audit_search_repository_json_is_refused_before_any_result_relabeling() {
    for verb in ["code", "issues"] {
        let error = plan(
            &args(&["search", verb, "needle", "--json", "repository"]),
            "cortexkit/aft",
        )
        .unwrap_err();
        assert!(error.contains("repository"), "{error}");
        assert!(plan(
            &args(&["search", verb, "needle", "--json", "url"]),
            "cortexkit/aft"
        )
        .is_ok());
    }
}

#[test]
fn v20_broker_t7_worker_discovery_never_loads_general_connection() {
    let _lock = crate::test_env::process_env_lock();
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("cortexkit");
    std::fs::create_dir_all(&config).unwrap();
    let connection = root.path().join("general-connection.json");
    std::fs::write(&connection, "not-a-real-general-credential").unwrap();
    std::fs::write(
        config.join("aft.jsonc"),
        serde_json::to_vec(&json!({"subc":{"connection_file":connection}})).unwrap(),
    )
    .unwrap();
    let changes = [
        (
            "XDG_CONFIG_HOME",
            root.path().to_string_lossy().into_owned(),
        ),
        ("AFT_GH_SHIM_WORKER", "1".into()),
        ("AFT_GH_SHIM_TICKET", "outer-command-ticket".into()),
        (
            "AFT_GH_READ_SOCKET",
            root.path().join("broker.sock").display().to_string(),
        ),
        (
            "AFT_GH_READ_CAPABILITY_FILE",
            root.path().join("capability").display().to_string(),
        ),
    ];
    let saved: Vec<_> = changes
        .iter()
        .map(|(key, _)| (*key, std::env::var_os(key)))
        .collect();
    for (key, value) in &changes {
        std::env::set_var(key, value);
    }
    let context = RelayContext::from_process();
    for (key, value) in saved {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    }
    assert!(context.worker_broker.is_some());
    assert!(
        context.ticket.is_none(),
        "a worker must never retain the general relay ticket"
    );
    assert!(
        context.connection_file.is_none(),
        "a worker must never discover the general daemon credential"
    );
}
