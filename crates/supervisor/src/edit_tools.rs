//! Task 25: 文件编辑四件套（insert / replace / delete 在 symbol 体内）。
//!
//! 设计要点（上游 solidlsp `insert_text_*` / `replace_text_in_symbol` /
//!   `delete_text_in_symbol` 复刻 + 简化）：

//! - **入口统一**：先 documentSymbol 找 symbol range，再在 range 内做编辑；
//! - **走写门（write_gate）**：与 replace-body 串行化，避免并发修改撕裂；
//! - **C3 一致性链路**：盘 hash 对账 → atomic_write → didChange → 失效符号缓存；
//! - **不走 lsp-types::WorkspaceEdit**：直接读盘 + 改 byte + 写盘，绕过 LSP 文本编辑（更快、更可控）；
//! - **`start_line/end_line` 1-based 含端点**，与 find-symbol / read-file 一致。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use lsp_core::docsync::path_to_uri_str;
use lsp_core::error::CoreError;

use lsp_core::offsets::{OffsetEncoding, Position as LspPos};
use lsp_core::session::Session;
use lsp_types::{DocumentSymbolResponse, Position};
use serde_json::json;
use thiserror::Error;

use crate::write_gate;

#[derive(Debug, Error)]
pub enum EditError {
    #[error("bad args: {detail}")]
    BadArgs { detail: String },
    /// 盘上 hash 与 expected_hash 不符（C3 防线 ①）—— 拒写，需重读文件。
    #[error("write conflict on {path}: {reason}")]
    WriteConflict { path: String, reason: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("core: {0}")]
    Core(#[from] CoreError),
}

pub type EditResult<T> = std::result::Result<T, EditError>;

/// 在 `root/file` 内定位 `symbol_name`（递归 documentSymbol 找第一个 name 匹配）。
/// 失败 → BadArgs。
async fn locate_symbol(
    session: &Arc<Session>,
    file: &Path,
    symbol_name: &str,
) -> EditResult<lsp_types::Range> {
    let uri = path_to_uri_str(file);
    let params = json!({ "textDocument": { "uri": uri } });
    let resp: Option<DocumentSymbolResponse> = session
        .request(
            "textDocument/documentSymbol",
            params,
            std::time::Duration::from_secs(30),
        )
        .await?;
    let range = find_first(resp.as_ref(), symbol_name).ok_or_else(|| EditError::BadArgs {
        detail: format!("symbol `{symbol_name}` not found"),
    })?;
    Ok(range)
}

fn find_first(resp: Option<&DocumentSymbolResponse>, name: &str) -> Option<lsp_types::Range> {
    match resp? {
        DocumentSymbolResponse::Nested(items) => walk_nested(items, name),
        DocumentSymbolResponse::Flat(_) => None,
    }
}

fn walk_nested(items: &[lsp_types::DocumentSymbol], name: &str) -> Option<lsp_types::Range> {
    for it in items {
        if it.name == name {
            return Some(it.range);
        }
        if let Some(children) = &it.children
            && let Some(r) = walk_nested(children, name)
        {
            return Some(r);
        }
    }
    None
}

/// 在 `text` 内 `[range.start, range.end]` 范围内，定位 `needle` 唯一出现处 byte 范围。
/// 返回 (start_byte, end_byte) 在 text 内的绝对 byte offset。
/// 多处命中 → BadArgs 拒改。拍板（bd serena-rust-pz4q）：默认拒改而非 replace-all
/// 或 warning——错误强制 AI 收敛 needle 语义，warning 会被忽略重现「静默改首处」
/// 盲测事故；`--all/--first` 旗需动 crates/cli，本票只收 supervisor 侧。
fn locate_in_range(
    path: &Path,
    text: &str,
    range: lsp_types::Range,
    symbol: &str,
    needle: &str,
    enc: OffsetEncoding,
) -> EditResult<(usize, usize)> {
    let start_byte = lsp_core::offsets::position_to_byte(
        text,
        LspPos {
            line: range.start.line,
            character: range.start.character,
        },
        enc,
    )
    .map_err(|e| EditError::BadArgs {
        detail: format!("range start: {e}"),
    })?;
    let end_byte = lsp_core::offsets::position_to_byte(
        text,
        LspPos {
            line: range.end.line,
            character: range.end.character,
        },
        enc,
    )
    .map_err(|e| EditError::BadArgs {
        detail: format!("range end: {e}"),
    })?;
    // 匹配/统计域扩到 range 首尾行**整行**：pyright 等对函数 range 在语句末尾截断
    // （不含行尾注释），同行注释里的同词命中落在 range 外被漏计（bd serena-rust-pz4q
    // rework：同行双命中静默改首处实测）。行首回退只含缩进空白、行尾推进只含注释，
    // 不会卷入相邻符号。
    let lo = text[..start_byte].rfind('\n').map_or(0, |i| i + 1);
    let hi = text[end_byte..]
        .find('\n')
        .map_or(text.len(), |i| end_byte + i);
    let body = &text[lo..hi];
    let hit_count = body.matches(needle).count();
    let rel = body.find(needle).ok_or_else(|| {
        // bd serena-rust-i4j：needle 在符号体内找不到 ≠ 参数错 —— 符号 range 是
        // 拿门后现解析的，此处失配说明盘上内容已被并发写改掉。报 BAD_ARGS 会
        // 误导 agent 误诊参数、盲目重试；归 WRITE_CONFLICT（重读重试语义）。
        EditError::WriteConflict {
            path: path.display().to_string(),
            reason: "needle not found in symbol body (content changed under a concurrent write)"
                .into(),
        }
    })?;
    if hit_count > 1 {
        return Err(EditError::BadArgs {
            detail: format!(
                "old_text matches {hit_count} times inside symbol `{symbol}` ({}); \
                 refine old_text to match exactly one occurrence",
                path.display()
            ),
        });
    }
    Ok((lo + rel, lo + rel + needle.len()))
}

/// 把 LSP Position 换算为绝对 byte offset。
fn pos_to_byte(text: &str, line: u32, col: u32, enc: OffsetEncoding) -> EditResult<usize> {
    lsp_core::offsets::position_to_byte(
        text,
        LspPos {
            line,
            character: col,
        },
        enc,
    )
    .map_err(|e| EditError::BadArgs {
        detail: format!("position {line}:{col}: {e}"),
    })
}

/// 取 `range.start` 行的行首缩进（空白前缀；空行/顶格返回空串）。
fn line_indent_of(text: &str, line_0based: u32) -> String {
    text.lines()
        .nth(line_0based as usize)
        .map(|l| {
            let ws_end = l.len() - l.trim_start().len();
            l[..ws_end].to_string()
        })
        .unwrap_or_default()
}

/// bd bt3h：auto-indent —— 多行 text 的第 2..n 行按 host 符号缩进补齐（首行原样，
/// 它接在插入点所在行）。已带 host 缩进（或更深）的行、空行不动，避免双重缩进。
fn auto_indent_text(text: &str, indent: &str) -> String {
    if indent.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + text.len() / 4);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
            if !line.is_empty() && !line.starts_with(indent) {
                out.push_str(indent);
            }
        }
        out.push_str(line);
    }
    out
}

