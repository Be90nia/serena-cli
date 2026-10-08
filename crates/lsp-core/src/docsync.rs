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
use percent_encoding::{CONTROLS, utf8_percent_encode};
use serde_json::{Value, json};
use tokio::time::sleep;

use crate::error::{CoreError, Result};
use crate::session::Session;

/// LSP `didOpen` 起始版本号（LSP spec §3.1.1：每次变更递增，初始为 1）。
const INITIAL_VERSION: i64 = 1;

/// 路径 → URI 时需百分号编码的 ASCII 集合：控制字符 + 空格 + `#` + `?` + `%`。
/// `/` `:` 字母数字和 `-_~.` 是 RFC 3986 path 合法字符，保持原样；非 ASCII 字节
/// （中文/emoji 等 UTF-8 序列）percent-encoding 无条件编码（bd serena-rust-cbd：
/// rust `Uri` 拒绝非 ASCII，原样拼接会导致 ensure_open 链路误报 INTERNAL）。
const PATH_UNSAFE: &percent_encoding::AsciiSet = &CONTROLS.add(b' ').add(b'#').add(b'?').add(b'%');

/// FileGuard TTL 窗口：归零到显式 evict 之间的"复用宽限"。串行工具调用场景下
/// 几乎都覆盖；后台并发/批处理场景下 `evict_all_buffers()` 提供强制回收出口。
///
/// 60s：捕获 daemon 上一工具调用到下一工具调用之间正常间隙；超过此值可认定
/// 当前文件真正"无人用了"，release LS 上的文档状态（rust-analyzer 等内存
/// 偏紧的 LS 在大量 didOpen 不 didClose 时会 OOM）。
///
/// 测试与 supervisor 调用方按需传同值；这里只宣告默认 ttl。
#[allow(dead_code)]
pub const FILE_GUARD_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// docsync 缓冲池容量上限（修 P1 #1 LRU）：当缓冲池条目数 ≥ 此值时，新插入触发
/// LRU 回收 —— 把 idle 条目按 `last_released_at` 升序淘汰，直到腾出空位。
///
/// 32：与 `crates/supervisor/src/lib.rs` 的 `RECLAIM_THRESHOLD` 对齐——前 32 次工具
/// 调用累积的活跃文件数；超过此值假定"冷文件"可丢。真实负载（rust-analyzer
/// 等内存偏紧的 LS）下此值应远低于单 LS 自身文档表上限。
///
/// 修 P1 #1：原实现无容量闸门，长驻 daemon 累计几千文件会让 RA 报 OOM。容量限制
/// + LRU 兜底 —— 复用窗口（60s TTL）内同文件访问不丢；窗口外冷文件被强制回收。
pub const FILE_BUFFER_CAPACITY: usize = 32;

/// didChange 去抖窗口（修 P0-A 50 文件挂死）：批工具（如 `tool_symbol_tree`）扇出
/// N 路 `ensure_open` 时，每路先 sleep `debounce_ms` 让前一波 `didOpen` 流到 LS，
/// 然后本批整体走 `didChange` —— 缓解 LS 内部队列拥塞导致 channel 关闭。
///
/// 200ms：覆盖 rust-analyzer 单文件 index 单次往返 P99；同 key 多次 didChange
/// 在 LS 进程内合并为一次全量（LS 自身行为）。`send_immediate=true` 跳过 debounce，
/// 给单文件路径用 —— 行为与原 `ensure_open` 完全等价。
#[derive(Debug, Clone, Copy)]
pub struct EnsureOpenParams {
    pub debounce_ms: u64,
    pub send_immediate: bool,
}

