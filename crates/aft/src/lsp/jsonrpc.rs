use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC request ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Int(i64),
    String(String),
}

/// Outgoing JSON-RPC request.
#[derive(Debug, Serialize)]
pub struct Request {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    pub fn new(id: RequestId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }
}

/// Outgoing JSON-RPC notification (no id).
#[derive(Debug, Serialize)]
pub struct Notification {
    pub jsonrpc: &'static str,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            method: method.into(),
            params,
        }
    }
}

/// Incoming JSON-RPC response.
#[derive(Debug, Deserialize)]
pub struct Response {
    pub id: RequestId,
    pub result: Option<Value>,
    pub error: Option<ResponseError>,
}

#[derive(Debug, Deserialize)]
pub struct ResponseError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

/// Outgoing JSON-RPC response (for responding to server requests).
#[derive(Debug, Serialize)]
pub struct OutgoingResponse {
    pub jsonrpc: &'static str,
    pub id: RequestId,
    pub result: Value,
}

impl OutgoingResponse {
    /// Create a success response with the given result.
    pub fn success(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result,
        }
    }
}

/// Any incoming message from the server.
#[derive(Debug)]
pub enum ServerMessage {
    Response(Response),
    Notification {
        method: String,
        params: Option<Value>,
    },
    Request {
        id: RequestId,
        method: String,
        params: Option<Value>,
    },
}

impl ServerMessage {
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        let value: Value = serde_json::from_str(json)?;

        if value.get("id").is_some() && value.get("method").is_some() {
            Ok(ServerMessage::Request {
                id: serde_json::from_value(value.get("id").cloned().unwrap_or(Value::Null))?,
                method: value
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                params: value.get("params").cloned(),
            })
        } else if value.get("id").is_some() {
            Ok(ServerMessage::Response(serde_json::from_value(value)?))
        } else {
            Ok(ServerMessage::Notification {
                method: value
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                params: value.get("params").cloned(),
            })
        }
    }
}
