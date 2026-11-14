pub use aft_base::url_fetch::*;

#[cfg(test)]
#[allow(unused_imports)]
use crate::lang_id::language_by_extension;
#[cfg(test)]
#[allow(unused_imports)]
use htmd::{
    element_handler::{HandlerResult, Handlers},
    Element, HtmlToMarkdown,
};
#[cfg(test)]
#[allow(unused_imports)]
use markup5ever_rcdom::{Node, NodeData};
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::blocking::{Client, Response as HttpResponse};
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_TYPE, LOCATION, USER_AGENT,
};
#[cfg(test)]
#[allow(unused_imports)]
use reqwest::redirect::Policy;
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::audit::{AuditReason, AuditRecordV1, Decision};
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::location::{
    evaluate_request_with_connect, location_kind_token, LabelOutcome, LabelRule, Location,
    RequestView,
};
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::material::{check_admissible, encode_for_location};
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::scrub::{MatcherSet, ScrubError, Scrubber};
#[cfg(test)]
#[allow(unused_imports)]
use secret_rules::target::{normalize_request_target, ConnectAuthority};
#[cfg(test)]
#[allow(unused_imports)]
use serde::{Deserialize, Serialize};
#[cfg(test)]
#[allow(unused_imports)]
use sha2::{Digest, Sha256};
#[cfg(test)]
#[allow(unused_imports)]
use std::collections::HashMap;
#[cfg(test)]
#[allow(unused_imports)]
use std::error::Error;
#[cfg(test)]
#[allow(unused_imports)]
use std::fmt;
#[cfg(test)]
#[allow(unused_imports)]
use std::fs;
#[cfg(test)]
#[allow(unused_imports)]
use std::io::{self, Read, Write};
#[cfg(test)]
#[allow(unused_imports)]
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
#[cfg(test)]
#[allow(unused_imports)]
use std::path::{Path, PathBuf};
#[cfg(test)]
#[allow(unused_imports)]
use std::rc::Rc;
#[cfg(test)]
#[allow(unused_imports)]
use std::sync::{mpsc, Arc};
#[cfg(test)]
#[allow(unused_imports)]
use std::thread;
#[cfg(test)]
#[allow(unused_imports)]
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
#[cfg(test)]
#[allow(unused_imports)]
use url::Url;
#[cfg(test)]
#[allow(unused_imports)]
use zeroize::Zeroizing;

#[cfg(test)]
mod tests {
    #[test]
    fn extension_only_url_decision_matches_parser() {
        for ext in crate::lang_id::TEST_EXTENSIONS {
            let probe = std::path::Path::new("file").with_extension(ext);
            let expected = if crate::parser::detect_language(&probe).is_some() {
                super::static_extension_for_lang_ext(ext)
            } else {
                None
            };
            let url = url::Url::parse(&format!("https://example.com/file.{ext}")).unwrap();
            assert_eq!(super::extension_from_url_path(&url), expected, "{ext}");
        }
        for name in ["Jenkinsfile", "file", ".hidden"] {
            let url = url::Url::parse(&format!("https://example.com/{name}")).unwrap();
            assert_eq!(super::extension_from_url_path(&url), None, "{name}");
        }
    }

    #[test]
    fn h_header_url_cache_preserves_ambiguous_extension() {
        assert_eq!(super::static_extension_for_lang_ext("h"), Some(".h"));
    }

    use super::*;
    use url::Url;

    fn write_cache_entry(storage: &Path, hash: &str, url: &str, fetched_at: u64) -> PathBuf {
        let dir = cache_dir(storage);
        fs::create_dir_all(&dir).unwrap();
        let meta = CacheMeta {
            url: url.to_string(),
            content_type: "text/markdown".to_string(),
            extension: ".md".to_string(),
            fetched_at,
            download_truncated: false,
            downloaded_bytes: 4,
            total_bytes: Some(4),
        };
        fs::write(content_path(storage, hash, ".md"), "body").unwrap();
        let meta_file = meta_path(storage, hash);
        fs::write(&meta_file, serde_json::to_vec(&meta).unwrap()).unwrap();
        meta_file
    }

