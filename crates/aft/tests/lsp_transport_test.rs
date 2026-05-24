use std::io::{BufReader, Cursor, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use aft::lsp::jsonrpc::{Notification, Request, RequestId, ServerMessage};
use aft::lsp::transport::{read_message, write_notification, write_request};
use serde_json::json;

fn framed_message(json: &str) -> Vec<u8> {
    format!("Content-Length: {}\r\n\r\n{}", json.len(), json).into_bytes()
}

fn fake_server_binary() -> PathBuf {
    option_env!("CARGO_BIN_EXE_fake-lsp-server")
        .or(option_env!("CARGO_BIN_EXE_fake_lsp_server"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake-lsp-server").map(PathBuf::from))
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_fake_lsp_server").map(PathBuf::from))
        .or_else(|| {
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let workspace_root = manifest_dir.parent()?.parent()?;
            Some(
                workspace_root
                    .join("target")
                    .join("debug")
                    .join("fake-lsp-server"),
            )
        })
        .filter(|path| path.exists())
        .expect("fake-lsp-server binary path not set")
}

struct FakeServerProcess {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl FakeServerProcess {
    fn spawn() -> Self {
        let mut child = Command::new(fake_server_binary())
            // Disable the fake server's didChangeWatchedFiles dynamic registration.
            // This transport test verifies raw initialize/diagnostics roundtrip; the
            // registerCapability request the fake server otherwise sends after
            // `initialized` would arrive before our publishDiagnostics and break
            // the simple expect-diagnostics-first loop below.
            .env("AFT_FAKE_LSP_NO_WATCHED_FILES", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn fake-lsp-server");

        let stdout = child.stdout.take().expect("missing stdout handle");
        let reader = BufReader::new(stdout);

        Self { child, reader }
    }

    fn send_request(&mut self, request: &Request) {
        let stdin = self.child.stdin.as_mut().expect("missing stdin handle");
        write_request(stdin, request).expect("failed to write request");
    }

    fn send_notification(&mut self, notification: &Notification) {
        let stdin = self.child.stdin.as_mut().expect("missing stdin handle");
        write_notification(stdin, notification).expect("failed to write notification");
    }

    fn recv(&mut self) -> ServerMessage {
        read_message(&mut self.reader)
            .expect("failed to read server message")
            .expect("expected server message, got EOF")
    }

    fn wait(mut self) -> (std::process::ExitStatus, String) {
        drop(self.child.stdin.take());
        let status = self.child.wait().expect("failed to wait on child");
        let mut stderr = String::new();
        if let Some(mut child_stderr) = self.child.stderr.take() {
            child_stderr
                .read_to_string(&mut stderr)
                .expect("failed to read child stderr");
        }
        (status, stderr)
    }
}

#[test]
fn test_write_and_read_request() {
    let request = Request::new(
        RequestId::Int(1),
        "initialize",
        Some(json!({"capabilities": {"hover": true}})),
    );

    let mut buffer = Vec::new();
    write_request(&mut buffer, &request).unwrap();

    let mut reader = Cursor::new(buffer);
    let message = read_message(&mut reader).unwrap().unwrap();

    match message {
        ServerMessage::Request { id, method, params } => {
            assert_eq!(id, RequestId::Int(1));
            assert_eq!(method, "initialize");
            assert_eq!(params, Some(json!({"capabilities": {"hover": true}})));
        }
        other => panic!("expected request, got {other:?}"),
    }
}

#[test]
fn test_write_and_read_notification() {
    let notification = Notification::new(
        "initialized",
        Some(json!({"clientInfo": {"name": "aft-tests"}})),
    );

    let mut buffer = Vec::new();
    write_notification(&mut buffer, &notification).unwrap();

    let mut reader = Cursor::new(buffer);
    let message = read_message(&mut reader).unwrap().unwrap();

    match message {
        ServerMessage::Notification { method, params } => {
            assert_eq!(method, "initialized");
            assert_eq!(params, Some(json!({"clientInfo": {"name": "aft-tests"}})));
        }
        other => panic!("expected notification, got {other:?}"),
    }
}

#[test]
fn test_read_response() {
    let payload = framed_message(
        r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"textDocumentSync":1}}}"#,
    );
    let mut reader = Cursor::new(payload);

    match read_message(&mut reader).unwrap().unwrap() {
        ServerMessage::Response(response) => {
            assert_eq!(response.id, RequestId::Int(1));
            assert_eq!(
                response.result,
                Some(json!({"capabilities": {"textDocumentSync": 1}}))
            );
            assert!(response.error.is_none());
        }
        other => panic!("expected response, got {other:?}"),
    }
}

#[test]
fn test_read_response_with_error() {
    let payload = framed_message(
        r#"{"jsonrpc":"2.0","id":"shutdown","error":{"code":-32601,"message":"method not found","data":{"method":"bogus"}}}"#,
    );
    let mut reader = Cursor::new(payload);

    match read_message(&mut reader).unwrap().unwrap() {
        ServerMessage::Response(response) => {
            assert_eq!(response.id, RequestId::String("shutdown".to_string()));
            assert!(response.result.is_none());
            let error = response.error.expect("expected error payload");
            assert_eq!(error.code, -32601);
            assert_eq!(error.message, "method not found");
            assert_eq!(error.data, Some(json!({"method": "bogus"})));
        }
        other => panic!("expected response, got {other:?}"),
    }
}

#[test]
fn test_read_server_request() {
    let payload = framed_message(
        r#"{"jsonrpc":"2.0","id":"req-1","method":"workspace/configuration","params":{"items":[]}}"#,
    );
    let mut reader = Cursor::new(payload);

    match read_message(&mut reader).unwrap().unwrap() {
        ServerMessage::Request { id, method, params } => {
            assert_eq!(id, RequestId::String("req-1".to_string()));
            assert_eq!(method, "workspace/configuration");
            assert_eq!(params, Some(json!({"items": []})));
        }
        other => panic!("expected request, got {other:?}"),
    }
}

#[test]
fn test_read_eof_returns_none() {
    let mut reader = Cursor::new(Vec::<u8>::new());
    assert!(read_message(&mut reader).unwrap().is_none());
}

#[test]
fn test_read_malformed_header() {
    let mut reader = Cursor::new(b"Content-Type: application/vscode-jsonrpc\r\n\r\n".to_vec());
    let error = read_message(&mut reader).expect_err("expected header parse failure");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("missing Content-Length"));
}

#[test]
fn test_read_extra_headers_ignored() {
    let json = r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
    let payload = format!(
        "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n{}",
        json.len(),
        json
    )
    .into_bytes();
    let mut reader = Cursor::new(payload);

    match read_message(&mut reader).unwrap().unwrap() {
        ServerMessage::Notification { method, params } => {
            assert_eq!(method, "initialized");
            assert_eq!(params, Some(json!({})));
        }
        other => panic!("expected notification, got {other:?}"),
    }
}

#[test]
fn test_server_message_classification() {
    let request = ServerMessage::from_json(
        r#"{"jsonrpc":"2.0","id":1,"method":"workspace/configuration","params":{"items":[]}}"#,
    )
    .unwrap();
    assert!(matches!(request, ServerMessage::Request { .. }));

    let response = ServerMessage::from_json(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#).unwrap();
    assert!(matches!(response, ServerMessage::Response(_)));

    let notification = ServerMessage::from_json(
        r#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":3}}"#,
    )
    .unwrap();
    assert!(matches!(notification, ServerMessage::Notification { .. }));
}

#[test]
fn test_request_id_int_and_string() {
    let int_request = Request::new(RequestId::Int(7), "shutdown", None);
    let string_request = Request::new(RequestId::String("abc".to_string()), "shutdown", None);

    assert_eq!(serde_json::to_value(int_request).unwrap()["id"], json!(7));
    assert_eq!(
        serde_json::to_value(string_request).unwrap()["id"],
        json!("abc")
    );

    match ServerMessage::from_json(r#"{"jsonrpc":"2.0","id":7,"result":null}"#).unwrap() {
        ServerMessage::Response(response) => assert_eq!(response.id, RequestId::Int(7)),
        other => panic!("expected response, got {other:?}"),
    }

    match ServerMessage::from_json(r#"{"jsonrpc":"2.0","id":"abc","result":null}"#).unwrap() {
        ServerMessage::Response(response) => {
            assert_eq!(response.id, RequestId::String("abc".to_string()))
        }
        other => panic!("expected response, got {other:?}"),
    }
}

#[test]
fn test_roundtrip_with_fake_server() {
    let mut server = FakeServerProcess::spawn();

    server.send_request(&Request::new(
        RequestId::Int(1),
        "initialize",
        Some(json!({
            "processId": null,
            "rootUri": "file:///tmp",
            "capabilities": {}
        })),
    ));

    match server.recv() {
        ServerMessage::Response(response) => {
            assert_eq!(response.id, RequestId::Int(1));
            let result = response.result.expect("expected initialize result");
            assert_eq!(result["capabilities"]["textDocumentSync"], json!(1));
            assert_eq!(result["serverInfo"]["name"], json!("fake-lsp-server"));
        }
        other => panic!("expected initialize response, got {other:?}"),
    }

    server.send_notification(&Notification::new("initialized", Some(json!({}))));
    server.send_notification(&Notification::new(
        "textDocument/didOpen",
        Some(json!({
            "textDocument": {
                "uri": "file:///tmp/test.rs",
                "languageId": "rust",
                "version": 1,
                "text": "fn main() {}"
            }
        })),
    ));

    let diagnostics = loop {
        match server.recv() {
            ServerMessage::Notification { method, params } => {
                if method == "textDocument/publishDiagnostics" {
                    break params.expect("expected diagnostics params");
                }
            }
            other => panic!("expected diagnostics notification, got {other:?}"),
        }
    };

    assert_eq!(diagnostics["uri"], json!("file:///tmp/test.rs"));
    assert_eq!(diagnostics["diagnostics"].as_array().unwrap().len(), 2);
    assert_eq!(
        diagnostics["diagnostics"][0]["message"],
        json!("test diagnostic error")
    );
    assert_eq!(diagnostics["diagnostics"][0]["severity"], json!(1));
    assert_eq!(diagnostics["diagnostics"][0]["code"], json!("E0001"));
    assert_eq!(
        diagnostics["diagnostics"][1]["message"],
        json!("test diagnostic warning")
    );
    assert_eq!(diagnostics["diagnostics"][1]["severity"], json!(2));

    server.send_request(&Request::new(
        RequestId::String("shutdown".to_string()),
        "shutdown",
        None,
    ));

    match server.recv() {
        ServerMessage::Response(response) => {
            assert_eq!(response.id, RequestId::String("shutdown".to_string()));
            assert!(response.result.is_none());
            assert!(response.error.is_none());
        }
        other => panic!("expected shutdown response, got {other:?}"),
    }

    server.send_notification(&Notification::new("exit", None));
    let (status, stderr) = server.wait();
    assert!(status.success(), "fake server exited with stderr: {stderr}");
}
