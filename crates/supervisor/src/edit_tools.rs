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

/// 在 `text` 内 `[range.start, range.end]` 范围内，定位 `needle` 第一次出现处 byte 范围。
/// 返回 (start_byte, end_byte) 在 text 内的绝对 byte offset。
fn locate_in_range(
    text: &str,
    range: lsp_types::Range,
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
    let body = &text[start_byte..end_byte];
    let rel = body.find(needle).ok_or_else(|| EditError::BadArgs {
        detail: "needle not found in symbol body".into(),
    })?;
    Ok((start_byte + rel, start_byte + rel + needle.len()))
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
    // atomic_write：tempfile 写 + rename（共享冲突重试 5×50ms）。
    atomic_write(file, new_content).await?;
    // mtime 推进 → docsync 自动发 version=prev+1 的 didChange 全量。
    let _refreshed = session.ensure_open(file).await?;
    Ok(())
}

/// tempfile 原子写 + rename（与 lib.rs::atomic_write 同语义，独立以免循环依赖）。
async fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    let tmp_path = tmp.into_temp_path().keep()?;
    tokio::fs::write(&tmp_path, content).await?;
    for attempt in 0..5 {
        match tokio::fs::rename(&tmp_path, path).await {
            Ok(()) => return Ok(()),
            Err(e) if attempt < 4 => {
                tokio::time::sleep(std::time::Duration::from_millis(50 * (attempt + 1))).await;
                let _ = e;
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(e);
            }
        }
    }
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
    let _gate = write_gate::acquire().await;
    let _guard = session.ensure_open(file).await?;
    let range = locate_symbol(session, file, symbol).await?;
    let content = tokio::fs::read_to_string(file).await?;
    let (lo, hi) = locate_in_range(&content, range, old_text, OffsetEncoding::Utf16)?;
    let mut new_content = String::with_capacity(content.len() + new_text.len());
    new_content.push_str(&content[..lo]);
    new_content.push_str(new_text);
    new_content.push_str(&content[hi..]);
    commit_change(session, file, root, &new_content).await
}

/// `insert_text_after_symbol`：在 symbol 末尾（range.end）插入 text。
pub async fn insert_text_after_symbol(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    symbol: &str,
    text: &str,
) -> EditResult<()> {
    let _gate = write_gate::acquire().await;
    let _guard = session.ensure_open(file).await?;
    let range = locate_symbol(session, file, symbol).await?;
    let content = tokio::fs::read_to_string(file).await?;
    let end_byte = pos_to_byte(
        &content,
        range.end.line,
        range.end.character,
        OffsetEncoding::Utf16,
    )?;
    let mut new_content = String::with_capacity(content.len() + text.len());
    new_content.push_str(&content[..end_byte]);
    new_content.push_str(text);
    new_content.push_str(&content[end_byte..]);
    commit_change(session, file, root, &new_content).await
}

/// `insert_text_before_symbol`：在 symbol 开头（range.start）插入 text。
pub async fn insert_text_before_symbol(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    symbol: &str,
    text: &str,
) -> EditResult<()> {
    let _gate = write_gate::acquire().await;
    let _guard = session.ensure_open(file).await?;
    let range = locate_symbol(session, file, symbol).await?;
    let content = tokio::fs::read_to_string(file).await?;
    let start_byte = pos_to_byte(
        &content,
        range.start.line,
        range.start.character,
        OffsetEncoding::Utf16,
    )?;
    let mut new_content = String::with_capacity(content.len() + text.len());
    new_content.push_str(&content[..start_byte]);
    new_content.push_str(text);
    new_content.push_str(&content[start_byte..]);
    commit_change(session, file, root, &new_content).await
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
    let _gate = write_gate::acquire().await;
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

/// 在 `line`（1-based）前插入 content（规范化补尾 `\n`），原有行整体下移；
/// `line == total+1` 即追加到文件尾。
pub(crate) fn apply_insert_at_line(text: &str, line: u32, content: &str) -> EditResult<String> {
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
    Ok(format!("{}{}{}", &text[..pos], content, &text[pos..]))
}

/// 删除 `[start, end]` 行（1-based 含端）。
pub(crate) fn apply_delete_lines(text: &str, start: u32, end: u32) -> EditResult<String> {
    let starts = line_starts(text);
    let total = starts.len() - 1;
    if start == 0 || end < start || end as usize > total {
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
/// hash 对账 → 行变换 → atomic_write + didChange 全量。
async fn line_edit<F>(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    expected_hash: Option<&str>,
    transform: F,
) -> EditResult<()>
where
    F: FnOnce(&str) -> EditResult<String>,
{
    let _gate = write_gate::acquire().await;
    let _guard = session.ensure_open(file).await?;
    let content = tokio::fs::read_to_string(file).await?;
    verify_hash(expected_hash, &content, file)?;
    let new_content = transform(&content)?;
    commit_change(session, file, root, &new_content).await
}

/// `insert-at-line`：在 `line`（1-based）前插入，原行下移；`line == total+1` 追加 EOF。
pub async fn insert_at_line(
    session: &Arc<Session>,
    root: &Path,
    file: &Path,
    line: u32,
    content: &str,
    expected_hash: Option<&str>,
) -> EditResult<()> {
    line_edit(session, root, file, expected_hash, |t| {
        apply_insert_at_line(t, line, content)
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
        apply_replace_lines(t, start_line, end_line, content)
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
        apply_delete_lines(t, start_line, end_line)
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
        // 首行前插入。
        assert_eq!(
            apply_insert_at_line("int a = 1;\nint b = 2;\n", 1, "// hdr\n").unwrap(),
            "// hdr\nint a = 1;\nint b = 2;\n"
        );
        // 中间插入 + content 缺尾换行自动补。
        assert_eq!(
            apply_insert_at_line("int a = 1;\nint b = 2;\n", 2, "int c = 3;").unwrap(),
            "int a = 1;\nint c = 3;\nint b = 2;\n"
        );
        // total+1 = 追加 EOF。
        assert_eq!(
            apply_insert_at_line("int a = 1;\n", 2, "int b = 2;\n").unwrap(),
            "int a = 1;\nint b = 2;\n"
        );
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
        assert!(
            apply_delete_lines("1\n2\n", 2, 1)
                .unwrap_err()
                .to_string()
                .contains("out of bounds")
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
}