    fn cleanup_counting_reads(storage: &Path, now: u64) -> (usize, usize) {
        CLEANUP_META_READS.with(|reads| reads.set(0));
        let removed = cleanup_url_cache_at(storage, now).unwrap();
        (removed, CLEANUP_META_READS.with(std::cell::Cell::get))
    }

    /// Every configure sweeps the URL cache, so a sweep must not reopen
    /// metadata files that have not changed since the previous sweep.
    #[test]
    fn url_cache_cleanup_does_not_reread_unchanged_metadata() {
        let storage = tempfile::tempdir().unwrap();
        let now = 10 * CACHE_TTL_MS;
        for index in 0..3 {
            write_cache_entry(
                storage.path(),
                &format!("fresh{index}"),
                "https://example.com/a",
                now - 1_000,
            );
        }
        let stale = write_cache_entry(
            storage.path(),
            "stale",
            "https://example.com/b",
            now - CACHE_TTL_MS - 1,
        );

        assert_eq!(cleanup_counting_reads(storage.path(), now), (1, 4));
        assert!(!stale.exists());
        assert!(!content_path(storage.path(), "stale", ".md").exists());

        // Nothing changed: no metadata file is opened again.
        assert_eq!(cleanup_counting_reads(storage.path(), now), (0, 0));
    }

    /// A remembered fetch time is still compared with the current time, and a
    /// rewritten metadata file is read again, so the memo never keeps an entry
    /// alive past its TTL.
    #[test]
    fn url_cache_cleanup_memo_still_expires_and_notices_rewrites() {
        let storage = tempfile::tempdir().unwrap();
        let now = 10 * CACHE_TTL_MS;
        let kept = write_cache_entry(storage.path(), "kept", "https://example.com/a", now);
        let rewritten =
            write_cache_entry(storage.path(), "rewritten", "https://example.com/b", now);
        assert_eq!(cleanup_counting_reads(storage.path(), now), (0, 2));

        // A different size is a different version even within one mtime tick.
        write_cache_entry(
            storage.path(),
            "rewritten",
            "https://example.com/b-longer",
            now - CACHE_TTL_MS - 1,
        );
        assert_eq!(cleanup_counting_reads(storage.path(), now), (1, 1));
        assert!(!rewritten.exists());
        assert!(kept.exists());

        // The unchanged file expires from its remembered fetch time.
        assert_eq!(
            cleanup_counting_reads(storage.path(), now + CACHE_TTL_MS + 1),
            (1, 0)
        );
        assert!(!kept.exists());
    }

    #[test]
    fn web_view_urls_rewrite_to_raw_content() {
        let cases = [
            (
                "https://github.com/owner/repo/blob/main/docs/guide.md?download=1#L42",
                "https://raw.githubusercontent.com/owner/repo/main/docs/guide.md?download=1",
            ),
            (
                "https://github.com/owner/repo/raw/v1.2/src/lib.rs?raw=1#L7",
                "https://raw.githubusercontent.com/owner/repo/v1.2/src/lib.rs?raw=1",
            ),
            (
                "https://www.github.com/owner/repo/blob/main/README.md",
                "https://raw.githubusercontent.com/owner/repo/main/README.md",
            ),
            (
                "https://gitlab.com/owner/repo/-/blob/main/docs/guide.md?plain=1#L4",
                "https://gitlab.com/owner/repo/-/raw/main/docs/guide.md?plain=1",
            ),
        ];

        for (input, expected) in cases {
            let url = Url::parse(input).unwrap();
            assert_eq!(rewrite_url_for_fetch(&url).as_str(), expected);
        }
    }