/// 修改文件 + atomic_write + didChange 全量同步（与 replace-body 一致）。
///
/// didChange 走 `session.ensure_open` —— 它按 mtime 检测是否需重发，
/// 并用内部递增的 `content_version`（与 didOpen 起始版本号连续）。
/// root cause：单写门后多个 write 工具各自维护 version 计数器，
/// ls 收到的不是单调递增序列 → 拒响应 / EOF channel。
async fn commit_change(
    session: &Arc<Session>,
    file: &Path,

    #[allow(unused_variables)] root: &Path,
    new_content: &str,
) -> EditResult<()> {
    // undo 收口：快照旧内容 → 原子写 → 入当前事务（符号级三件套与行级三件套
    // 的公共写点）。io::Error 经 EditError::Io 冒泡，错误面与原 atomic_write 一致。
    crate::undo::recorded_write(file, new_content).await?;
    // mtime 推进 → docsync 自动发 version=prev+1 的 didChange 全量。
    let _refreshed = session.ensure_open(file).await?;
    Ok(())
}

/// `replace_text_in_symbol`：在 symbol 体内找 `old_text` 替换为 `new_text`。
pub async fn replace_text_in_symbol(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    symbol: &str,
    old_text: &str,
    new_text: &str,
) -> EditResult<()> {
    let _gate = write_gate::acquire("replace-text-in-symbol").await?;
    let _guard = session.ensure_open(file).await?;
    let range = locate_symbol(session, file, symbol).await?;
    let content = tokio::fs::read_to_string(file).await?;
    let (lo, hi) = locate_in_range(file, &content, range, symbol, old_text, OffsetEncoding::Utf16)?;
    let mut new_content = String::with_capacity(content.len() + new_text.len());
    new_content.push_str(&content[..lo]);
    new_content.push_str(new_text);
    new_content.push_str(&content[hi..]);
    commit_change(session, file, root, &new_content).await
}

