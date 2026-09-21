//! 文档同步：`textDocument/didOpen` 全量上送 + `didChange` 全量重同步 +
//! ref-count `didClose`（PLAN Task 7 / ARCHITECTURE §3.2 BUF + §3.4 锁）。
//!
//! ↖ mirror: ls.py@43ae021 `LSPFileBuffer._open_in_ls` + `open_file_buffers`：
//! - 文件 → 首次 ensure_open：读盘 + 记录 mtime/size/version + 全量 didOpen。
//! - 后续 ensure_open：stat 拿 mtime+size，与记账不符 → 全量 didChange（version++）。
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
//!
//! ## TTL 窗口（修 P1 #2）
//!
//! `FileGuard` 归零不再立即 didClose+移表。改为记 `last_released_at` 戳；保留
//! entry 留在表里供下次 `ensure_open` 复用——窗口内同文件二次访问走 Some(buf)
//! 分支（mtime/size 未变 → 无 didOpen），LS 端文档状态连续，缓存不重放。
//! 窗口到期或显式 `evict_all_buffers()` 才同步 didClose + 移除。
//! 锁纪律不变：drop 路径纯同步（仅戳时间），显式 evict 锁内取 list、锁外发通知。
//!
//! `FileBuffer` 不存文本——只存 mtime/size/version/ref_count/last_released_at，
//! 缓冲复用靠 LS 自身文档表（设计拍板的，勿改）。

use std::fs;
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use lsp_types::Uri;
use serde_json::{Value, json};

use crate::error::{CoreError, Result};
use crate::session::Session;

/// LSP `didOpen` 起始版本号（LSP spec §3.1.1：每次变更递增，初始为 1）。
const INITIAL_VERSION: i64 = 1;

/// FileGuard TTL 窗口：归零到显式 evict 之间的"复用宽限"。串行工具调用场景下
/// 几乎都覆盖；后台并发/批处理场景下 `evict_all_buffers()` 提供强制回收出口。
///
/// 60s：捕获 daemon 上一工具调用到下一工具调用之间正常间隙；超过此值可认定
/// 当前文件真正"无人用了"，release LS 上的文档状态（rust-analyzer 等内存
/// 偏紧的 LS 在大量 didOpen 不 didClose 时会 OOM）。
///
/// 测试与 supervisor 调用方按需传同值；这里只宣告默认 ttl。
#[allow(dead_code)]
const FILE_GUARD_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// 单文件状态：uri + 上次记账 mtime/size + 当前 LSP 版本 + 引用计数。
#[derive(Debug)]
pub struct FileBuffer {
    pub uri: Uri,
    pub mtime: Option<SystemTime>,
    /// 外部修改感知的第二因子：同 mtime 粒度窗口内的改写靠 size 检出
    /// （NTFS 等文件系统 mtime 精度有限，mtime+size 双对账堵住漏检窗口）。
    pub size: Option<u64>,
    pub content_version: i64,
    pub ref_count: usize,
    /// ref_count 归零时刻；修 P1 #2 引入，归零后不立即 didClose+移表，
    /// 等 evict 主动回收或 `last_released_at + FILE_GUARD_TTL` 超时。
    /// `None` = 当前有活 guard 持住，或归零时间未到。
    pub last_released_at: Option<Instant>,
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
        // 修 P1 #2：归零不立即 didClose+移表，只记 last_released_at。
        // 窗口内下次 ensure_open 命中 Some(buf) 分支 → ref_count++ + 戳清 None，
        // 无 LS 重放。窗口到期或显式 evict 才走 didClose + 移除路径。
        // 锁纪律不变：纯同步、临界区微秒、不跨 await。
        let _ = self
            .session
            .buffers
            .lock()
            .expect("docsync buffers mutex poisoned")
            .get_mut(&self.uri)
            .map(|buf| {
                buf.ref_count = buf.ref_count.saturating_sub(1);
                if buf.ref_count == 0 {
                    buf.last_released_at = Some(Instant::now());
                }
            });
    }
}