    #[test]
    fn non_web_view_urls_are_not_rewritten() {
        for input in [
            "https://gist.github.com/owner/abcdef/blob/main/file.md",
            "https://github.com/owner/repo",
            "https://github.com/owner/repo/blob",
            "https://api.github.com/repos/owner/repo",
            "https://notgithub.com/owner/repo/blob/main/file.md",
            "https://gitlab.example.com/owner/repo/-/blob/main/file.md",
        ] {
            let url = Url::parse(input).unwrap();
            assert_eq!(rewrite_url_for_fetch(&url).as_str(), input);
        }
    }

    #[test]
    fn extension_from_path_uses_parser_mapping() {
        let url = Url::parse("https://example.com/pkg/index.mjs").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/javascript").unwrap();
        assert_eq!(ext, ".mjs");
        assert!(from_path);
    }

    #[test]
    fn text_plain_rs_url_ignores_content_type_gate() {
        let url = Url::parse("https://raw.githubusercontent.com/o/r/main/lib.rs").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".rs");
        assert!(from_path);
    }

    #[test]
    fn extensionless_javascript_maps_to_js() {
        let url = Url::parse("https://cdn.example/bundle").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/javascript").unwrap();
        assert_eq!(ext, ".js");
        assert!(!from_path);
    }

    #[test]
    fn extensionless_plain_stays_md() {
        let url = Url::parse("https://example.com/readme").unwrap();
        let (ext, _) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".md");
    }

    #[test]
    fn query_and_fragment_do_not_break_path_extension() {
        let url = Url::parse("https://example.com/src/file.ts?v=2#L10").unwrap();
        let (ext, from_path) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".ts");
        assert!(from_path);
    }

    #[test]
    fn percent_encoded_path_segment() {
        let url = Url::parse("https://example.com/foo%2Fbar.rs").unwrap();
        // path is /foo%2Fbar.rs - segment is foo%2Fbar.rs -> decode to foo/bar.rs -> ext rs
        let (ext, _) = resolve_fetch_extension(&url, "text/plain").unwrap();
        assert_eq!(ext, ".rs");
    }

    #[test]
    fn binary_sniff_detects_nul() {
        let mut body = vec![b'f', b'n', 0, b' '];
        assert!(body_contains_nul_in_prefix(&body));
        body = vec![b'h'; 9000];
        assert!(!body_contains_nul_in_prefix(&body));
    }

    #[test]
    fn unsupported_pdf_still_errors_via_resolve() {
        let url = Url::parse("https://example.com/doc.pdf").unwrap();
        assert!(resolve_fetch_extension(&url, "application/pdf").is_none());
    }

    #[test]
    fn is_permalink_anchor_doc_anchor_class_drops_anchor() {
        let md = html_to_markdown_converter()
            .convert("<a class=\"doc-anchor\" href=\"#stdio-transport\">x</a>")
            .unwrap();
        assert_eq!(
            md.trim(),
            "",
            "doc-anchor permalink should be dropped: {md:?}"
        );
    }

    #[test]
    fn html_to_markdown_rustdoc_heading_cleanup() {
        let html = "<main><h1>Function <span class=\"fn\">stdio</span>&nbsp;<button id=\"copy-path\">Copy item path</button></h1><h2 class=\"location\"><a href=\"#\">stdio</a></h2><h2><a class=\"doc-anchor\" href=\"#stdio-transport\">§</a>StdIO Transport</h2></main>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert!(md.contains("# Function stdio"), "{md:?}");
        assert!(!md.contains("Copy item path"), "{md:?}");
        assert!(md.contains("## stdio"), "{md:?}");
        assert!(!md.contains("[stdio](#)"), "{md:?}");
        assert!(md.contains("## StdIO Transport"), "{md:?}");
        assert!(!md.contains('§'), "{md:?}");
        assert!(!md.contains("](#stdio-transport)"), "{md:?}");
    }

    #[test]
    fn html_to_markdown_docusaurus_hash_link_still_dropped() {
        let html = "<h2>The Retain Pipeline<a class=\"hash-link\" href=\"#x\" aria-label=\"Direct link to The Retain Pipeline\">\u{200b}</a></h2>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert!(md.contains("## The Retain Pipeline"), "{md:?}");
        assert!(!md.contains('\u{200b}'), "{md:?}");
    }

    const CHROME_AUTO_CONNECT_HTML: &str =
        include_str!("../tests/fixtures/url_html/chrome_auto_connect.html");

    /// The fenced blocks of `markdown` as (opening fence line, body). Body
    /// lines lose only the indentation the opening fence has (a list item's
    /// indent), so the code's own indentation is compared as written.
    fn fenced_blocks(markdown: &str) -> Vec<(String, String)> {
        let mut blocks = Vec::new();
        let mut open: Option<(String, usize, Vec<String>)> = None;
        for line in markdown.lines() {
            let trimmed = line.trim_start();
            match open.take() {
                None => {
                    if trimmed.starts_with("```") {
                        let indent = line.len() - trimmed.len();
                        open = Some((trimmed.to_string(), indent, Vec::new()));
                    }
                }
                Some((fence_line, indent, mut body)) => {
                    let fence: String = fence_line.chars().take_while(|c| *c == '`').collect();
                    if trimmed == fence {
                        blocks.push((fence_line, body.join("\n")));
                    } else {
                        let unindented = line
                            .strip_prefix(&" ".repeat(indent))
                            .unwrap_or(line.trim_start());
                        body.push(unindented.to_string());
                        open = Some((fence_line, indent, body));
                    }
                }
            }
        }
        assert!(open.is_none(), "unclosed fence in {markdown}");
        blocks
    }

    #[test]
    fn html_to_markdown_pre_closed_code_early_stays_one_fenced_block() {
        // The live page closes </code> inside the "chrome-devtools-mcp@latest"
        // string; everything after it must still land inside the same fence.
        let md = html_to_markdown_converter()
            .convert(CHROME_AUTO_CONNECT_HTML)
            .unwrap();
        let blocks = fenced_blocks(&md);
        assert_eq!(blocks.len(), 1, "{md}");
        let (fence_line, body) = &blocks[0];
        assert_eq!(fence_line, "```", "{md}");
        assert_eq!(
            body,
            concat!(
                "{\n",
                "  \"mcpServers\": {\n",
                "    \"chrome-devtools\": {\n",
                "      \"command\": \"npx\",\n",
                "      \"args\": [\"chrome-devtools-mcp@latest\", \"--autoConnect\"]\n",
                "    }\n",
                "  }\n",
                "}",
            ),
            "{md}"
        );
        assert!(
            md.contains(r#""args": ["chrome-devtools-mcp@latest", "--autoConnect"]"#),
            "{md}"
        );
        assert!(!md.contains("```ome"), "{md}");
    }

    #[test]
    fn html_to_markdown_pre_keeps_code_verbatim() {
        let html = "<pre><code class=\"language-md\">line *one*\n\n\n\n  # not a heading &lt;b&gt;\n</code></pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert_eq!(
            md.trim(),
            "```md\nline *one*\n\n\n\n  # not a heading <b>\n```",
            "{md:?}"
        );
    }

    #[test]
    fn html_to_markdown_code_containing_triple_backticks_gets_longer_fence() {
        let html = "<pre><code>Example:\n```rust\nfn main() {}\n```\n</code></pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert_eq!(
            md.trim(),
            "````\nExample:\n```rust\nfn main() {}\n```\n````",
            "{md:?}"
        );

        let html = "<pre><code>a ````` run</code></pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert_eq!(md.trim(), "``````\na ````` run\n``````", "{md:?}");
    }

    #[test]
    fn html_to_markdown_pre_without_code_keeps_htmd_rendering() {
        let html = "<pre>plain preformatted</pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert!(!md.contains("```"), "{md:?}");
        assert!(md.contains("plain preformatted"), "{md:?}");
    }

    #[test]
    fn html_to_markdown_pre_per_line_divs_keep_their_lines() {
        let md = html_to_markdown_converter()
            .convert(include_str!(
                "../tests/fixtures/url_html/per_line_divs.html"
            ))
            .unwrap();
        let blocks = fenced_blocks(&md);
        assert_eq!(blocks.len(), 1, "{md}");
        // The template literal's single backticks leave the fence at three.
        assert_eq!(blocks[0].0, "```js", "{md}");
        assert_eq!(
            blocks[0].1, "function greet(name) {\n  return `Hello, ${name}`;\n\n}",
            "{md}"
        );
    }

    #[test]
    fn html_to_markdown_pre_table_rows_keep_their_lines() {
        let md = html_to_markdown_converter()
            .convert(include_str!("../tests/fixtures/url_html/table_rows.html"))
            .unwrap();
        let blocks = fenced_blocks(&md);
        assert_eq!(blocks.len(), 1, "{md}");
        assert_eq!(blocks[0].0, "```python", "{md}");
        assert_eq!(
            blocks[0].1, "def add(a, b):\n    return a + b\n\nprint(add(1, 2))",
            "{md}"
        );
    }

    #[test]
    fn html_to_markdown_pre_shiki_line_spans_keep_their_lines() {
        let md = html_to_markdown_converter()
            .convert(include_str!("../tests/fixtures/url_html/shiki_lines.html"))
            .unwrap();
        let blocks = fenced_blocks(&md);
        assert_eq!(blocks.len(), 1, "{md}");
        assert_eq!(blocks[0].0, "```", "{md}");
        assert_eq!(
            blocks[0].1,
            "const total = items.reduce((sum, item) => sum + item.price, 0);\n\nconsole.log(total);",
            "{md}"
        );
    }

    #[test]
    fn html_to_markdown_pre_list_item_lines_keep_their_lines() {
        let html = "<pre><code><ol><li>first</li><li></li><li>  third</li></ol></code></pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert_eq!(md.trim(), "```\nfirst\n\n  third\n```", "{md:?}");
    }

    #[test]
    fn html_to_markdown_pre_newline_text_between_lines_is_not_doubled() {
        // Lines that already end in newline text must not gain a second break.
        let html = "<pre><code><span class=\"line\">a</span>\n<span class=\"line\">b</span>\n<div>c\n</div></code></pre>";
        let md = html_to_markdown_converter().convert(html).unwrap();
        assert_eq!(md.trim(), "```\na\nb\nc\n```", "{md:?}");
    }

    #[test]
    fn code_fence_for_outgrows_longest_backtick_run() {
        assert_eq!(code_fence_for("no ticks"), "```");
        assert_eq!(code_fence_for("`inline` ``double``"), "```");
        assert_eq!(code_fence_for("```"), "````");
        assert_eq!(code_fence_for("``` and ````"), "`````");
    }

    /// htmd decodes an HTML entity into its character and then escapes that
    /// character for Markdown, so the converted text spells a key in a form
    /// the byte matchers alone do not recognise.
    #[test]
    fn html_conversion_markdown_escapes_an_entity_decoded_character() {
        let markdown =
            convert_html_body_to_markdown(b"<p>abcdef&#42;1234567890</p>", "https://example.test/")
                .unwrap();
        let text = String::from_utf8(markdown).unwrap();
        assert!(text.contains("abcdef\\*1234567890"), "{text}");
    }

    /// The text of a failed body read can name the request URL, which held
    /// the key in a placeholder fetch; it is scrubbed before it is reported.
    #[test]
    fn a_failed_body_read_in_a_placeholder_fetch_is_scrubbed_of_held_keys() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other(
                    "error reading body for url (https://api.test/v1?key=fetch-secret-key%2F0123456789%2Babc)",
                ))
            }
        }
        let mut held = HeldKeys::default();
        held.hold("test", b"fetch-secret-key/0123456789+abc")
            .unwrap();
        let deadline = FetchDeadline::for_current_call();
        let sanitize = |detail: &str| held.sanitize_detail(detail);
        let error = read_response_body_with(Failing, "https://api.test/v1", &deadline, &sanitize)
            .expect_err("the read fails");
        let text = error.to_string();
        assert!(!text.contains("fetch-secret-key"), "{text}");
        assert!(text.contains("key={{ck-secret:test}})"), "{text}");
    }

    /// reqwest prints the request URL in its own error text; a placeholder
    /// fetch's failure must drop it (the URL held the key), not merely scrub
    /// the key out of it.
    #[test]
    fn a_placeholder_fetch_send_failure_drops_the_request_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            // Accept one connection and hold it open without answering.
            let accepted = listener.accept();
            thread::sleep(Duration::from_secs(5));
            drop(accepted);
        });
        let url = format!("http://{addr}/v1?key=fetch-secret-key%2F0123456789%2Babc");
        let error = Client::builder()
            .timeout(Duration::from_millis(300))
            .build()
            .unwrap()
            .get(&url)
            .send()
            .expect_err("the silent server times the request out");
        assert!(error.to_string().contains("for url"), "{error}");
        let mut held = HeldKeys::default();
        held.hold("test", b"fetch-secret-key/0123456789+abc")
            .unwrap();
        let deadline = FetchDeadline::for_current_call();
        let failure = send_failure(
            error,
            Some(FetchPhase::Headers),
            &deadline,
            "http://api.test/v1?key={{ck-secret:test}}",
            Some(&held),
        );
        let text = failure.to_string();
        assert!(text.contains("timed out"), "{text}");
        assert!(!text.contains("for url"), "{text}");
        assert!(!text.contains("fetch-secret-key"), "{text}");
    }

    #[test]
    fn deprecated_site_local_ipv6_is_private() {
        for address in ["fec0::1", "feff::1", "fef0:1::1"] {
            let ip: IpAddr = address.parse().unwrap();
            assert!(is_private_ip(ip), "{address}");
        }
        assert!(!is_private_ip("2606:4700::1".parse().unwrap()));
    }
}