/// `insert_text_after_symbol`：在 symbol 末尾（range.end）插入 text。
/// 返回插入内容末尾的 `(end_line, end_col)`（1-based，col 按字符计）。
/// bd bt3h：`auto_indent=true`（默认）时第 2..n 行按 host 符号起始行缩进补齐。
pub async fn insert_text_after_symbol(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    symbol: &str,
    text: &str,
    auto_indent: bool,
) -> EditResult<(u32, u32)> {
    let _gate = write_gate::acquire("insert-text-after-symbol").await?;
    let _guard = session.ensure_open(file).await?;
    let range = locate_symbol(session, file, symbol).await?;
    let content = tokio::fs::read_to_string(file).await?;
    // bd bt3h：插入点在符号末尾 —— 新内容层级与 host 符号起始行一致。
    let text = if auto_indent {
        auto_indent_text(text, &line_indent_of(&content, range.start.line))
    } else {
        text.to_string()
    };
    let end_byte = pos_to_byte(
        &content,
        range.end.line,
        range.end.character,
        OffsetEncoding::Utf16,
    )?;
    let new_content = format!("{}{}{}", &content[..end_byte], text, &content[end_byte..]);
    let (end_line, end_col) = end_line_col(&new_content, end_byte + text.len());
    commit_change(session, file, root, &new_content).await?;
    Ok((end_line, end_col))
}

/// `insert_text_before_symbol`：在 symbol 开头（range.start）插入 text。
/// bd bt3h：`auto_indent=true`（默认）时第 2..n 行按 host 符号起始行缩进补齐。
pub async fn insert_text_before_symbol(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    symbol: &str,
    text: &str,
    auto_indent: bool,
) -> EditResult<(u32, u32)> {
    let _gate = write_gate::acquire("insert-text-before-symbol").await?;
    let _guard = session.ensure_open(file).await?;
    let range = locate_symbol(session, file, symbol).await?;
    let content = tokio::fs::read_to_string(file).await?;
    // bd bt3h：插入点在符号起始行前 —— 新内容与 host 符号同层级。
    let text = if auto_indent {
        auto_indent_text(text, &line_indent_of(&content, range.start.line))
    } else {
        text.to_string()
    };
    let start_byte = pos_to_byte(
        &content,
        range.start.line,
        range.start.character,
        OffsetEncoding::Utf16,
    )?;
    let new_content = format!(
        "{}{}{}",
        &content[..start_byte],
        text,
        &content[start_byte..]
    );
    let (end_line, end_col) = end_line_col(&new_content, start_byte + text.len());
    commit_change(session, file, root, &new_content).await?;
    Ok((end_line, end_col))
}

