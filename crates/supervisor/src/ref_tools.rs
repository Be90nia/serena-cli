//! Task 24: 引用查询工具（find_referencing_symbols / find_referencing_code_snippets）。
//!
//! 设计要点（上游 solidlsp `find_referencing_*` 复刻 + 简化）：
//! - **入口统一**：`textDocument/references` 拿 `Location[]`；
//! - **find_referencing_symbols**：每个 ref 反查 `textDocument/documentSymbol` 找**外层
//!   容器**（类/方法/函数名），按 file + 容器聚类去重；
//! - **find_referencing_code_snippets**：每个 ref 取前后 N 行代码片段；
//! - **同 file 的多 ref 共享一次 documentSymbol / read_to_string**（N+1 → 1+1 调用）。
//!
//! ponylabel: 容器推断走 documentSymbol；clangd 对 100+ ref 文件 docSymbol 耗时 ~50ms。
//! ponytail: 升级到 LSP `callHierarchy`（需服务器能力声明，clangd 不支持）。

use std::collections::{HashMap, HashSet};
use std::path::Path;

use lsp_core::docsync::path_to_uri_str;
use lsp_core::session::Session;

use crate::uri_to_path;

use std::sync::Arc;

use lsp_types::{DocumentSymbol, DocumentSymbolResponse, Location, Position, Range};
use serde::Serialize;
use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RefError {
    #[error("bad args: {detail}")]
    BadArgs { detail: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("core: {0}")]
    Core(String),
}

pub type RefResult<T> = std::result::Result<T, RefError>;

/// `find_referencing_symbols` 的单条结果（按 file + 容器聚类）。
#[derive(Debug, Serialize)]
pub struct RefSymbolHit {
    pub file: String,
    pub line: u32,
    pub col: u32,
    /// ref 落在哪个外层符号里（类/方法/函数名）。顶层时空字符串。
    pub container_name: String,
}

/// `find_referencing_code_snippets` 的单条结果。
#[derive(Debug, Serialize)]
pub struct RefSnippetHit {
    pub file: String,
    pub line: u32,
    pub col: u32,
    /// ref 处单行（trim 末尾换行）。
    pub text: String,
    /// ref 前后 N 行的代码片段（多行用 `\n` 分隔）。
    pub snippet: String,
}

async fn fetch_references(
    session: &Arc<Session>,

    file: &Path,
    line: u32,
    col: u32,
) -> RefResult<Vec<Location>> {
    let _guard = session
        .ensure_open(file)
        .await
        .map_err(|e| RefError::Core(format!("ensure_open: {e}")))?;
    let uri = path_to_uri_str(file);
    let params = json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": col },
        "context": { "includeDeclaration": true },
    });
    let raw: Option<serde_json::Value> = session
        .request(
            "textDocument/references",
            params,
            std::time::Duration::from_secs(30),
        )
        .await
        .map_err(|e| RefError::Core(format!("references: {e}")))?;
    Ok(crate::normalize_implementations(raw.as_ref()))
}

async fn fetch_document_symbols(
    session: &Arc<Session>,
    file: &Path,
) -> RefResult<Vec<DocumentSymbol>> {
    let _guard = session
        .ensure_open(file)
        .await
        .map_err(|e| RefError::Core(format!("ensure_open: {e}")))?;
    let uri = path_to_uri_str(file);
    let params = json!({ "textDocument": { "uri": uri } });
    let resp: Option<DocumentSymbolResponse> = session
        .request(
            "textDocument/documentSymbol",
            params,
            std::time::Duration::from_secs(30),
        )
        .await
        .map_err(|e| RefError::Core(format!("documentSymbol: {e}")))?;
    let mut out = Vec::new();
    match resp {
        Some(DocumentSymbolResponse::Nested(items)) => {
            flatten(&items, &mut out);
        }
        Some(DocumentSymbolResponse::Flat(items)) => {
            for it in items {
                out.push(DocumentSymbol {
                    name: it.name.clone(),
                    detail: None,
                    kind: it.kind,
                    tags: None,
                    #[allow(deprecated)]
                    deprecated: None,
                    range: it.location.range,
                    selection_range: it.location.range,
                    children: None,
                });
            }
        }
        None => {}
    }
    Ok(out)
}

fn flatten(items: &[DocumentSymbol], out: &mut Vec<DocumentSymbol>) {
    for it in items {
        out.push(DocumentSymbol {
            name: it.name.clone(),
            detail: it.detail.clone(),
            kind: it.kind,
            tags: it.tags.clone(),
            #[allow(deprecated)]
            deprecated: it.deprecated,
            range: it.range,

            selection_range: it.selection_range,
            children: None,
        });
        if let Some(children) = &it.children {
            flatten(children, out);
        }
    }
}