impl Default for EnsureOpenParams {
    fn default() -> Self {
        Self {
            debounce_ms: 200,
            send_immediate: false,
        }
    }
}

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

        let (to_send, _version_after, lru_evicted) = {
            let mut map = self.buffers.lock().expect("docsync buffers mutex poisoned");
            let mut lru_evicted: Vec<Uri> = Vec::new();
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
                    // 修 P1 #1：插入新条目后立即跑 LRU 容量闸门。
                    // 此时池大小 = capacity + 1（newly inserted），evict 把池收回到 capacity。
                    lru_evicted = evict_lru_idle_locked(&mut map, FILE_BUFFER_CAPACITY);
                    let action = Action::Send {
                        method: "textDocument/didOpen",
                        params: make_did_open(&uri, &text, version, &self.language_id_for(path)),
                    };
                    (action, version, lru_evicted)
                }
                Some(buf) => {
                    buf.ref_count += 1;
                    // 复用命中（同 file 在 TTL 窗口内再访问）：戳清释放时间，
                    // 让 next drop 的 last_released_at 重新起算。
                    buf.last_released_at = None;
                    // mtime+size 双因子：同 mtime 粒度窗口内的外部改写靠 size 检出。
                    // 记账侧任一为 None（stat 异常）→ 保守按「变了」处理。
                    let unchanged =
                        mtime.is_some() && size.is_some() && mtime == buf.mtime && size == buf.size;
                    if unchanged {
                        (Action::None, buf.content_version, lru_evicted)
                    } else {
                        let text = fs::read_to_string(path).map_err(CoreError::Io)?;
                        let version = buf.content_version + 1;
                        buf.mtime = mtime;
                        buf.size = size;
                        buf.content_version = version;
                        let action = Action::Send {
                            method: "textDocument/didChange",
                            params: make_did_change(&uri, version, &text),
                        };
                        (action, version, lru_evicted)
                    }
                }
            }
        };

        match to_send {
            Action::Send { method, params } => {
                if let Err(e) = self.notify(method, params).await {
                    // 审计 F1 次生：notify 失败（典型=writer 死亡 channel 关）时回滚
                    // 记账，否则新插条目变孤儿（ref_count=1 无人 drop）且 mtime 记账
                    // 让后续 ensure_open 走复用分支永不补发 didOpen——该文件语义
                    // 请求全部拿空/旧结果。复用/didChange 路径同理回滚 ref_count。
                    let mut map = self.buffers.lock().expect("docsync buffers mutex poisoned");
                    match map.get_mut(&uri) {
                        // ref_count==1：唯一持有者就是本次失败的调用方，条目记账
                        // （mtime/version）已不可信，直接移表让下次冷启动重新 didOpen。
                        Some(buf) if buf.ref_count == 1 => {
                            map.remove(&uri);
                        }
                        // 并发 ensure_open 抢先 +1 过：只回滚本次计数。
                        Some(buf) => {
                            buf.ref_count = buf.ref_count.saturating_sub(1);
                            if buf.ref_count == 0 {
                                buf.last_released_at = Some(Instant::now());
                            }
                        }
                        None => {}
                    }
                    return Err(e);
                }
            }
            Action::None => {}
        }

        // 修 P1 #1：LRU 淘汰的条目锁外发 didClose（outbound 关 send 失败忽略）。
        // 注意 LRU 辅助是同步函数，drain 已发生；此处不影响锁纪律。
        // 走 client().notify（同步）而非 self.notify（async）：避免再走一次 ready
        // gate 等门——session 此时已 Ready，且 notify 等门是冗余开销。
        for uri in &lru_evicted {
            let _ = self
                .client()
                .notify("textDocument/didClose", make_did_close(uri));
        }

        Ok(FileGuard {
            session: Arc::clone(self),
            uri,
        })
    }

    /// 强制回收所有缓冲：发 didClose + 移表。仅当调用方确认要立即关闭所有文档
    /// 时使用（daemon shutdown / 测试清理 / LS 内存压力回收）。
    /// 锁内 drain 取待发列表，锁外发通知（outbound 关 send 失败忽略）。
    pub fn evict_all_buffers(&self) {
        let to_close: Vec<Uri> = {
            let mut map = self.buffers.lock().expect("docsync buffers mutex poisoned");
            map.drain().map(|(uri, _)| uri).collect()
        };
        for uri in &to_close {
            let _ = self
                .client()
                .notify("textDocument/didClose", make_did_close(uri));
        }
    }

    /// 批量 `ensure_open`（修 P0-A 50 文件挂死）：对 N 个文件并行调用 `ensure_open`，
    /// 每路先 `sleep(params.debounce_ms)` 让前一波流到 LS —— 把扇出产生的
    /// `didOpen/didChange` 洪泛合并成 N 个时间上错开的脉冲，缓解 LS 内部队列拥塞。
    ///
    /// `send_immediate=true`：跳过 debounce，等价于对每个 path 串行/并发调 `ensure_open`，
    /// 行为与逐文件调完全一致。
    ///
    /// 实现：JoinSet 有界并发（MAX_INFLIGHT=4）—— 与 `tool_symbol_tree` 同形态；
    /// 单文件失败 → 收集到 `errors`，不阻断其他文件（与 overview 路径容错一致）。
    /// 返回：`Vec<Result<FileGuard>>` 与输入 `paths` 同序 —— 调用方按索引对齐原数据。
    pub async fn ensure_open_batch(
        self: &Arc<Self>,
        paths: &[&Path],
        params: EnsureOpenParams,
    ) -> Vec<Result<FileGuard>> {
        let mut results: Vec<Option<Result<FileGuard>>> = (0..paths.len()).map(|_| None).collect();
        let mut set = tokio::task::JoinSet::new();
        let max_inflight = 4usize;

        for (idx, path) in paths.iter().enumerate() {
            // 槽位耗尽 → 等一个完成再放新的
            while set.len() >= max_inflight {
                if let Some(Ok((i, r))) = set.join_next().await {
                    results[i] = Some(r);
                }
            }
            let session = Arc::clone(self);
            let path = path.to_path_buf();
            set.spawn(async move {
                if !params.send_immediate && params.debounce_ms > 0 {
                    sleep(std::time::Duration::from_millis(params.debounce_ms)).await;
                }
                let r = session.ensure_open(&path).await;
                (idx, r)
            });
        }
        // drain 剩余
        while let Some(Ok((i, r))) = set.join_next().await {
            results[i] = Some(r);
        }
        results
            .into_iter()
            .enumerate()
            .map(|(i, opt)| {
                opt.unwrap_or_else(|| {
                    Err(CoreError::Io(std::io::Error::other(format!(
                        "ensure_open_batch slot {i} dropped"
                    ))))
                })
            })
            .collect()
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
            let mut map = self.buffers.lock().expect("docsync buffers mutex poisoned");
            let now_idle: Vec<Uri> = map
                .iter()
                .filter(|(_, buf)| {
                    buf.ref_count == 0 && buf.last_released_at.is_some_and(|t| t.elapsed() >= ttl)
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

    /// 文件当前是否在 buffers 表（LS 侧文档态存活，含 TTL 宽限窗口内的条目）。
    /// undo/redo LS 同步用：表外 = 本 daemon 从未 didOpen（或已 evict），LS 侧无
    /// 陈旧文档态，跳过 didChange 避免 LS 收到"未打开文档"的变更。
    pub fn is_open(&self, path: &Path) -> bool {
        match path_to_uri(path) {
            Ok(uri) => self
                .buffers
                .lock()
                .expect("docsync buffers mutex poisoned")
                .contains_key(&uri),
            Err(_) => false,
        }
    }

    /// 强制关闭单个文件的 LS 文档态：移表 + didClose（锁内移表、锁外发通知，
    /// outbound 关闭时 send 失败忽略 —— 与 evict_* 同款纪律）。mirror ls.py 的
    /// close 语义：LS 端文档表删除该 uri。
    ///
    /// undo 删除 created 文件专用：文件已不存在，didChange 无从谈起，必须走
    /// didClose 让 LS 丢弃文档态。表中无此 uri = LS 从未见过该文件 → no-op。
    /// 残余活 guard 的 drop 打在已移除表项上 = no-op（`FileGuard::drop` 容忍缺项）。
    /// 返回是否真的关闭（表内存在并移除）。
    pub fn force_close(&self, path: &Path) -> bool {
        let Ok(uri) = path_to_uri(path) else {
            return false;
        };
        let removed = {
            let mut map = self.buffers.lock().expect("docsync buffers mutex poisoned");
            map.remove(&uri).is_some()
        };
        if removed {
            // 走 client().notify（同步）而非 self.notify（async）：与 evict_* 同款，
            // 调用方（undo 收口）持有 session 时它必已过 ready 门，无需再等门。
            let _ = self
                .client()
                .notify("textDocument/didClose", make_did_close(&uri));
        }
        removed
    }
}

/// 把绝对路径转成 `file://` URL 字符串。`dunce` 去 UNC 前缀，`\` → `/`，
/// URI 不安全字符（非 ASCII/空格/`#`/`?`/`%`）按 UTF-8 百分号编码。
pub fn path_to_uri_str(path: &Path) -> String {
    let lossy = path.to_string_lossy().replace('\\', "/");
    let encoded = utf8_percent_encode(&lossy, PATH_UNSAFE).collect::<String>();
    if lossy.starts_with('/') {
        format!("file://{encoded}")
    } else {
        format!("file:///{encoded}")
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

/// LS 返回的 `file://` URI → 本地路径（bd avw / audit-secdesign S7 的 lsp-core
/// 侧归一原语）：percent-decode + Windows 盘符大写 + dunce canonicalize（失败退
/// 词法形态）。凡把 **LS 提供的 uri** 变成磁盘路径再读/写的消费方，必须经本函数
/// 归一后走 [`uri_in_root`] 做 containment 过滤——LS 是半信任面，恶意/异常 uri
/// （绝对路径注入、`..` 穿越、盘符小写绕过前缀比对）不得触发越权读。
pub fn uri_to_path(uri: &str) -> Option<std::path::PathBuf> {
    use percent_encoding::percent_decode_str;
    let stripped = uri.strip_prefix("file://")?;
    // Windows: `file:///C:/foo` → `C:/foo`（保留 `//server/share` UNC 形态）。
    let decoded = percent_decode_str(stripped).decode_utf8().ok()?;
    let mut s = if cfg!(windows) && decoded.starts_with('/') && !decoded.starts_with("//") {
        decoded[1..].to_string()
    } else {
        decoded.to_string()
    };
    // 盘符小写归一（RA 推 `file:///d:/` 小写盘符的 Windows 双重身份教训）：
    // 存在路径 canonicalize 会纠 case，此兜底保证不存在的词法路径也能对上前缀。
    if cfg!(windows)
        && s.as_bytes().get(1) == Some(&b':')
        && s.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
    {
        s[..1].make_ascii_uppercase();
    }
    let path = std::path::PathBuf::from(s);
    Some(dunce::canonicalize(&path).unwrap_or_else(|_| lexically_normalize(&path)))
}

/// 词法归一：解析 `.`/`..` 组件（canonicalize 只对存在的路径生效，LS uri 指向
/// 不存在文件时 containment 靠它拒绝 `..` 穿越）。
fn lexically_normalize(p: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// [`uri_to_path`] 的 containment 门：LS uri 归一后的路径是否落在 `root` 内。
/// 双侧 canonicalize（失败退词法归一形态），Windows 前缀比对大小写不敏感
/// （路径/盘符双重身份）；`root == path` 视为在内。根自身非绝对 → false（调用方
/// 传错 root 不得静默放行）。
pub fn uri_in_root(uri: &str, root: &Path) -> bool {
    if !root.is_absolute() {
        return false;
    }
    let Some(path) = uri_to_path(uri) else {
        return false;
    };
    let root_canon = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    contains_norm(&lexically_normalize(&path), &lexically_normalize(&root_canon))
}

/// 前缀包含比对：分隔符统一 `/`，Windows 不分大小写（盘符/路径双重身份），
/// 边界守卫保证 `/proj-evil` 不命中 root `/proj`。
fn contains_norm(path: &Path, root: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s
        }
    };
    let (p, r) = (norm(path), norm(root));
    if p == r {
        return true;
    }
    p.strip_prefix(&r).is_some_and(|rest| rest.starts_with('/'))
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

/// LRU 容量闸门（修 P1 #1）：缓冲池达容量上限时，按 `last_released_at` 升序淘汰
/// 空闲条目，直到池大小 < `capacity`。活跃条目（`ref_count > 0`）一律不动——
/// 容量压力下也允许短时 overflow（拒绝为容量限制而打断活跃工具调用）。
///
/// 调用方：必须在持 `buffers` 锁时调用；返回被淘汰的 URI 列表，调用方**锁外**
/// 走 didClose 通知（outbound 关 send 失败忽略）。
///
/// 复杂度：O(n log n)（按 `last_released_at` 排序），n = 池大小。32 条目下
/// 微秒级；超 1000 文件 OOM 不会发生（容量闸门本就该在前面）。
///
/// `lsp_types::Uri` 含 `UnsafeCell`（内部 path percent-encoding cache），
/// `clippy::mutable_key_type` 静态告警——但调用方持锁单线程访问，运行时安全。
#[allow(clippy::mutable_key_type)]
fn evict_lru_idle_locked(
    map: &mut std::collections::HashMap<Uri, FileBuffer>,
    capacity: usize,
) -> Vec<Uri> {
    if map.len() < capacity {
        return Vec::new();
    }
    // 收集空闲条目 (uri, last_released_at)；活跃 (ref_count>0) 或未释放
    // (last_released_at=None) 一律跳过。
    let mut idle: Vec<(Uri, Instant)> = map
        .iter()
        .filter_map(|(uri, buf)| {
            if buf.ref_count == 0 {
                buf.last_released_at.map(|t| (uri.clone(), t))
            } else {
                None
            }
        })
        .collect();
    if idle.is_empty() {
        // 全表皆活跃：允许本轮 overflow，调用方的 ensure_open 不会被阻塞。
        return Vec::new();
    }
    // 按 last_released_at 升序（最久未用在前）。稳定排序 + collect 出 URI。
    idle.sort_by_key(|(_, t)| *t);
    // 淘汰直到 len < capacity。
    let need_evict = map.len().saturating_sub(capacity - 1).max(1);
    let to_close: Vec<Uri> = idle
        .into_iter()
        .take(need_evict)
        .map(|(uri, _)| uri)
        .collect();
    for uri in &to_close {
        map.remove(uri);
    }
    to_close
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bd avw：percent-decode + Windows 盘符大写归一（supervisor uri_to_path
    /// 既有单测的形态对齐版）。
    #[test]
    fn uri_to_path_decodes_percent_and_normalizes() {
        let p = uri_to_path("file:///d%3A/proj/foo%20bar/a.rs").expect("file uri");
        let s = p.to_string_lossy().replace('\\', "/");
        assert!(!s.contains('%'), "percent 序列必须解码: {s}");
        assert!(s.ends_with("foo bar/a.rs"), "got: {s}");
        if cfg!(windows) {
            assert!(s.starts_with("D:/"), "盘符必须大写: {s}");
        }
    }

    #[test]
    fn uri_to_path_rejects_non_file_scheme() {
        assert!(uri_to_path("https://x/a.rs").is_none());
        assert!(uri_to_path("untitled:untitled-1").is_none());
    }

    /// bd avw：containment 门——root 内放行；兄弟目录共享前缀拒；`..` 穿越
    /// （目标不存在、canonicalize 失效的词法路径）拒；非绝对 root 恒拒。
    #[test]
    fn uri_in_root_contains_and_rejects_escape() {
        let base = std::env::temp_dir();
        let root_dir = base.join("serena_avw_test");
        let inside = root_dir.join("a.rs");
        std::fs::create_dir_all(&root_dir).unwrap();
        std::fs::write(&inside, b"x").unwrap();
        let in_uri = path_to_uri_str(&inside);
        assert!(
            uri_in_root(&in_uri, &root_dir),
            "root 内文件必须在: {in_uri}"
        );

        // 兄弟目录共享前缀（…/serena_avw_test2）不得命中 root（…/serena_avw_test）。
        let sibling_uri = path_to_uri_str(&base.join("serena_avw_test2/a.rs"));
        assert!(
            !uri_in_root(&sibling_uri, &root_dir),
            "兄弟目录（共享前缀）必须拒: {sibling_uri}"
        );

        // `..` 穿越且目标不存在（canonicalize 失效 → 词法归一兜底拒收）。
        let escape_uri =
            path_to_uri_str(&root_dir.join("../../serena_avw_escape/x"));
        assert!(
            !uri_in_root(&escape_uri, &root_dir),
            "穿越路径归一后落在 root 外必须拒: {escape_uri}"
        );

        // root 非绝对 → 恒 false。
        assert!(!uri_in_root(&in_uri, Path::new("relative/root")));

        let _ = std::fs::remove_dir_all(&root_dir);
    }
}
