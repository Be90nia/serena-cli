//! JSON-RPC 录制/回放（PLAN Task 26，M3 T2 验收依赖）。
//!
//! 用途：录制真实 LS（clangd / rust-analyzer / jdtls）一次会话的出站/入站帧，
//! 回放时用同一份 JSONL 替代真 LS —— 让 M3 适配器对照测试无需真服务器。
//!
//! JSONL 格式（每行一条方向标注 + JSON-RPC 帧）：
//!
//! ```text
//!   --> {"jsonrpc":"2.0","id":1,"method":"initialize",...}
//!   <-- {"jsonrpc":"2.0","id":1,"result":{...}}
//!   --> {"jsonrpc":"2.0","method":"initialized",...}
//!   <-- {"jsonrpc":"2.0","method":"textDocument/publishDiagnostics",...}
//! ```
//!
//! "方向"约定：`-->` client→LS 出站，` <--` LS→client 入站。回放按原序逐行还原：
//! 写时记 `-->`，回放时把出站帧**吞掉不发真 LS**；读时记 `<--`，回放时把入站帧
//! 喂回 stdout pump（按 on_msg 路径内联处理）。
//!
//! ponytail: 全局一个文件 + 单 Mutex 保护写者；不做流式压缩、不做时间戳——
//! M3 验收只需要"同输入同输出"语义，不需 diff 工具。
//!
//! ## 公开入口
//!
//! - `Recorder::open(path)` → 写出站/入站帧到文件。
//! - `Recorder::open_replay(path)` → 读录文件，按"出站吞掉 + 入站喂回"语义工作。
//! - `Recorder::close()` → flush + drop。
//!
//! ## 线程安全
//!
//! `Recorder` 用 `Arc<Mutex<...>>` 共享，多个 task（writer / stdout pump）可并发调用。
//! 写操作持锁短（写入几 KB），读回放持锁也是短（读一行）。

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::framing::JsonRpc;

/// 出站方向：client → LS。
const DIR_OUT: &[u8] = b"--> ";
/// 入站方向：LS → client。
const DIR_IN: &[u8] = b"<-- ";

/// 录制器：同时支持 record（写）和 replay（读）。
#[derive(Clone)]
pub struct Recorder {
    inner: Arc<Mutex<RecorderInner>>,
}

enum RecorderInner {
    /// 录制模式：所有帧追加写到文件。
    Record {
        file: File,
    },
    /// 回放模式：从文件读，逐行还原。
    Replay {
        /// 预读的所有入站帧（按出现顺序）。
        inbound: std::collections::VecDeque<JsonRpc>,
    },
    /// 透传模式：无副作用。
    PassThrough,
}

impl Recorder {
    /// 打开录制文件，新帧追加写入。文件不存在则创建。
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(RecorderInner::Record { file })),
        })
    }

    /// 打开回放文件。所有出站帧被吞掉；入站帧按录文件顺序喂回。
    pub fn open_replay(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut inbound = std::collections::VecDeque::new();
        for line in reader.lines() {
            let line = line?;
            if line.is_empty() {
                continue;
            }
            // 至少 4 字节前缀 + 至少 1 字节 JSON。
            let Some(rest) = line.strip_prefix("--> ").or_else(|| line.strip_prefix("<-- "))
            else {
                return Err(std::io::Error::other(format!(
                    "record file line missing direction prefix: {line:?}"
                )));
            };
            let frame: JsonRpc = serde_json::from_str(rest).map_err(|e| {
                std::io::Error::other(format!("record file JSON decode: {e}"))
            })?;
            // 出站帧在回放模式下"吞掉" —— 计数由调用方不需要，直接丢。
            if line.starts_with("<-- ") {
                inbound.push_back(frame);
            }
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(RecorderInner::Replay { inbound })),
        })
    }

    /// 透传：什么都不做。**默认行为**（未设 env 时）。
    pub fn passthrough() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RecorderInner::PassThrough)),
        }
    }

    /// 当前是否透传。
    pub fn is_passthrough(&self) -> bool {
        matches!(*self.inner.lock().unwrap(), RecorderInner::PassThrough)
    }

    /// 当前是否回放模式。
    pub fn is_replay(&self) -> bool {
        matches!(*self.inner.lock().unwrap(), RecorderInner::Replay { .. })
    }

    /// 记录一帧出站（client → LS）。replay 模式下吞掉。
    pub fn record_outbound(&self, msg: &JsonRpc) {
        let mut guard = self.inner.lock().unwrap();
        if let RecorderInner::Record { file } = &mut *guard {
            if let Ok(s) = serde_json::to_string(msg) {
                let _ = file.write_all(DIR_OUT);
                let _ = file.write_all(s.as_bytes());
                let _ = file.write_all(b"\n");
                let _ = file.flush();
            }
        }
    }

    /// 记录一帧入站（LS → client）。replay 模式下不需要调用此方法
    /// （入站帧由 next_inbound 直接从预读队列返回）。
    pub fn record_inbound(&self, msg: &JsonRpc) {
        let mut guard = self.inner.lock().unwrap();
        if let RecorderInner::Record { file } = &mut *guard {
            if let Ok(s) = serde_json::to_string(msg) {
                let _ = file.write_all(DIR_IN);
                let _ = file.write_all(s.as_bytes());
                let _ = file.write_all(b"\n");
                let _ = file.flush();
            }
        }
    }

    /// replay 模式：弹出下一条入站帧给 stdout pump 喂。
    /// 非 replay 模式返回 `None`。
    pub fn next_inbound(&self) -> Option<JsonRpc> {
        let mut guard = self.inner.lock().unwrap();
        if let RecorderInner::Replay { inbound } = &mut *guard {
            inbound.pop_front()
        } else {
            None
        }
    }

    /// replay 模式：入站队列是否已空。Session 主动 EOF 触发 drain。
    pub fn inbound_remaining(&self) -> usize {
        let guard = self.inner.lock().unwrap();
        if let RecorderInner::Replay { inbound } = &*guard {
            inbound.len()
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn msg(method: &str) -> JsonRpc {
        JsonRpc {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: Some(method.into()),
            params: None,
            result: None,
            error: None,
        }
    }

    #[test]
    fn roundtrip_record_then_replay() {
        let tmp = std::env::temp_dir().join(format!("serena-record-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&tmp);

        let rec = Recorder::open(&tmp).unwrap();
        rec.record_outbound(&msg("initialize"));
        rec.record_inbound(&msg("reply-1"));
        rec.record_outbound(&msg("initialized"));
        rec.record_inbound(&msg("notify-1"));
        drop(rec);

        let replay = Recorder::open_replay(&tmp).unwrap();
        assert!(replay.is_replay());
        // 顺序：入站是 reply-1, notify-1。
        let first = replay.next_inbound().unwrap();
        assert_eq!(first.method.as_deref(), Some("reply-1"));
        let second = replay.next_inbound().unwrap();
        assert_eq!(second.method.as_deref(), Some("notify-1"));
        assert_eq!(replay.inbound_remaining(), 0);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn passthrough_is_default() {
        let rec = Recorder::passthrough();
        assert!(rec.is_passthrough());
        assert!(!rec.is_replay());
        rec.record_outbound(&msg("x")); // 静默不报错
        assert!(rec.next_inbound().is_none());
    }
}