/// `delete_text_in_symbol`：在 symbol 体内删除 `start_line..end_line` 切片（行号 1-based 含端）。
pub async fn delete_text_in_symbol(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    symbol: &str,
    start_line: u32,
    end_line: u32,
) -> EditResult<()> {
    let _gate = write_gate::acquire("delete-text-in-symbol").await?;
    let _guard = session.ensure_open(file).await?;
    let range = locate_symbol(session, file, symbol).await?;
    let content = tokio::fs::read_to_string(file).await?;
    // start_line/end_line 是 1-based；转 0-based 行号。
    let s = (start_line.saturating_sub(1)) as usize;
    let e = (end_line.saturating_sub(1)) as usize;
    let lines: Vec<&str> = content.lines().collect();
    if s >= lines.len() || e >= lines.len() || s > e {
        return Err(EditError::BadArgs {
            detail: format!(
                "invalid line range {start_line}..{end_line} (1..={})",
                lines.len()
            ),
        });
    }
    // 校验范围落在 symbol range 内（line 在 [range.start.line, range.end.line]）。
    if s < range.start.line as usize || e > range.end.line as usize {
        return Err(EditError::BadArgs {
            detail: format!(
                "line range {start_line}..{end_line} outside symbol body {}..{}",
                range.start.line + 1,
                range.end.line + 1
            ),
        });
    }
    // 计算 byte range：s 行开头到 e 行末。
    let mut start_byte = 0usize;
    for (i, l) in lines.iter().enumerate() {
        if i == s {
            break;
        }
        start_byte += l.len() + 1; // +1 for '\n'
    }
    let mut end_byte = start_byte;
    for (i, l) in lines.iter().enumerate().skip(s) {
        if i > e {
            break;
        }
        if i == e {
            end_byte += l.len();
            break;
        }
        end_byte += l.len() + 1;
    }
    let mut new_content = String::with_capacity(content.len());
    new_content.push_str(&content[..start_byte]);
    new_content.push_str(&content[end_byte..]);
    commit_change(session, file, root, &new_content).await
}

/// bd ou83：format-on-write —— 把 `textDocument/formatting` 的 TextEdit 落盘。
/// 走写门 + undo 收口（与其它写工具同一公共写点）；edits 按 start 倒序应用避免
/// 偏移漂移。返回应用条数。
pub async fn apply_format_edits(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    mut edits: Vec<lsp_types::TextEdit>,
) -> EditResult<usize> {
    if edits.is_empty() {
        return Ok(0);
    }
    let _gate = write_gate::acquire("format-on-write").await?;
    let _guard = session.ensure_open(file).await?;
    let mut new_content = tokio::fs::read_to_string(file).await?;
    edits.sort_by(|a, b| b.range.start.cmp(&a.range.start));
    for e in &edits {
        let lo = pos_to_byte(
            &new_content,
            e.range.start.line,
            e.range.start.character,
            OffsetEncoding::Utf16,
        )?;
        let hi = pos_to_byte(
            &new_content,
            e.range.end.line,
            e.range.end.character,
            OffsetEncoding::Utf16,
        )?;
        if lo > hi || hi > new_content.len() {
            return Err(EditError::BadArgs {
                detail: format!("format edit range out of bounds: {:?}", e.range),
            });
        }
        new_content.replace_range(lo..hi, &e.new_text);
    }
    commit_change(session, file, root, &new_content).await?;
    Ok(edits.len())
}

// ============ 行级三件套（全文行级，非符号体内；1-based 含端）============
// ↖ mirror: file_tools.py@43ae021 InsertAtLineTool / ReplaceLinesTool / DeleteLinesTool
// （上游 0-based → 本项目 1-based 含端，与 delete-text-in-symbol 一致；Δ expected_hash 对账）。

