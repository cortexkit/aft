use super::super::{carrier, operations};
use super::*;

fn cursor<'a>(session: &'a str, task: &'a str) -> Cursor<'a> {
    Cursor {
        key: [0x42; 32],
        harness: "mcp:a/b",
        session,
        task,
    }
}

fn backend(stdout: &[u8], stderr: &[u8], offsets: Offsets, terminal: bool, pty: bool) -> Value {
    let read = |bytes: &[u8], offset: u64| {
        let start = offset as usize;
        let end = (start + 65_536).min(bytes.len());
        (
            base64::engine::general_purpose::STANDARD.encode(&bytes[start..end]),
            end < bytes.len(),
        )
    };
    let (out, out_more) = read(stdout, offsets.stdout);
    let (err, err_more) = read(stderr, offsets.stderr);
    json!({"status": if terminal { "completed" } else { "running" },
        "mode":if pty { "pty" } else { "pipes" },
        "output_chunk_base64":out, "output_has_more":out_more,
        "stderr_chunk_base64":err, "stderr_has_more":err_more})
}

fn validate(page: &Value) {
    let entry = operations::operations()
        .into_iter()
        .find(|op| op["id"] == "bash_output")
        .unwrap();
    super::super::schema::validate(&carrier::result_schema(&entry), page).unwrap();
    assert!(page.get("total").is_none());
    assert!(serde_json::to_vec(page).unwrap().len() <= PAGE_BYTES);
}

fn collect(stdout: &[u8], stderr: &[u8], limit: usize, pty: bool) -> [String; 2] {
    let c = cursor("session", "task");
    let mut offsets = Offsets::default();
    let mut texts = [String::new(), String::new()];
    let mut counts = [0usize; 2];
    for _ in 0..200 {
        let data = backend(stdout, stderr, offsets, true, pty);
        let result = page(&c, offsets, &data, limit).unwrap();
        validate(&result);
        for item in result["items"].as_array().unwrap() {
            let stream = item["stream"].as_str().unwrap();
            assert!(!pty || stream == "pty");
            let index = usize::from(stream == "stderr");
            let text = item["text"].as_str().unwrap();
            let count = item["bytes"].as_u64().unwrap() as usize;
            assert!(text.len() <= ITEM_BYTES && count > 0 && count <= ITEM_BYTES);
            texts[index].push_str(text);
            counts[index] += count;
        }
        if result["next"].is_null() {
            assert_eq!(counts, [stdout.len(), stderr.len()]);
            assert_eq!(texts[0], String::from_utf8_lossy(stdout));
            assert_eq!(texts[1], String::from_utf8_lossy(stderr));
            return texts;
        }
        let advanced = c.decode(result["next"].as_str().unwrap()).unwrap();
        assert!(advanced.stdout > offsets.stdout || advanced.stderr > offsets.stderr);
        offsets = advanced;
    }
    panic!("page did not drain");
}

#[test]
fn output_pages_cover_invalid_utf8_multibyte_cuts_and_item_limits() {
    for boundary in [8191, 8192, 65_535, 65_536] {
        for sequence in [
            b"\xe2\x82\xac".as_slice(),
            b"\xe2\x82X",
            b"\xf0\x9fX",
            b"\xe2\x82",
        ] {
            let mut bytes = vec![b'a'; boundary];
            bytes.extend(sequence);
            collect(&bytes, b"err\xff\n", 1, false);
        }
    }
    collect(&vec![0xff; 65_536], b"", 200, false);
    collect(b"\x1b[31mred\x1b[0m\r\n", b"", 1, true);
}

#[test]
fn output_page_reads_one_backend_page_per_stream_in_stdout_then_stderr_order() {
    let c = cursor("s", "t");
    let out = vec![b'o'; 65_537];
    let err = b"stderr";
    let result = page(
        &c,
        Offsets::default(),
        &backend(&out, err, Offsets::default(), true, false),
        200,
    )
    .unwrap();
    validate(&result);
    let items = result["items"].as_array().unwrap();
    assert_eq!(items.len(), 9);
    assert!(items[..8].iter().all(|item| item["stream"] == "stdout"));
    assert_eq!(items[8]["stream"], "stderr");
    let next = c.decode(result["next"].as_str().unwrap()).unwrap();
    assert_eq!((next.stdout, next.stderr), (65_536, 6));
    collect(&out, err, 1, false);
}