#[cfg(test)]
/// The fetch times the slow-call line reports, measured against local
/// servers that are slow in a known phase.
#[path = "url_fetch_deadline_tests.rs"]
mod deadline_tests;

#[cfg(test)]
mod fetch_times_tests {
    use super::timing::{self, UrlFetchTimes};
    use super::*;
    use std::net::TcpListener;

    const DOCUMENT: &[u8] = b"# Guide\n\n## Install\n\nRun the installer.\n";

    /// Answers every connection: reads the request head, waits
    /// `before_headers`, sends the response headers, waits `before_body`,
    /// then sends [`DOCUMENT`] and closes.
    fn slow_server(before_headers: Duration, before_body: Duration) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("test server address");
        thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut head = Vec::new();
                    let mut buffer = [0u8; 1024];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut buffer) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => head.extend_from_slice(&buffer[..n]),
                        }
                    }
                    thread::sleep(before_headers);
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/markdown\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        DOCUMENT.len()
                    );
                    if stream.write_all(headers.as_bytes()).is_err() {
                        return;
                    }
                    let _ = stream.flush();
                    thread::sleep(before_body);
                    let _ = stream.write_all(DOCUMENT);
                });
            }
        });
        address
    }

    fn local() -> UrlFetchOptions {
        UrlFetchOptions {
            allow_private: true,
            ..UrlFetchOptions::default()
        }
    }

    /// One fetch under a call budget, inside a fetch-times scope.
    fn timed_fetch(
        budget: Duration,
        url: &str,
        storage: &Path,
    ) -> (Result<PathBuf, UrlFetchError>, UrlFetchTimes) {
        let _call = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(budget)));
        let scope = timing::begin();
        let result = fetch_url_to_cache(url, storage, local());
        let times = scope.finish().expect("the fetch was counted");
        (result, times)
    }

    #[test]
    fn url_fetch_times_split_a_slow_download_and_count_a_cache_hit() {
        let storage = tempfile::tempdir().unwrap();
        let address = slow_server(Duration::from_millis(300), Duration::from_millis(200));
        let url = format!("http://127.0.0.1:{}/guide.md", address.port());

        let (result, times) = timed_fetch(Duration::from_secs(10), &url, storage.path());
        result.expect("the slow server answers within the deadline");
        assert_eq!(times.fetches, 1, "{times:?}");
        assert_eq!(times.cache_hits, 0, "{times:?}");
        assert_eq!(times.remembered_failures, 0, "{times:?}");
        assert_eq!((times.redirects, times.retries), (0, 0), "{times:?}");
        assert_eq!(times.error, None, "{times:?}");
        // The server held the headers 300 ms after it read the request, and
        // the body another 200 ms after the headers.
        assert!(times.first_byte >= Duration::from_millis(300), "{times:?}");
        assert!(times.connect <= times.first_byte, "{times:?}");
        assert!(times.body >= Duration::from_millis(150), "{times:?}");
        assert_eq!(times.body_bytes, DOCUMENT.len() as u64, "{times:?}");
        let rendered = times.render();
        assert!(
            rendered.starts_with("url_fetch=fetches:1,cache_hits:0,remembered:0,"),
            "{rendered}"
        );
        assert!(!rendered.contains("guide"), "{rendered}");
        assert!(!rendered.contains(' '), "{rendered}");

        // The same URL again is served from the cache: no request, no body.
        let (result, times) = timed_fetch(Duration::from_secs(10), &url, storage.path());
        result.expect("the cached copy is served");
        assert_eq!((times.fetches, times.cache_hits), (1, 1), "{times:?}");
        assert_eq!(times.first_byte, Duration::ZERO, "{times:?}");
        assert_eq!(times.body_bytes, 0, "{times:?}");
    }

    #[test]
    fn url_fetch_times_name_a_timeout_and_its_phase() {
        let storage = tempfile::tempdir().unwrap();
        let address = slow_server(Duration::from_secs(60), Duration::ZERO);
        let url = format!("http://127.0.0.1:{}/silent.md", address.port());
        // A 5 s call budget leaves the fetch a 2 s deadline (see
        // `FetchDeadline::for_current_call`).
        let (result, times) = timed_fetch(Duration::from_secs(5), &url, storage.path());
        let error = result.expect_err("a server that never answers fails");
        assert_eq!(error.code(), "url_fetch_timeout", "{error}");
        assert_eq!(
            times.error,
            Some(("url_fetch_timeout", Some(FetchPhase::Headers))),
            "{times:?}"
        );
        assert!(
            times.first_byte >= Duration::from_millis(1_500),
            "{times:?}"
        );
        assert_eq!(times.body_bytes, 0, "{times:?}");
        assert!(
            times.render().ends_with(",error:url_fetch_timeout/headers"),
            "{}",
            times.render()
        );
    }

    #[test]
    fn url_fetch_times_collect_only_inside_the_outermost_scope() {
        timing::record(|times| times.fetches += 1);
        assert_eq!(timing::begin().finish(), None, "nothing ran in this scope");

        let outer = timing::begin();
        let inner = timing::begin();
        timing::record(|times| times.fetches += 1);
        assert_eq!(inner.finish(), None);
        timing::record(|times| times.fetches += 1);
        assert_eq!(outer.finish().map(|times| times.fetches), Some(2));

        drop(timing::begin());
        timing::record(|times| times.fetches += 1);
        assert_eq!(timing::begin().finish(), None, "a dropped scope closed");
    }

    #[test]
    fn url_fetch_times_render_every_field_in_milliseconds() {
        let times = UrlFetchTimes {
            fetches: 2,
            cache_hits: 1,
            remembered_failures: 0,
            lookup: Duration::from_micros(2_100),
            connect: Duration::from_millis(40),
            first_byte: Duration::from_micros(5_800_004),
            body: Duration::from_millis(300),
            body_bytes: 51_234,
            convert: Duration::from_millis(4),
            redirects: 1,
            retries: 0,
            error: Some(("url_fetch_failed", None)),
        };
        assert_eq!(
            times.render(),
            "url_fetch=fetches:2,cache_hits:1,remembered:0,lookup:2.10,connect:40.00,first_byte:5800.00,body:300.00,bytes:51234,convert:4.00,redirects:1,retries:0,error:url_fetch_failed"
        );
    }
}