/// 每行起始 byte 偏移表：`starts[0]=0`，末尾追加 EOF 锚点 `text.len()`。
/// `"a\nb\n"` → `[0,2,4]`（total=2）；`"a\nb"` → `[0,2,3]`（total=2）；`""` → `[0]`（total=0）。
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    if *starts.last().unwrap_or(&0) != text.len() {
        starts.push(text.len());
    }
    starts
}

fn line_bounds_error(start: u32, end: u32, total: usize) -> EditError {
    EditError::BadArgs {
        detail: format!("line range {start}..{end} out of bounds (1..={total})"),
    }
}

/// 从结果文本反推插入内容末尾的 `(line, col)`（均 1-based，col 按 Unicode 标量字符计）。
///
/// ↖ mirror: ls_utils.py@9554456 insert_text_at_position — 位置必须从插入后的**结果文本**
/// 反推，而非用插入文本自身步进估算：插入落在既有 `\r` 之后的 `\n` 会与之合并成单个
/// `\r\n` 行界，只看插入文本会错位一行。Δ 上游 0-based → 本项目行级工具 1-based 约定。
fn end_line_col(new_text: &str, end_byte: usize) -> (u32, u32) {
    let upto = &new_text[..end_byte];
    let line = (upto.bytes().filter(|&b| b == b'\n').count() + 1).min(u32::MAX as usize) as u32;
    let line_start = upto.rfind('\n').map_or(0, |i| i + 1);
    let col = (upto[line_start..].chars().count() + 1).min(u32::MAX as usize) as u32;
    (line, col)
}

/// 在 `line`（1-based）前插入 content（规范化补尾 `\n`），原有行整体下移；
/// `line == total+1` 即追加到文件尾。返回 `(new_text, end_line, end_col)`：
/// end position = 插入内容末尾（1-based，col 按字符计）。
pub(crate) fn apply_insert_at_line(
    text: &str,
    line: u32,
    content: &str,
) -> EditResult<(String, u32, u32)> {
    let starts = line_starts(text);
    let total = starts.len() - 1;
    if line == 0 || line as usize > total + 1 {
        return Err(line_bounds_error(line, line, total + 1));
    }
    let pos = starts[(line - 1) as usize];
    let mut content = content.to_string();
    if !content.ends_with('\n') {
        content.push('\n'); // ↖ mirror: file_tools.py@43ae021 InsertAtLineTool.apply 内容规范化
    }
    let new_text = format!("{}{}{}", &text[..pos], content, &text[pos..]);
    let (end_line, end_col) = end_line_col(&new_text, pos + content.len());
    Ok((new_text, end_line, end_col))
}

/// 删除 `[start, end]` 行（1-based 含端）。
pub(crate) fn apply_delete_lines(text: &str, start: u32, end: u32) -> EditResult<String> {
    let starts = line_starts(text);
    let total = starts.len() - 1;
    // 杠精 07u5-7：start>end 是「顺序错」不是「越界」——文案区分两种失败，
    // AI 才能直接换算重试而不是盲目缩范围。
    if start != 0 && end < start {
        return Err(EditError::BadArgs {
            detail: format!(
                "line range {start}..{end}: wrong order — start_line must be <= end_line (both 1-based)"
            ),
        });
    }
    if start == 0 || end as usize > total {
        return Err(line_bounds_error(start, end, total));
    }
    let from = starts[(start - 1) as usize];
    let to = starts[end as usize]; // EOF 锚点：删到末行时自然收尾
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..from]);
    out.push_str(&text[to..]);
    Ok(out)
}

/// 用 content 整体替换 `[start, end]` 行（规范化补尾 `\n`）。
pub(crate) fn apply_replace_lines(
    text: &str,
    start: u32,
    end: u32,
    content: &str,
) -> EditResult<String> {
    let starts = line_starts(text);
    let total = starts.len() - 1;
    if start == 0 || end < start || end as usize > total {
        return Err(line_bounds_error(start, end, total));
    }
    let from = starts[(start - 1) as usize];
    let to = starts[end as usize];
    let mut content = content.to_string();
    if !content.ends_with('\n') {
        content.push('\n'); // ↖ mirror: file_tools.py@43ae021 ReplaceLinesTool.apply
    }
    Ok(format!("{}{}{}", &text[..from], content, &text[to..]))
}