impl Session {
    /// 打开一个文件供 LSP 操作：首次 → 全量 didOpen；mtime/size 变了 → 全量 didChange。
    pub async fn ensure_open(self: &Arc<Self>, path: &Path) -> Result<FileGuard> {
        let uri = path_to_uri(path)?;
        let meta = fs::metadata(path).map_err(CoreError::Io)?;
        let mtime = meta.modified().ok();
        let size = Some(meta.len());

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
                            size,
                            content_version: version,
                            ref_count: 1,
                            last_released_at: None,
                        },
                    );
                    (
                        Action::Send {
                            method: "textDocument/didOpen",
                            params: make_did_open(&uri, &text, version, &self.language_id()),
                        },
                        version,
                    )
                }
                Some(buf) => {
                    buf.ref_count += 1;
                    // 复用命中（同 file 在 TTL 窗口内再访问）：戳清释放时间，
                    // 让 next drop 的 last_released_at 重新起算。
                    buf.last_released_at = None;
                    // mtime+size 双因子：同 mtime 粒度窗口内的外部改写靠 size 检出。
                    // 记账侧任一为 None（stat 异常）→ 保守按「变了」处理。
                    let unchanged = mtime.is_some()
                        && size.is_some()
                        && mtime == buf.mtime
                        && size == buf.size;
                    if unchanged {
                        (Action::None, buf.content_version)
                    } else {
                        let text = fs::read_to_string(path).map_err(CoreError::Io)?;
                        let version = buf.content_version + 1;
                        buf.mtime = mtime;
                        buf.size = size;
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

    /// 强制回收所有缓冲：发 didClose + 移表。仅当调用方确认要立即关闭所有文档
    /// 时使用（daemon shutdown / 测试清理 / LS 内存压力回收）。
    /// 锁内 drain 取待发列表，锁外发通知（outbound 关 send 会失败，吞错兜底）。
    pub fn evict_all_buffers(&self) {
        let to_close: Vec<Uri> = {
            let mut map = self
                .buffers
                .lock()
                .expect("docsync buffers mutex poisoned");
            map.drain()
                .map(|(uri, _)| uri)
                .collect()
        };
        for uri in &to_close {
            let _ = self
                .client()
                .notify("textDocument/didClose", make_did_close(uri));
        }
    }

/// 回收超过 TTL 的空闲缓冲：ref_count=0 且 last_released_at + ttl < now。
    /// 复用路径（ref_count>0）一律不动。`FILE_GUARD_TTL` 为默认 ttl。
    /// 锁内 drain 取待发列表，锁外发通知（outbound 关 send 失败忽略）。
    ///
    /// 锁外 notify 期间可能并发 ensure_open 导致条目标记 ref_count>0 的"复活"；
    /// 该状态下再 didClose 会让 LS 收到"关闭已开文档" —— LS 通常忽略。
    /// 若需要严格语义，可换『两轮锁 + 比较戳』模式，本场景不引入复杂度。
    pub fn evict_idle_buffers(&self, ttl: std::time::Duration) -> usize {
        let now_idle: Vec<Uri> = {
            let mut map = self
                .buffers
                .lock()
                .expect("docsync buffers mutex poisoned");
            let now_idle: Vec<Uri> = map
                .iter()
                .filter(|(_, buf)| {
                    buf.ref_count == 0
                        && buf.last_released_at.is_some_and(|t| t.elapsed() >= ttl)
                })
                .map(|(uri, _)| uri.clone())
                .collect();
            for uri in &now_idle {
                map.remove(uri);
            }
            now_idle
        };
        for uri in &now_idle {
            let _ = self
                .client()
                .notify("textDocument/didClose", make_did_close(uri));
        }
        now_idle.len()
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

pub fn path_to_uri(path: &Path) -> Result<Uri> {
    let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let raw = path_to_uri_str(&canonical);
    Uri::from_str(&raw).map_err(|e| {
        CoreError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("path 不能解析为 file URI ({e}): {}", canonical.display()),
        ))
    })
}

fn make_did_open(uri: &Uri, text: &str, version: i64, language_id: &str) -> Value {
    json!({
        "textDocument": {
            "uri": uri.as_str(),
            "languageId": language_id,
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