#[test]
fn output_pages_account_for_json_escaping_and_leave_unreached_stream_offsets() {
    let c = cursor("s", "t");
    let bytes = vec![0; 65_536];
    let result = page(
        &c,
        Offsets::default(),
        &backend(&bytes, &bytes, Offsets::default(), true, false),
        200,
    )
    .unwrap();
    validate(&result);
    assert!(serde_json::to_vec(&result).unwrap().len() > 240_000);
    let next = c.decode(result["next"].as_str().unwrap()).unwrap();
    assert_eq!(next.stdout, 5 * 8192);
    assert_eq!(next.stderr, 0);
    collect(&bytes, &bytes, 200, false);
}

#[test]
fn output_incomplete_tail_waits_for_completion_or_invalidating_byte_or_exit() {
    let c = cursor("s", "t");
    for boundary in [8191, 65_535] {
        let mut bytes = vec![b'a'; boundary];
        bytes.extend([0xe2, 0x82]);
        let mut offsets = Offsets::default();
        let mut text = String::new();
        loop {
            let result = page(
                &c,
                offsets,
                &backend(&bytes, b"", offsets, false, false),
                200,
            )
            .unwrap();
            validate(&result);
            for item in result["items"].as_array().unwrap() {
                text.push_str(item["text"].as_str().unwrap());
            }
            let next = c.decode(result["next"].as_str().unwrap()).unwrap();
            if next.stdout == offsets.stdout {
                break;
            }
            offsets = next;
        }
        assert_eq!(offsets.stdout as usize, boundary);
        assert_eq!(text, "a".repeat(boundary));
        for suffix in [b"\xac".as_slice(), b"X", b""] {
            let mut completed = bytes.clone();
            completed.extend(suffix);
            let result = page(
                &c,
                offsets,
                &backend(&completed, b"", offsets, true, false),
                200,
            )
            .unwrap();
            validate(&result);
            assert_eq!(
                result["items"][0]["text"],
                String::from_utf8_lossy(&completed[boundary..]).as_ref()
            );
            assert_eq!(result["items"][0]["bytes"], 2 + suffix.len());
            assert!(result["next"].is_null());
        }
    }
}

#[test]
fn output_exhausted_running_stream_can_grow_and_cursor_rejects_other_identities() {
    let session = "s".repeat(900);
    let c = cursor(&session, "task");
    let result = page(
        &c,
        Offsets::default(),
        &backend(b"", b"", Offsets::default(), false, false),
        1,
    )
    .unwrap();
    assert_eq!(result["items"], json!([]));
    let encoded = result["next"].as_str().unwrap();
    assert!(encoded.len() <= 1024);
    let offsets = c.decode(encoded).unwrap();
    let empty_again = page(&c, offsets, &backend(b"", b"", offsets, false, false), 1).unwrap();
    assert_eq!(empty_again["items"], json!([]));
    assert_ne!(empty_again["next"], encoded);
    let result = page(
        &c,
        offsets,
        &backend(b"new", b"err", offsets, false, false),
        1,
    )
    .unwrap();
    assert_eq!(result["items"][0]["text"], "new");
    let next = c.decode(result["next"].as_str().unwrap()).unwrap();
    assert_eq!((next.stdout, next.stderr), (3, 0));
    assert!(cursor("other", "task").decode(encoded).is_none());
    assert!(cursor(&session, "other").decode(encoded).is_none());
    let mut other_harness = cursor(&session, "task");
    other_harness.harness = "other";
    assert!(other_harness.decode(encoded).is_none());
    let mut other_key = cursor(&session, "task");
    other_key.key[0] ^= 1;
    assert!(other_key.decode(encoded).is_none());
    let mut bytes = URL_SAFE_NO_PAD.decode(encoded).unwrap();
    bytes[1] ^= 1;
    assert!(c.decode(&URL_SAFE_NO_PAD.encode(&bytes)).is_none());
    bytes[0] = 2;
    assert!(c.decode(&URL_SAFE_NO_PAD.encode(&bytes)).is_none());
    for invalid in ["!", "", &"a".repeat(1025)] {
        assert!(c.decode(invalid).is_none());
    }
}

#[test]
fn cursor_mac_matches_independent_hmac_sha256_vector() {
    let c = cursor("session", "task");
    // Python's hmac.digest over compact JSON identity plus the binary payload.
    let expected = "AQAAAAAAAAAHAAAAAAAAAAsAAAAAAAAAA_3ae2v-CyA0Og34gE8Mr0JmglrN9t8c8S5F20jkvw1R";
    assert_eq!(
        c.encode(Offsets {
            stdout: 7,
            stderr: 11,
            read_sequence: 3
        }),
        expected
    );
    let decoded = c.decode(expected).unwrap();
    assert_eq!(
        (decoded.stdout, decoded.stderr, decoded.read_sequence),
        (7, 11, 3)
    );
}