/// hash 对账（C3 防线 ①）：expected 与盘上全文 hash 不符 → 拒写 WRITE_CONFLICT。
pub(crate) fn verify_hash(expected: Option<&str>, actual: &str, path: &Path) -> EditResult<()> {
    let Some(want) = expected else {
        return Ok(());
    };
    let got = crate::content_hash(actual);
    if got != want {
        return Err(EditError::WriteConflict {
            path: path.display().to_string(),
            reason: format!(
                "content hash mismatch: disk={got} expected={want}; re-read the file first"
            ),
        });
    }
    Ok(())
}

/// 行级写事务公共链路（与 replace-body 一致）：写门 → ensure_open → 读盘 →
/// hash 对账 → 行变换 → atomic_write + didChange 全量。变换产出 `(new_text, T)`，
/// T 供调用方附带返回值（如 insert 的 end position）。
async fn line_edit<T, F>(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    expected_hash: Option<&str>,
    transform: F,
) -> EditResult<T>
where
    F: FnOnce(&str) -> EditResult<(String, T)>,
{
    let _gate = write_gate::acquire("line-edit").await?;
    let _guard = session.ensure_open(file).await?;
    let content = tokio::fs::read_to_string(file).await?;
    verify_hash(expected_hash, &content, file)?;
    let (new_content, extra) = transform(&content)?;
    commit_change(session, file, root, &new_content).await?;
    Ok(extra)
}

/// `insert-at-line`：在 `line`（1-based）前插入，原行下移；`line == total+1` 追加 EOF。
/// 返回插入内容末尾的 `(end_line, end_col)`（1-based）。
pub async fn insert_at_line(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    line: u32,
    content: &str,
    expected_hash: Option<&str>,
) -> EditResult<(u32, u32)> {
    line_edit(session, root, file, expected_hash, |t| {
        apply_insert_at_line(t, line, content)
            .map(|(s, end_line, end_col)| (s, (end_line, end_col)))
    })
    .await
}

/// `replace-lines`：用 content 替换 `[start_line, end_line]`（1-based 含端）。
pub async fn replace_lines(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    start_line: u32,
    end_line: u32,
    content: &str,
    expected_hash: Option<&str>,
) -> EditResult<()> {
    line_edit(session, root, file, expected_hash, |t| {
        apply_replace_lines(t, start_line, end_line, content).map(|s| (s, ()))
    })
    .await
}

/// `delete-lines`：删除 `[start_line, end_line]`（1-based 含端）。
pub async fn delete_lines(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    start_line: u32,
    end_line: u32,
    expected_hash: Option<&str>,
) -> EditResult<()> {
    line_edit(session, root, file, expected_hash, |t| {
        apply_delete_lines(t, start_line, end_line).map(|s| (s, ()))
    })
    .await
}