#[cfg(test)]
mod secret_audit_regressions {
    use super::*;
    #[test]
    fn substituted_query_allocates_its_exact_complete_size_before_copying_any_key() {
        let first = Zeroizing::new(vec![b'a'; 1001]);
        let second = Zeroizing::new(vec![b'b'; 2103]);
        let result = substitute_query(b"a=x&b=y", &[(2, 3, first), (6, 7, second)]).unwrap();
        assert_eq!(result.len(), 5 + 1001 + 2103);
        assert_eq!(
            result.capacity(),
            result.len(),
            "a later key must not reallocate a buffer that already contains an earlier key"
        );
        assert_eq!(&result[..2], b"a=");
        assert_eq!(&result[1003..1006], b"&b=");
    }
    #[test]
    fn approval_wait_preserves_the_fetch_and_read_back_network_budget() {
        let _scope = crate::bounded_io::enter(Some(crate::bounded_io::Budget::after(
            Duration::from_secs(25),
        )));
        let before = crate::bounded_io::current().unwrap().deadline;
        let deadline = FetchDeadline::for_current_call();
        let person_wait = Duration::from_secs(360);
        crate::bounded_io::exclude_secret_approval_wait(person_wait);
        assert_eq!(
            crate::bounded_io::current().unwrap().deadline,
            before + person_wait
        );
        assert_eq!(deadline.allowed, URL_FETCH_CEILING);
    }
    #[test]
    fn failed_audit_does_not_create_an_ordinary_key_bearing_url() {
        struct NoAudit;
        impl crate::secret::SecretVault for NoAudit {
            fn use_secret(
                &self,
                _: &crate::secret::SecretScope,
                _: &str,
                _: &crate::secret::ApiKeyTarget,
            ) -> crate::secret::UseAnswer {
                crate::secret::UseAnswer::Released(crate::secret::ReleasedApiKey::new(
                    "query:key".into(),
                    crate::secret::Material::new(b"released-test-key-123456".to_vec()),
                ))
            }
            fn audit(&self, _: &AuditRecordV1) -> Result<(), crate::secret::AuditError> {
                Err(crate::secret::AuditError::Unavailable {
                    detail: "offline".into(),
                })
            }
        }
        let root = tempfile::tempdir().unwrap();
        let stamp: subc_protocol::scope::ScopeStamp = serde_json::from_value(serde_json::json!({"owner":{"kind":"reserved","module_id":"prefrontal-core"},"ref":"audit-url","kind":"head","scope_epoch":1,"owner_authorized":true})).unwrap();
        let scope = crate::secret::SecretScope::from_stamp(&stamp);
        SECRET_URL_BUILDS.with(|n| n.set(0));
        crate::subc::with_secret_identity_for_test(root.path(), stamp, || {
            let secrets = SecretFetch {
                vault: Arc::new(NoAudit),
                authorization: Some(scope),
                labels: vec!["test".into()],
                test_https_port: None,
                call_id: "audit-url".into(),
                cancellation: Default::default(),
            };
            let mut held = HeldKeys::default();
            assert!(secret_hop(
                &Url::parse("https://example.test/doc.md?key={{ck-secret:test}}").unwrap(),
                0,
                &secrets,
                &mut held
            )
            .is_err());
        });
        assert_eq!(
            SECRET_URL_BUILDS.with(std::cell::Cell::get),
            0,
            "audit refusal must precede construction of any Url that owns a key"
        );
    }
}

#[cfg(test)]
#[path = "url_fetch_sensitive.rs"]
mod sensitive;
