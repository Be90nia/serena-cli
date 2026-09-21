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

/// 帧体（`Content-Length`）上限：异常 LS 声明超大帧时直接拒绝，不做对应
/// 巨量 reserve（原实现可被诱导分配到 capacity overflow panic，pump task
/// 死 → 会话僵死）。64 MiB 覆盖全量符号/大文档响应，留约 10 倍余量。
const MAX_FRAME_BODY: usize = 64 * 1024 * 1024;

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
    #[error("Content-Length {0} exceeds the 64 MiB frame limit")]
    TooLarge(usize),
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

/// 增量帧解码器：记住帧头扫描进度，body 累积期零重扫。
///
/// 泵按 8 KiB chunk 喂 `BytesMut` 反复调 [`Decoder::decode`]；无状态解码每次
/// 从 0 重扫找 `\r\n\r\n`（O(n²)：5 MB 帧累计扫 GB 级字节，泵 task 被饿死）。
/// Decoder 把已扫偏移留在 `scanned`，帧头只定位一次；出帧后归零。
#[derive(Debug, Default)]
pub struct Decoder {
    /// 头部搜索进度：`buf[..scanned]` 已确认无 `\r\n\r\n`（留 3 字节窗口重叠）。
    scanned: usize,
    /// 已定位帧头 `(header_len, frame_len)`；`None` = 仍在找帧头。
    header: Option<(usize, usize)>,
}

impl Decoder {
    /// Try to decode one frame from `buf`; `Ok(None)` while fewer bytes than a
    /// complete frame are buffered. Consumes exactly the frame bytes on success.
    ///
    /// ↖ mirror: lsp_protocol_handler/server.py@43ae021 `content_length`
    pub fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<JsonRpc>, FrameError> {
        let (header_len, frame_len) = match self.header {
            Some(found) => found,
            None => {
                // 只从上次扫到的位置继续（3 字节重叠防分隔符跨 chunk 裂开）。
                let start = self.scanned.min(buf.len());
                let Some(rel) = buf[start..].windows(4).position(|w| w == b"\r\n\r\n") else {
                    self.scanned = buf.len().saturating_sub(3);
                    return Ok(None); // headers still streaming
                };
                let header_len = start + rel;
                let content_length = parse_content_length(&buf[..header_len])?;
                if content_length > MAX_FRAME_BODY {
                    return Err(FrameError::TooLarge(content_length));
                }
                let found = (header_len, header_len + 4 + content_length);
                self.header = Some(found);
                found
            }
        };
        if buf.len() < frame_len {
            buf.reserve(frame_len - buf.len()); // frame_len 已钳在 MAX_FRAME_BODY 内
            return Ok(None); // body still streaming
        }
        let frame = buf.split_to(frame_len);
        self.header = None;
        self.scanned = 0;
        Ok(Some(serde_json::from_slice(&frame[header_len + 4..])?))
    }
}

/// 解析帧头的 `Content-Length`（重复声明时最后一条生效；缺失/坏值报错）。
fn parse_content_length(headers: &[u8]) -> Result<usize, FrameError> {
    let headers = std::str::from_utf8(headers).map_err(|_| FrameError::BadHeader)?;
    let mut content_length: Option<usize> = None;
    for line in headers.split('\n') {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            content_length = Some(value.trim().parse()?);
        }
    }
    content_length.ok_or(FrameError::MissingContentLength)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn big_request(id: i64, pad_len: usize) -> (JsonRpc, Vec<u8>) {
        let msg = JsonRpc::request(id, "big", json!({"pad": "x".repeat(pad_len)}));
        (msg.clone(), encode(&msg))
    }

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
        let mut dec = Decoder::default();
        assert!(dec.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b);
        assert!(dec.decode(&mut buf).unwrap().is_some());
    }

    #[test]
    fn decode_roundtrip() {
        let frame = encode(&JsonRpc::request(
            7,
            "textDocument/references",
            json!({"uri":"u"}),
        ));
        let mut buf = BytesMut::from(&frame[..]);
        let mut dec = Decoder::default();
        let msg = dec
            .decode(&mut buf)
            .unwrap()
            .expect("complete frame must decode");
        assert_eq!(msg.id, Some(json!(7)));
        assert_eq!(msg.method.as_deref(), Some("textDocument/references"));
        assert_eq!(msg.params, Some(json!({"uri":"u"})));
        assert!(buf.is_empty());
    }

    #[test]
    fn oversized_content_length_rejected() {
        // 异常 LS 声明 ~100GB 帧：报 TooLarge，不巨量 reserve、不 panic
        let mut buf = BytesMut::new();
        buf.extend_from_slice(b"Content-Length: 99999999999\r\n\r\n");
        let mut dec = Decoder::default();
        match dec.decode(&mut buf) {
            Err(FrameError::TooLarge(n)) => assert_eq!(n, 99_999_999_999),
            other => panic!("expected TooLarge, got {other:?}"),
        }
        assert_eq!(buf.len(), 31); // 帧头未被消费
    }

    #[test]
    fn chunked_5mb_frame_boundary_in_header() {
        let (msg, frame) = big_request(1, 5 * 1024 * 1024);
        let (a, b) = frame.split_at(10); // "Content-Le" | "ngth: …"
        let mut buf = BytesMut::from(a);
        let mut dec = Decoder::default();
        assert!(dec.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(b);
        assert_eq!(dec.decode(&mut buf).unwrap().as_ref(), Some(&msg));
        assert!(buf.is_empty());
    }

    #[test]
    fn chunked_5mb_frame_boundary_mid_body() {
        let (msg, frame) = big_request(2, 5 * 1024 * 1024);
        let header_end = frame.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let mut buf = BytesMut::new();
        let mut dec = Decoder::default();
        // 第一片切在 \r\n\r\n 中间（分隔符跨 chunk 裂开，扫描重叠必须兜住）
        buf.extend_from_slice(&frame[..header_end - 1]);
        assert!(dec.decode(&mut buf).unwrap().is_none());
        let mid = frame.len() / 2;
        buf.extend_from_slice(&frame[header_end - 1..mid]);
        assert!(dec.decode(&mut buf).unwrap().is_none());
        buf.extend_from_slice(&frame[mid..]);
        assert_eq!(dec.decode(&mut buf).unwrap().as_ref(), Some(&msg));
        assert!(buf.is_empty());
    }

    #[test]
    fn chunked_frame_boundary_after_frame_end() {
        let (msg, frame) = big_request(3, 5 * 1024 * 1024);
        let mut buf = BytesMut::new();
        let mut dec = Decoder::default();
        // 粘包：完整帧 + 下一帧的前 5 个字节一次到达
        buf.extend_from_slice(&frame);
        buf.extend_from_slice(b"Conte");
        assert_eq!(dec.decode(&mut buf).unwrap().as_ref(), Some(&msg));
        assert_eq!(buf.as_ref(), b"Conte"); // 残余留在 buf，扫描状态已归零
        assert!(dec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn scan_state_resets_after_large_frame() {
        let (big, big_frame) = big_request(4, 5 * 1024 * 1024);
        let small = encode(&JsonRpc::request(5, "ping", json!({})));
        let small_msg = JsonRpc::request(5, "ping", json!({}));
        let mut buf = BytesMut::new();
        let mut dec = Decoder::default();
        buf.extend_from_slice(&big_frame);
        buf.extend_from_slice(&small);
        assert_eq!(dec.decode(&mut buf).unwrap().as_ref(), Some(&big));
        assert_eq!(dec.decode(&mut buf).unwrap().as_ref(), Some(&small_msg));
        assert!(buf.is_empty());
    }
}
