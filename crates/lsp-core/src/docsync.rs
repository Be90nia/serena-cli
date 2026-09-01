//! 文档同步：`textDocument/didOpen` 全量上送 + `didChange` 全量重同步 +
//! ref-count `didClose`（PLAN Task 7 / ARCHITECTURE §3.2 BUF + §3.4 锁）。
//!
//! ↖ mirror: ls.py@43ae021 `LSPFileBuffer._open_in_ls` + `open_file_buffers`：
//! - 文件 → 首次 ensure_open：读盘 + 记录 mtime/version + 全量 didOpen。
//! - 后续 ensure_open：stat 拿 mtime，与记账不符 → 全量 didChange（version++）。
//! - 同一 URI 多次 ensure_open → ref_count++，drop 归零 → didClose + 从表移除。
//!
//! 设计要点（ARCHITECTURE §3.4 锁纪律 + Task 7 设计）：
//! - `Session::buffers: Mutex<HashMap<Uri, FileBuffer>>`（std 锁；临界区微秒、无 await）。
//! - **盘上 IO 全部在锁外做**（§3.4 注：didOpen/didChange 的实际发送在锁外）。
//! - `FileGuard` 持 `Arc<Session>`（cheap clone）。`Mutex` map 锁内 dec 计数；归零时
//!   同步 `try_send` 发 didClose + 移除表项 —— drop 路径纯同步，不跨 await。
//! - mtime 用 `std::time::SystemTime` 直接比较；stat 失败记 None，后续按「变了」处理。
//!
//! **drop 不跨 await**：guard drop 时 outbound 可能已关，`try_send` 失败被忽略。

use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::SystemTime;

use lsp_types::Uri;
use serde_json::{Value, json};

use crate::error::{CoreError, Result};
use crate::session::Session;

/// LSP `didOpen` 起始版本号（LSP spec §3.1.1：每次变更递增，初始为 1）。
const INITIAL_VERSION: i64 = 1;

/// 单文件状态：uri + 上次记账 mtime + 当前 LSP 版本 + 引用计数。
#[derive(Debug)]
pub struct FileBuffer {
    pub uri: Uri,
    pub mtime: Option<SystemTime>,
    pub content_version: i64,
    pub ref_count: usize,
}

/// `ensure_open` 返回的 RAII guard。drop 时 ref_count--，归零 → didClose + 移除。
pub struct FileGuard {
    session: Arc<Session>,
    uri: Uri,
}

impl FileGuard {
    /// guard 当前关联的文件 URI（调试/诊断用）。
    pub fn uri(&self) -> &Uri {
        &self.uri
    }
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        let did_close = {
            let mut map = self
                .session
                .buffers
                .lock()
                .expect("docsync buffers mutex poisoned");
            match map.get_mut(&self.uri) {
                Some(buf) => {
                    buf.ref_count = buf.ref_count.saturating_sub(1);
                    if buf.ref_count == 0 {
                        map.remove(&self.uri);
                        Some(make_did_close(&self.uri))
                    } else {
                        None
                    }
                }
                None => None,
            }
        };
        if let Some(notif) = did_close {
            let _ = self.session.client().notify("textDocument/didClose", notif);
        }
    }
}

impl Session {
    /// 打开一个文件供 LSP 操作：首次 → 全量 didOpen；mtime 变了 → 全量 didChange。
    pub async fn ensure_open(self: &Arc<Self>, path: &Path) -> Result<FileGuard> {
        let uri = path_to_uri(path)?;
        let meta = fs::metadata(path).map_err(CoreError::Io)?;
        let mtime = meta.modified().ok();

        enum Action {
            Send { method: &'static str, params: Value },
            None,
        }

        let (to_send, _version_after) = {
            let mut map = self.buffers.lock().expect("docsync buffers mutex poisoned");
            match map.get_mut(&uri) {
                None => {
                    let text = fs::read_to_string(path).map_err(CoreError::Io)?;
                    let version = INITIAL_VERSION;
                    map.insert(
                        uri.clone(),
                        FileBuffer {
                            uri: uri.clone(),
                            mtime,
                            content_version: version,
                            ref_count: 1,
                        },
                    );
                    (
                        Action::Send {
                            method: "textDocument/didOpen",
                            params: make_did_open(&uri, &text, version),
                        },
                        version,
                    )
                }
                Some(buf) => {
                    buf.ref_count += 1;
                    if matches!(mtime, Some(new) if Some(new) == buf.mtime) {
                        (Action::None, buf.content_version)
                    } else {
                        let text = fs::read_to_string(path).map_err(CoreError::Io)?;
                        let version = buf.content_version + 1;
                        buf.mtime = mtime;
                        buf.content_version = version;
                        (
                            Action::Send {
                                method: "textDocument/didChange",
                                params: make_did_change(&uri, version, &text),
                            },
                            version,
                        )
                    }
                }
            }
        };

        match to_send {
            Action::Send { method, params } => {
                self.notify(method, params).await?;
            }
            Action::None => {}
        }

        Ok(FileGuard {
            session: Arc::clone(self),
            uri,
        })
    }
}

/// 把绝对路径转成 `file://` URL 字符串。`dunce` 去 UNC 前缀，`\` → `/`。
pub fn path_to_uri_str(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{}", s)
    } else {
        format!("file:///{}", s)
    }
}

fn path_to_uri(path: &Path) -> Result<Uri> {
    let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let raw = path_to_uri_str(&canonical);
    Uri::from_str(&raw).map_err(|e| {
        CoreError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path 不能解析为 file URI ({e}): {}", canonical.display()),
        ))
    })
}

fn make_did_open(uri: &Uri, text: &str, version: i64) -> Value {
    json!({
        "textDocument": {
            "uri": uri.as_str(),
            "languageId": "cpp",
            "version": version,
            "text": text,
        }
    })
}

fn make_did_change(uri: &Uri, version: i64, text: &str) -> Value {
    json!({
        "textDocument": {
            "uri": uri.as_str(),
            "version": version,
        },
        "contentChanges": [{ "text": text }],
    })
}

fn make_did_close(uri: &Uri) -> Value {
    json!({
        "textDocument": {
            "uri": uri.as_str(),
        }
    })
}
