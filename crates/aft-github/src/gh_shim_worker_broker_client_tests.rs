use super::*;

fn read(repository: &str) -> Operation {
    Operation::ReadRequest {
        request: serde_json::from_value(json!({
            "action":"github.repo.get", "repository":repository, "args":{"path":""}
        }))
        .unwrap(),
    }
}

// A refusal carries the daemon's explanation (for an unknown query key it
// names the key and the allowed ones). The worker-facing error must keep it,
// or a refused command can't be corrected without reading broker traffic.
#[cfg(unix)]
#[test]
fn broker_refusal_error_keeps_the_daemon_message() {
    use std::io::{Read, Write};
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("broker.sock");
    let capability_file = root.path().join("capability");
    std::fs::write(&capability_file, "0123456789abcdef0123456789abcdef").unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut size = [0u8; 4];
        stream.read_exact(&mut size).unwrap();
        let mut request = vec![0u8; u32::from_be_bytes(size) as usize];
        stream.read_exact(&mut request).unwrap();
        let reply = serde_json::to_vec(&serde_json::json!({
            "status": "error",
            "data": {
                "refusal_code": "worker_query_key_not_allowed",
                "message": "query key `sort` is not allowed; allowed keys: page, per_page"
            }
        }))
        .unwrap();
        stream
            .write_all(&(reply.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&reply).unwrap();
    });
    let client = Client {
        socket,
        capability_file,
    };
    let error = request(&client, Operation::SessionContext, Duration::from_secs(5)).unwrap_err();
    server.join().unwrap();
    assert_eq!(
        error,
        "worker read broker refused: worker_query_key_not_allowed: query key `sort` is not allowed; allowed keys: page, per_page"
    );
}

fn broker_reply_roundtrip(data: Value) -> Result<Value, String> {
    let root = tempfile::tempdir().unwrap();
    let socket = root.path().join("broker.sock");
    let capability_file = root.path().join("capability");
    std::fs::write(&capability_file, "0123456789abcdef0123456789abcdef").unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let listener = tokio::net::UnixListener::from_std(listener).unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            read_frame(&mut stream, MAX_FRAME).await.unwrap();
            write_reply(&mut stream, RelayReply { ok: true, data })
                .await
                .unwrap();
        });
    });
    let result = request(
        &Client {
            socket,
            capability_file,
        },
        read("cortexkit/aft"),
        Duration::from_secs(10),
    );
    server.join().unwrap();
    result
}

#[test]
fn gh_shim_broker_large_read_replies_fit_the_body_cap() {
    let cap = crate::gh_shim::read_contract::MAX_REPLY_BYTES as usize;
    for body in [
        "+line with a quote: \"\\\"\n".repeat(32_000),
        "\0".repeat(cap),
    ] {
        let result = json!({"status":200,"body":body,"next_page":null});
        let data = json!({
            "isError":false,
            "result":result,
            "structuredContent":result,
            "content":[{"type":"text","text":serde_json::to_string_pretty(&result).unwrap()}]
        });
        let legacy_size = serde_json::to_vec(&json!({"status":"ok","data":data}))
            .unwrap()
            .len();
        assert!(body.len() <= cap);
        assert!(
            legacy_size > 2_097_152,
            "fixture must exceed the old wire cap"
        );
        let reply = broker_reply_roundtrip(data).expect("valid bodies must survive broker framing");
        assert_eq!(reply["result"]["body"], body);
        assert!(reply.get("content").is_none());
        assert!(reply.get("structuredContent").is_none());
    }
    let error = broker_reply_roundtrip(json!({
        "isError":false,
        "result":{"status":200,"body":"x".repeat(cap + 1),"next_page":null}
    }))
    .unwrap_err();
    assert!(error.contains("response_too_large"), "{error}");
    assert!(error.contains("1048577 bytes"), "{error}");
    assert!(error.contains("1048576 bytes"), "{error}");
    assert!(!error.contains("reply unavailable"), "{error}");

    // A lean Plexus reply (no `isError`) is held to the same body cap.
    let error = broker_reply_roundtrip(json!({
        "result":{"status":200,"body":"x".repeat(cap + 1),"next_page":null}
    }))
    .unwrap_err();
    assert!(error.contains("response_too_large"), "{error}");
    let lean = json!({"result":{"status":200,"body":"ok","next_page":null}});
    assert_eq!(broker_reply_roundtrip(lean.clone()).unwrap(), lean);
}

#[test]
fn gh_shim_broker_reply_shapes_keep_the_decoder_contract() {
    for data in [
        json!({"isError":false,"result":{"status":200,"body":{"files":["a","b"]},"next_page":null}}),
        json!({"isError":false,"result":{"items":[{"name":"a"}],"total_count":1,"incomplete_results":false,"next_page":null}}),
        json!({"isError":false,"result":{"body":"log\n","offset":0,"length":4,"total_bytes":4}}),
        json!({"isError":true,"error":{"code":"response_too_large","transmission":"not_sent","message":"upstream body exceeds 1048576 bytes"}}),
        json!({"isError":false,"result":{},"error":{"code":"ambiguous"}}),
    ] {
        assert_eq!(broker_reply_roundtrip(data.clone()).unwrap(), data);
    }
    let size = MAX_ENCODED_RESPONSE + 1;
    let error = broker_reply_roundtrip(json!({"unexpected":"x".repeat(size)})).unwrap_err();
    assert!(error.contains("response_too_large"), "{error}");
    assert!(
        error.contains(&format!("wire cap is {MAX_ENCODED_RESPONSE} bytes")),
        "{error}"
    );
    assert!(error.contains("read body cap is 1048576 bytes"), "{error}");
}

#[tokio::test]
async fn gh_shim_broker_chunk_lengths_are_bounded() {
    for (header, chunk, expected) in [
        (
            json!({"status":"chunked","bytes":MAX_ENCODED_RESPONSE + 1}),
            None,
            format!(
                "{} bytes; cap is {MAX_ENCODED_RESPONSE} bytes",
                MAX_ENCODED_RESPONSE + 1
            ),
        ),
        (
            json!({"status":"chunked","bytes":0}),
            None,
            "0 bytes; cap is".to_string(),
        ),
        (
            json!({"status":"chunked","bytes":"unknown"}),
            None,
            "chunk length malformed".to_string(),
        ),
        (
            json!({"status":"chunked","bytes":1}),
            Some(vec![b'x'; 2]),
            "frame is 2 bytes; cap is 1 bytes".to_string(),
        ),
        (
            json!({"status":"chunked","bytes":MAX_FRAME + 1}),
            Some(vec![b'x'; MAX_FRAME + 1]),
            format!("frame is {} bytes; cap is {MAX_FRAME} bytes", MAX_FRAME + 1),
        ),
        (
            json!({"status":"chunked","bytes":10}),
            Some(vec![b'x'; 1]),
            "reply chunk failed".to_string(),
        ),
    ] {
        let (mut writer, mut reader) = tokio::io::duplex(MAX_FRAME * 2);
        write_frame(&mut writer, &serde_json::to_vec(&header).unwrap())
            .await
            .unwrap();
        if let Some(chunk) = chunk {
            write_frame(&mut writer, &chunk).await.unwrap();
        }
        drop(writer);
        let error = read_reply(&mut reader).await.unwrap_err();
        assert!(error.contains(&expected), "{error}");
    }
}