fn contains(r: Range, p: Position) -> bool {
    let after_start =
        p.line > r.start.line || (p.line == r.start.line && p.character >= r.start.character);
    let before_end =
        p.line < r.end.line || (p.line == r.end.line && p.character <= r.end.character);
    after_start && before_end
}

fn area(r: Range) -> u64 {
    let h = (r.end.line as i64 - r.start.line as i64).unsigned_abs();
    let w = if h == 0 {
        (r.end.character as i64 - r.start.character as i64).unsigned_abs()
    } else {
        r.end.character as u64
    };
    h.saturating_mul(w)
}

fn find_container_name(symbols: &[DocumentSymbol], line: u32, col: u32) -> String {
    let pos = Position::new(line, col);
    let mut best: Option<&DocumentSymbol> = None;
    for sym in symbols {
        if contains(sym.range, pos) {
            let replace = match best {
                None => true,
                Some(b) => area(sym.range) < area(b.range),
            };
            if replace {
                best = Some(sym);
            }
        }
    }
    best.map(|s| s.name.clone()).unwrap_or_default()
}

pub async fn find_referencing_symbols(
    session: &Arc<Session>,
    root: &Path,
    file: &str,
    line: u32,
    col: u32,
) -> RefResult<Vec<RefSymbolHit>> {
    let canon_root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let abs_file = canon_root.join(file);
    let refs = fetch_references(session, &abs_file, line, col).await?;

    let mut by_file: HashMap<String, Vec<(u32, u32)>> = HashMap::new();
    for loc in refs {
        let abs = match uri_to_path(loc.uri.as_str()) {
            Some(p) => p,
            None => continue,
        };
        let rel = abs
            .strip_prefix(&canon_root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .replace('\\', "/");
        let (line, col) = (loc.range.start.line, loc.range.start.character);
        by_file.entry(rel).or_default().push((line, col));
    }

    let mut out = Vec::new();
    let mut seen: HashSet<(String, String, u32, u32)> = HashSet::new();
    for (rel, positions) in by_file {
        let abs = canon_root.join(&rel);
        let symbols = fetch_document_symbols(session, &abs)
            .await
            .unwrap_or_default();
        for (line, col) in positions {
            let container = find_container_name(&symbols, line, col);
            let key = (rel.clone(), container.clone(), line, col);
            if seen.insert(key) {
                out.push(RefSymbolHit {
                    file: rel.clone(),
                    line,
                    col,
                    container_name: container,
                });
            }
        }
    }
    Ok(out)
}

pub async fn find_referencing_code_snippets(
    session: &Arc<Session>,
    root: &Path,
    file: &str,
    line: u32,
    col: u32,
    context_lines: u32,
    max_results: usize,
) -> RefResult<Vec<RefSnippetHit>> {
    let canon_root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let abs_file = canon_root.join(file);
    let refs = fetch_references(session, &abs_file, line, col).await?;

    let mut cache: HashMap<String, String> = HashMap::new();
    let mut out = Vec::new();
    let mut seen: HashSet<(String, u32, u32)> = HashSet::new();
    for loc in refs {
        if out.len() >= max_results {
            break;
        }
        let abs = match uri_to_path(loc.uri.as_str()) {
            Some(p) => p,
            None => continue,
        };
        let rel = abs
            .strip_prefix(&canon_root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .replace('\\', "/");
        let (line, col) = (loc.range.start.line, loc.range.start.character);
        let key = (rel.clone(), line, col);
        if !seen.insert(key) {
            continue;
        }
        let content = match cache.get(&rel) {
            Some(c) => c.clone(),
            None => match tokio::fs::read_to_string(&abs).await {
                Ok(c) => {
                    cache.insert(rel.clone(), c.clone());
                    c
                }
                Err(_) => continue,
            },
        };
        let line_idx = line as usize;
        let lines: Vec<&str> = content.lines().collect();
        if line_idx >= lines.len() {
            continue;
        }
        let text = lines[line_idx].trim_end().to_string();
        let n = context_lines as usize;
        let lo = line_idx.saturating_sub(n);
        let hi = (line_idx + n + 1).min(lines.len());
        let snippet = lines[lo..hi].join("\n");
        out.push(RefSnippetHit {
            file: rel,
            line,
            col,
            text,
            snippet,
        });
    }
    Ok(out)
}
