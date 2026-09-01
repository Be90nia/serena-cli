//! JSON-RPC 2.0 messages and `Content-Length` frame codec.
//!
//! ↖ mirror: lsp_protocol_handler/server.py@43ae021 `create_message` / `content_length`

use bytes::BytesMut;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Methods that must not carry a `params` field at all — some servers reject
/// `params: null` on the shutdown sequence.
///
/// ↖ mirror: lsp_protocol_handler/server.py@43ae021 `_NO_PARAMS_METHODS`
const NO_PARAMS_METHODS: [&str; 2] = ["shutdown", "exit"];

/// Minimal JSON-RPC 2.0 message: request / notification / response — the shape
/// the Task 5 client extends on. `id` is a `Value` so upstream string ids
/// (`response_id.isdigit()` fallback) round-trip untouched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpc {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl JsonRpc {
    pub fn request(id: i64, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id.into()),
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn notification(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: None,
            method: Some(method.into()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn response_ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: None,
            params: None,
            result: Some(result),
            error: None,
        }
    }

    pub fn response_err(id: Value, error: RpcError) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id: Some(id),
            method: None,
            params: None,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame has no Content-Length header")]
    MissingContentLength,
    #[error("frame header is not UTF-8")]
    BadHeader,
    #[error("invalid Content-Length value: {0}")]
    BadContentLength(#[from] std::num::ParseIntError),
    #[error("frame body is not valid JSON-RPC: {0}")]
    BadJson(#[from] serde_json::Error),
}

/// Encode as one `Content-Length`-headed frame.
///
/// ↖ mirror: lsp_protocol_handler/server.py@43ae021 `create_message`
pub fn encode(msg: &JsonRpc) -> Vec<u8> {
    let mut wire = msg.clone();
    if wire
        .method
        .as_deref()
        .is_some_and(|m| NO_PARAMS_METHODS.contains(&m))
    {
        wire.params = None;
    }
    let body = serde_json::to_string(&wire).expect("infallible: JsonRpc holds only JSON data");
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Try to decode one frame from `buf`; `Ok(None)` while fewer bytes than a
/// complete frame are buffered. Consumes exactly the frame bytes on success.
///
/// ↖ mirror: lsp_protocol_handler/server.py@43ae021 `content_length`
pub fn decode(buf: &mut BytesMut) -> Result<Option<JsonRpc>, FrameError> {
    let Some(header_len) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Ok(None); // headers still streaming
    };
    let headers = std::str::from_utf8(&buf[..header_len]).map_err(|_| FrameError::BadHeader)?;
    let mut content_length: Option<usize> = None;
    for line in headers.split('\n') {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = Some(value.trim().parse()?);
        }
    }
    let Some(content_length) = content_length else {
        return Err(FrameError::MissingContentLength);
    };
    let body_start = header_len + 4;
    let frame_len = body_start + content_length;
    if buf.len() < frame_len {
        buf.reserve(frame_len - buf.len());
        return Ok(None); // body still streaming
    }
    let frame = buf.split_to(frame_len);
    Ok(Some(serde_json::from_slice(&frame[body_start..])?))
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn encodes_content_length_frame() {
        let out = encode(&JsonRpc::request(
            1,
            "textDocument/definition",
            json!({"uri":"u"}),
        ));
        let s = String::from_utf8(out).unwrap();
        assert!(s.starts_with("Content-Length: "));
        assert!(s.contains("\r\n\r\n{"));
    }

    #[test]
    fn no_params_methods_omit_params() {
        // ↖ mirror: lsp_protocol_handler/server.py@43ae021 _NO_PARAMS_METHODS（shutdown/exit）
        let out = encode(&JsonRpc::request(2, "shutdown", json!(null)));
        assert!(!String::from_utf8(out).unwrap().contains("params"));
    }

    #[test]
    fn split_across_reads() {
        // 半帧不解码，拼齐才出
        let full = encode(&JsonRpc::request(1, "ping", json!({})));
        let (a, b) = full.split_at(full.len() / 2);
        let mut buf = BytesMut::from(a);
        assert!(decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b);
        assert!(decode(&mut buf).unwrap().is_some());
    }

    #[test]
    fn decode_roundtrip() {
        let frame = encode(&JsonRpc::request(
            7,
            "textDocument/references",
            json!({"uri":"u"}),
        ));
        let mut buf = BytesMut::from(&frame[..]);
        let msg = decode(&mut buf)
            .unwrap()
            .expect("complete frame must decode");
        assert_eq!(msg.id, Some(json!(7)));
        assert_eq!(msg.method.as_deref(), Some("textDocument/references"));
        assert_eq!(msg.params, Some(json!({"uri":"u"})));
        assert!(buf.is_empty());
    }
}
