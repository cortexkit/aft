#![allow(clippy::collapsible_match)]

use std::io::{self, BufReader, Write};

use aft::lsp::jsonrpc::{Notification, RequestId, ServerMessage};
use aft::lsp::transport::{read_message, write_notification};
use serde_json::{json, Value};

fn write_json_message(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    let json = serde_json::to_string(value)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    write!(writer, "Content-Length: {}\r\n\r\n", json.len())?;
    writer.write_all(json.as_bytes())?;
    writer.flush()
}

fn write_response(writer: &mut impl Write, id: RequestId, result: Value) -> io::Result<()> {
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
    )
}

fn write_request(writer: &mut impl Write, id: i64, method: &str, params: Value) -> io::Result<()> {
    write_json_message(
        writer,
        &json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }),
    )
}

fn request_position(params: &Option<Value>) -> (u64, u64) {
    let line = params
        .as_ref()
        .and_then(|value| value.get("position"))
        .and_then(|value| value.get("line"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    let character = params
        .as_ref()
        .and_then(|value| value.get("position"))
        .and_then(|value| value.get("character"))
        .and_then(|value| value.as_u64())
        .unwrap_or(0);
    (line, character)
}

fn request_document_uri(params: &Option<Value>) -> Value {
    params
        .as_ref()
        .and_then(|value| value.get("textDocument"))
        .and_then(|value| value.get("uri"))
        .cloned()
        .unwrap_or_else(|| Value::String("file:///unknown".to_string()))
}

fn request_include_declaration(params: &Option<Value>) -> bool {
    params
        .as_ref()
        .and_then(|value| value.get("context"))
        .and_then(|value| value.get("includeDeclaration"))
        .and_then(|value| value.as_bool())
        .unwrap_or(true)
}

fn request_new_name(params: &Option<Value>) -> String {
    params
        .as_ref()
        .and_then(|value| value.get("newName"))
        .and_then(|value| value.as_str())
        .unwrap_or("renamed")
        .to_string()
}

fn request_document_uri_string(params: &Option<Value>) -> String {
    params
        .as_ref()
        .and_then(|value| value.get("textDocument"))
        .and_then(|value| value.get("uri"))
        .and_then(|value| value.as_str())
        .unwrap_or("file:///unknown")
        .to_string()
}

fn document_uri(params: &Option<Value>) -> Value {
    params
        .as_ref()
        .and_then(|value| value.get("textDocument"))
        .and_then(|value| value.get("uri"))
        .cloned()
        .unwrap_or_else(|| Value::String("file:///unknown".to_string()))
}

fn document_version(params: &Option<Value>) -> Value {
    params
        .as_ref()
        .and_then(|value| value.get("textDocument"))
        .and_then(|value| value.get("version"))
        .cloned()
        .unwrap_or(Value::Null)
}

fn write_custom_notification(
    writer: &mut impl Write,
    method: &str,
    uri: Value,
    version: Value,
) -> io::Result<()> {
    write_notification(
        writer,
        &Notification::new(
            method,
            Some(json!({
                "uri": uri,
                "version": version,
            })),
        ),
    )
}

fn write_publish_diagnostics(
    writer: &mut impl Write,
    uri: Value,
    diagnostics: Value,
) -> io::Result<()> {
    write_publish_diagnostics_versioned(writer, uri, diagnostics, Value::Null)
}

/// Same as `write_publish_diagnostics` but includes the LSP `version` field
/// so the v0.17.3 version-match freshness path can be tested.
///
/// When the env var `AFT_FAKE_LSP_STALE_VERSION` is set, the fake server
/// publishes `version - 1` instead of the actual version, simulating an
/// out-of-order publish that should be rejected as stale by the post-edit
/// freshness check.
fn write_publish_diagnostics_versioned(
    writer: &mut impl Write,
    uri: Value,
    diagnostics: Value,
    version: Value,
) -> io::Result<()> {
    let effective_version = if std::env::var("AFT_FAKE_LSP_STALE_VERSION").is_ok() {
        match &version {
            Value::Number(n) if n.is_i64() => Value::Number((n.as_i64().unwrap() - 1).into()),
            _ => version.clone(),
        }
    } else {
        version
    };

    let mut params = json!({
        "uri": uri,
        "diagnostics": diagnostics,
    });
    if !effective_version.is_null() {
        params["version"] = effective_version;
    }
    write_notification(
        writer,
        &Notification::new("textDocument/publishDiagnostics", Some(params)),
    )
}

fn opened_diagnostics() -> Value {
    json!([
        {
            "range": {
                "start": { "line": 0, "character": 0 },
                "end": { "line": 0, "character": 5 }
            },
            "severity": 1,
            "code": "E0001",
            "source": "fake-lsp",
            "message": "test diagnostic error"
        },
        {
            "range": {
                "start": { "line": 1, "character": 4 },
                "end": { "line": 1, "character": 10 }
            },
            "severity": 2,
            "source": "fake-lsp",
            "message": "test diagnostic warning"
        }
    ])
}

fn changed_diagnostics() -> Value {
    json!([
        {
            "range": {
                "start": { "line": 2, "character": 1 },
                "end": { "line": 2, "character": 8 }
            },
            "severity": 1,
            "code": "E0002",
            "source": "fake-lsp",
            "message": "test diagnostic after change"
        }
    ])
}

fn main() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    let mut writer = stdout.lock();
    let mut should_register_watched_files = false;

    while let Some(message) = read_message(&mut reader)? {
        match message {
            ServerMessage::Request { id, method, params } => match method.as_str() {
                "initialize" => {
                    // Capability variants controlled by env vars so tests
                    // can exercise different code paths:
                    //   AFT_FAKE_LSP_PULL=1                → declare diagnosticProvider
                    //   AFT_FAKE_LSP_WORKSPACE=1           → also support workspace/diagnostic
                    //   AFT_FAKE_LSP_NO_WATCHED_FILES=1    → OMIT workspace.didChangeWatchedFiles
                    //                                        (exercises the F5 capability-gate skip path)
                    //   (no env)                           → push-only with watched-files support
                    let pull_enabled =
                        std::env::var("AFT_FAKE_LSP_PULL").ok().as_deref() == Some("1");
                    let workspace_pull =
                        std::env::var("AFT_FAKE_LSP_WORKSPACE").ok().as_deref() == Some("1");
                    let no_watched_files = std::env::var("AFT_FAKE_LSP_NO_WATCHED_FILES")
                        .ok()
                        .as_deref()
                        == Some("1");

                    let mut capabilities = json!({
                        "textDocumentSync": 1,
                        "hoverProvider": true,
                        "definitionProvider": true,
                        "referencesProvider": true,
                        "renameProvider": {
                            "prepareProvider": true
                        }
                    });

                    if !no_watched_files {
                        // Default: advertise didChangeWatchedFiles so the capability gate
                        // in notify_files_watched_changed (#32) allows notifications
                        // to reach the fake server during integration tests.
                        capabilities["workspace"] = json!({
                            "didChangeWatchedFiles": {
                                "dynamicRegistration": true
                            }
                        });
                        should_register_watched_files = true;
                    }

                    if pull_enabled {
                        capabilities["diagnosticProvider"] = json!({
                            "interFileDependencies": true,
                            "workspaceDiagnostics": workspace_pull,
                            "identifier": "fake-lsp"
                        });
                    }

                    write_response(
                        &mut writer,
                        id,
                        json!({
                            "capabilities": capabilities,
                            "serverInfo": {
                                "name": "fake-lsp-server",
                                "version": "0.1.0",
                            }
                        }),
                    )?;
                    write_notification(
                        &mut writer,
                        &Notification::new(
                            "custom/initialized",
                            Some(json!({
                                "initializationOptions": params
                                    .as_ref()
                                    .and_then(|value| value.get("initializationOptions"))
                                    .cloned()
                                    .unwrap_or(Value::Null),
                                "env": {
                                    "AFT_TEST_LSP_ENV": std::env::var("AFT_TEST_LSP_ENV").ok()
                                }
                            })),
                        ),
                    )?;
                }
                "shutdown" => {
                    write_response(&mut writer, id, Value::Null)?;
                }
                "textDocument/hover" => {
                    let (line, character) = request_position(&params);
                    if line == 0 && character == 0 {
                        write_response(
                            &mut writer,
                            id,
                            json!({
                                "contents": {
                                    "kind": "markdown",
                                    "value": "```typescript\nconst x: number\n```"
                                },
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 7 }
                                }
                            }),
                        )?;
                    } else {
                        write_response(&mut writer, id, Value::Null)?;
                    }
                }
                "textDocument/definition" => {
                    let uri = request_document_uri(&params);
                    write_response(
                        &mut writer,
                        id,
                        json!({
                            "uri": uri,
                            "range": {
                                "start": { "line": 0, "character": 0 },
                                "end": { "line": 0, "character": 10 }
                            }
                        }),
                    )?;
                }
                "textDocument/references" => {
                    let uri = request_document_uri(&params);
                    let include_declaration = request_include_declaration(&params);
                    let mut locations = vec![json!({
                        "uri": uri.clone(),
                        "range": {
                            "start": { "line": 2, "character": 0 },
                            "end": { "line": 2, "character": 5 }
                        }
                    })];
                    if include_declaration {
                        locations.insert(
                            0,
                            json!({
                                "uri": uri,
                                "range": {
                                    "start": { "line": 0, "character": 0 },
                                    "end": { "line": 0, "character": 5 }
                                }
                            }),
                        );
                    }
                    write_response(&mut writer, id, json!(locations))?;
                }
                "textDocument/prepareRename" => {
                    let (line, character) = request_position(&params);
                    if line == 0 && character == 4 {
                        write_response(
                            &mut writer,
                            id,
                            json!({
                                "range": {
                                    "start": { "line": 0, "character": 4 },
                                    "end": { "line": 0, "character": 9 }
                                },
                                "placeholder": "hello"
                            }),
                        )?;
                    } else {
                        write_response(&mut writer, id, Value::Null)?;
                    }
                }
                "textDocument/diagnostic" => {
                    // LSP 3.17 pull diagnostics. Honor previousResultId for
                    // unchanged-state replies. Fail-mode is controlled by
                    // env var so tests can drive each branch.
                    let force_unchanged =
                        std::env::var("AFT_FAKE_LSP_PULL_UNCHANGED").ok().as_deref() == Some("1");
                    let force_error =
                        std::env::var("AFT_FAKE_LSP_PULL_ERROR").ok().as_deref() == Some("1");

                    if force_error {
                        write_json_message(
                            &mut writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32603,
                                    "message": "fake-lsp: forced pull error"
                                }
                            }),
                        )?;
                    } else if force_unchanged {
                        write_response(
                            &mut writer,
                            id,
                            json!({
                                "kind": "unchanged",
                                "resultId": "fake-result-1"
                            }),
                        )?;
                    } else {
                        write_response(
                            &mut writer,
                            id,
                            json!({
                                "kind": "full",
                                "resultId": "fake-result-1",
                                "items": [
                                    {
                                        "range": {
                                            "start": { "line": 4, "character": 0 },
                                            "end": { "line": 4, "character": 8 }
                                        },
                                        "severity": 1,
                                        "code": "E0PULL",
                                        "source": "fake-lsp",
                                        "message": "test pull diagnostic"
                                    }
                                ]
                            }),
                        )?;
                    }
                }
                "workspace/diagnostic" => {
                    // LSP 3.17 workspace pull. Fake produces one report for
                    // a synthetic URI. Test variants:
                    //   AFT_FAKE_LSP_WS_TIMEOUT=1 → never reply (server hangs)
                    //   AFT_FAKE_LSP_WS_PARTIAL=1 → reply with empty items
                    let force_timeout =
                        std::env::var("AFT_FAKE_LSP_WS_TIMEOUT").ok().as_deref() == Some("1");
                    let force_partial =
                        std::env::var("AFT_FAKE_LSP_WS_PARTIAL").ok().as_deref() == Some("1");

                    if force_timeout {
                        // Never respond — emulates a server still analyzing.
                        // The client should hit its 10s timeout and cancel.
                        continue;
                    }

                    let items = if force_partial {
                        json!([])
                    } else {
                        json!([{
                            "kind": "full",
                            "uri": "file:///workspace-pull-target.ts",
                            "resultId": "fake-ws-1",
                            "items": [
                                {
                                    "range": {
                                        "start": { "line": 7, "character": 0 },
                                        "end": { "line": 7, "character": 5 }
                                    },
                                    "severity": 1,
                                    "code": "E0WSP",
                                    "source": "fake-lsp",
                                    "message": "workspace pull diagnostic"
                                }
                            ]
                        }])
                    };

                    write_response(&mut writer, id, json!({ "items": items }))?;
                }
                "textDocument/rename" => {
                    let uri_key = request_document_uri_string(&params);
                    let new_name = request_new_name(&params);
                    let edits = if new_name == "__force_failure__" {
                        vec![
                            json!({
                                "range": {
                                    "start": { "line": 0, "character": 4 },
                                    "end": { "line": 0, "character": 9 }
                                },
                                "newText": new_name
                            }),
                            json!({
                                "range": {
                                    "start": { "line": 99, "character": 0 },
                                    "end": { "line": 99, "character": 5 }
                                },
                                "newText": new_name
                            }),
                        ]
                    } else {
                        vec![
                            json!({
                                "range": {
                                    "start": { "line": 0, "character": 4 },
                                    "end": { "line": 0, "character": 9 }
                                },
                                "newText": new_name
                            }),
                            json!({
                                "range": {
                                    "start": { "line": 2, "character": 0 },
                                    "end": { "line": 2, "character": 5 }
                                },
                                "newText": new_name
                            }),
                        ]
                    };
                    let mut changes = serde_json::Map::new();
                    changes.insert(uri_key, Value::Array(edits));
                    write_response(
                        &mut writer,
                        id,
                        Value::Object(
                            [("changes".to_string(), Value::Object(changes))]
                                .into_iter()
                                .collect(),
                        ),
                    )?;
                }
                _ => {
                    write_json_message(
                        &mut writer,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": -32601,
                                "message": format!("method not found: {method}"),
                            }
                        }),
                    )?;
                }
            },
            ServerMessage::Notification { method, params } => match method.as_str() {
                "initialized" => {
                    if should_register_watched_files {
                        write_request(
                            &mut writer,
                            10_000,
                            "client/registerCapability",
                            json!({
                                "registrations": [
                                    {
                                        "id": "fake-lsp-watched-files",
                                        "method": "workspace/didChangeWatchedFiles",
                                        "registerOptions": {
                                            "watchers": [
                                                { "globPattern": "**/*" }
                                            ]
                                        }
                                    }
                                ]
                            }),
                        )?;
                        should_register_watched_files = false;
                    }
                }
                "workspace/didChangeWatchedFiles" => {
                    write_notification(
                        &mut writer,
                        &Notification::new("custom/watchedFilesChanged", params),
                    )?;
                }
                "textDocument/didOpen" => {
                    let uri = document_uri(&params);
                    let version = document_version(&params);

                    write_custom_notification(
                        &mut writer,
                        "custom/documentOpened",
                        uri.clone(),
                        version.clone(),
                    )?;
                    write_publish_diagnostics_versioned(
                        &mut writer,
                        uri,
                        opened_diagnostics(),
                        version,
                    )?;
                }
                "textDocument/didChange" => {
                    let uri = document_uri(&params);
                    let version = document_version(&params);
                    write_custom_notification(
                        &mut writer,
                        "custom/documentChanged",
                        uri.clone(),
                        version.clone(),
                    )?;
                    write_publish_diagnostics_versioned(
                        &mut writer,
                        uri,
                        changed_diagnostics(),
                        version,
                    )?;
                }
                "textDocument/didClose" => {
                    let uri = document_uri(&params);
                    write_custom_notification(
                        &mut writer,
                        "custom/documentClosed",
                        uri.clone(),
                        Value::Null,
                    )?;
                    write_publish_diagnostics(&mut writer, uri, json!([]))?;
                }
                "exit" => break,
                _ => {}
            },
            ServerMessage::Response(_) => {}
        }
    }

    Ok(())
}