// 抑制未使用 Position 警告（备扩展）。
#[allow(dead_code)]
fn _force_use_position(_p: Position) -> PathBuf {
    PathBuf::new()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_starts_anchors() {
        assert_eq!(line_starts("a\nb\n"), vec![0, 2, 4]);
        assert_eq!(line_starts("a\nb"), vec![0, 2, 3]);
        assert_eq!(line_starts(""), vec![0]);
    }

    #[test]
    fn insert_pushes_lines_down_and_appends() {
        // 首行前插入；插入内容以 \n 结尾 → 末尾位置落在下一行行首。
        let (out, l, c) = apply_insert_at_line("int a = 1;\nint b = 2;\n", 1, "// hdr\n").unwrap();
        assert_eq!(out, "// hdr\nint a = 1;\nint b = 2;\n");
        assert_eq!((l, c), (2, 1));
        // 中间插入 + content 缺尾换行自动补；补入的 \n 属插入内容
        // → 尾 = 其后一行行首（原 b 行，新第 3 行）。
        let (out, l, c) =
            apply_insert_at_line("int a = 1;\nint b = 2;\n", 2, "int c = 3;").unwrap();
        assert_eq!(out, "int a = 1;\nint c = 3;\nint b = 2;\n");
        assert_eq!((l, c), (3, 1));
        // total+1 = 追加 EOF；插入内容以 \n 结尾 → 尾 = one-past-EOF 行首
        // （"末尾之后"语义，与该坐标可直接作为下次 insert-at-line 的 line 复用）。
        let (out, l, c) = apply_insert_at_line("int a = 1;\n", 2, "int b = 2;\n").unwrap();
        assert_eq!(out, "int a = 1;\nint b = 2;\n");
        assert_eq!((l, c), (3, 1));
    }

    /// ↖ mirror: ls_utils.py@9554456（PR #1842）— 插入多行内容后，end position 必须
    /// 等于插入内容在**结果文本**中的末尾；CRLF 文件行界按 `\n` 计，`\r` 不多算一行；
    /// col 按 Unicode 字符计而非字节。
    #[test]
    fn insert_end_position_points_at_insertion_tail() {
        // 多行插入："x\ny\n" 插到原第 2 行前 → 新文本行序 a/x/y/b，尾 \n 已推进
        // → end position = 第 4 行行首（原 b 行），即插入内容末尾之后。
        let (_, l, c) = apply_insert_at_line("a\nb\n", 2, "x\ny\n").unwrap();
        assert_eq!((l, c), (4, 1));
        // CRLF 文件：插入 "c\n" 后尾 = 新第 3 行行首（原 b 行）；行界按 \n 计，
        // 行中 \r 不产生额外行号偏移。
        let (_, l, c) = apply_insert_at_line("a\r\nb\r\n", 2, "c\n").unwrap();
        assert_eq!((l, c), (3, 1));
        // col 按 Unicode 标量字符计而非字节："çé" 4 字节 = 2 字符。
        assert_eq!(end_line_col("a\nçéb", "a\nçé".len()), (2, 3));
    }

    #[test]
    fn insert_out_of_bounds_rejected() {
        let err = apply_insert_at_line("a\nb\n", 4, "x\n").unwrap_err();
        assert!(err.to_string().contains("out of bounds"), "{err}");
        assert!(apply_insert_at_line("a\n", 0, "x\n").is_err());
    }

    #[test]
    fn delete_removes_inclusive_range() {
        assert_eq!(apply_delete_lines("1\n2\n3\n", 2, 2).unwrap(), "1\n3\n");
        assert_eq!(apply_delete_lines("1\n2\n3\n", 1, 3).unwrap(), "");
        // 末行无尾换行也能删干净。
        assert_eq!(apply_delete_lines("1\n2", 2, 2).unwrap(), "1\n");
    }

    #[test]
    fn delete_out_of_bounds_rejected() {
        assert!(
            apply_delete_lines("1\n2\n", 2, 3)
                .unwrap_err()
                .to_string()
                .contains("out of bounds")
        );
        // 杠精 07u5-7：start>end 文案 = 顺序错，不再误报越界。
        assert!(
            apply_delete_lines("1\n2\n", 2, 1)
                .unwrap_err()
                .to_string()
                .contains("wrong order")
        );
        assert!(
            apply_delete_lines("1\n2\n", 0, 1)
                .unwrap_err()
                .to_string()
                .contains("out of bounds")
        );
    }

    #[test]
    fn replace_swaps_range() {
        assert_eq!(
            apply_replace_lines("1\n2\n3\n", 2, 2, "two").unwrap(),
            "1\ntwo\n3\n"
        );
        assert_eq!(
            apply_replace_lines("1\n2\n3\n", 1, 3, "x\ny\n").unwrap(),
            "x\ny\n"
        );
    }

    #[test]
    fn replace_out_of_bounds_rejected() {
        assert!(apply_replace_lines("1\n", 1, 2, "x\n").is_err());
    }

    #[test]
    fn hash_mismatch_rejects_write() {
        let path = Path::new("demo.cpp");
        verify_hash(None, "anything", path).unwrap(); // 不传 hash = 跳过对账
        let good = crate::content_hash("int a = 1;\n");
        verify_hash(Some(&good), "int a = 1;\n", path).unwrap();
        let err = verify_hash(Some(&good), "int a = 2;\n", path).unwrap_err();
        assert!(
            matches!(err, EditError::WriteConflict { .. }),
            "hash 失配必须拒写，实际: {err}"
        );
    }

    /// bd serena-rust-pz4q：old_text 在符号体内多处命中必须拒改（静默改首处盲测实锤）；
    /// 单命中照旧可替换，零命中保持 WRITE_CONFLICT（bd i4j 语义不变）。
    #[test]
    fn multi_hit_needle_rejected_single_hit_ok() {
        let path = Path::new("calc.py");
        let text = "def divide(x, y):\n    \"\"\"uses x - y.\"\"\"\n    return x - y\n";
        let range = lsp_types::Range {
            start: Position::new(0, 0),
            end: Position::new(2, 16),
        };
        let err =
            locate_in_range(path, text, range, "divide", "x - y", OffsetEncoding::Utf16)
                .unwrap_err();
        let EditError::BadArgs { detail } = &err else {
            panic!("多命中必须 BadArgs 拒改: {err:?}")
        };
        assert!(detail.contains("2 times"), "detail 必须含命中数: {detail}");

        let text1 = "def divide(x, y):\n    return x - y\n";
        let range1 = lsp_types::Range {
            start: Position::new(0, 0),
            end: Position::new(1, 16),
        };
        let (lo, hi) =
            locate_in_range(path, text1, range1, "divide", "x - y", OffsetEncoding::Utf16)
                .unwrap();
        assert_eq!(&text1[lo..hi], "x - y");

        let err0 = locate_in_range(path, text1, range1, "divide", "x * y", OffsetEncoding::Utf16)
            .unwrap_err();
        assert!(
            matches!(err0, EditError::WriteConflict { .. }),
            "零命中语义不变: {err0:?}"
        );
    }

    /// bd serena-rust-pz4q rework：pyright 函数 range 在语句末尾截断（不含行尾注释），
    /// 同行注释里的第二处命中必须计入统计域（匹配/统计域扩到 range 首尾行整行）。
    #[test]
    fn same_line_hit_in_trailing_comment_counts() {
        let path = Path::new("twofx.py");
        // range 照 pyright 实测形态：end (1,16) = `    return a - 1` 语句末尾，注释在 range 外。
        let text = "def helper(a):\n    return a - 1  # two hits mention a - 1\n";
        let range = lsp_types::Range {
            start: Position::new(0, 1),
            end: Position::new(1, 16),
        };
        let err =
            locate_in_range(path, text, range, "helper", "a - 1", OffsetEncoding::Utf16)
                .unwrap_err();
        let EditError::BadArgs { detail } = &err else {
            panic!("行尾注释中的第二处命中必须计入: {err:?}")
        };
        assert!(detail.contains("2 times"), "detail 必须含命中数: {detail}");

        // 对照：唯一命中在语句内（注释无同词）→ 单命中照旧返回语句内切片。
        let text1 = "def helper(a):\n    return a - 1  # halve it\n";
        let (lo, hi) =
            locate_in_range(path, text1, range, "helper", "a - 1", OffsetEncoding::Utf16)
                .unwrap();
        assert_eq!(&text1[lo..hi], "a - 1");
    }
}
