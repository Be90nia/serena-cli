//! Task 23: 纯 fs 工具集（read_file / list_dir / find_file）。
//!
//! 设计要点（ARCH §6）：
//! - **不走 LSP**——读盘开销 << LSP RPC；不走 write_gate（只读）；
//! - 路径必须在 `root` 下（防 path traversal）：canonicalize 后 starts_with 校验；
//! - `list_dir` / `find_file` 用 `ignore` crate 自动尊重 `.gitignore` / `.ignore`；
//! - 排除 binary / >5MB 大文件（与 `tool_search_for_pattern` 启发一致）。

use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("bad args: {detail}")]
    BadArgs { detail: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("glob: {detail}")]
    Glob {
        detail: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

pub type FsResult<T> = std::result::Result<T, FsError>;

/// 目录扫描内置 ignore 列表（Phase 3.3）。表驱动，不读 .gitignore 协议。
pub fn should_ignore(name: &str) -> bool {
    matches!(
        name,
        "node_modules"
            | "target"
            | "dist"
            | ".git"
            | ".idea"
            | ".vscode"
            | "__pycache__"
            | "venv"
            | ".venv"
            | "build"
            | "out"
            | "coverage"
            | ".pytest_cache"
            | ".mypy_cache"
            | ".tox"
            | ".gradle"
            | ".terraform"
            | ".next"
            | ".nuxt"
    )
}

/// 构造带内置 ignore 过滤的 walker；depth 0（扫描根自身）不过滤，
/// 以便显式列 `dist/` 等仍可行。
fn filtered_walker(root: &Path) -> ignore::WalkBuilder {
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .standard_filters(true)
        .skip_stdout(true)
        .max_filesize(Some(5 * 1024 * 1024))
        .filter_entry(|e| e.depth() == 0 || !should_ignore(e.file_name().to_str().unwrap_or("")));
    walker
}

/// `read_file` 返回的结果：内容 + 总行数（用于客户端分页显示）。
#[derive(Debug, Serialize)]
pub struct ReadReport {
    pub content: String,
    pub total_lines: usize,
    /// 客户端请求的 start_line（1-based，未指定 = 1）。
    pub start_line: u32,
    /// 客户端请求的 end_line（1-based，含）。
    pub end_line: u32,
    /// 全文 content-hash（sha256 前 16 位）—— 行级三件套 `expected_hash` 对账用。
    pub hash: String,
}

/// `list_dir` 单条结果。
#[derive(Debug, Serialize)]
pub struct DirEntry {
    /// 相对 root 路径。
    pub path: String,
    /// 是否目录。
    pub is_dir: bool,
    /// 文件字节数（目录为 0）。
    pub size: u64,
}

/// 把 `root` 下的 `file` 路径规范化并校验在 root 内。
fn safe_join(root: &Path, sub: &str) -> FsResult<PathBuf> {
    let canon_root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let target = canon_root.join(sub);
    let canon_target = dunce::canonicalize(&target)?;
    if !canon_target.starts_with(&canon_root) {
        return Err(FsError::BadArgs {
            detail: format!("path escapes root: {sub}"),
        });
    }
    Ok(canon_target)
}

/// 读 `root/file` 内容，可选 1-based 行切片。
///
/// - `start_line=None, end_line=None`：全文件；
/// - `start_line=Some(s), end_line=None`：s..末；
/// - `start_line=None, end_line=Some(e)`：1..e；
/// - `start_line=Some(s), end_line=Some(e)`：s..e（含 e）。
pub async fn read_file(
    root: &Path,
    file: &str,
    start_line: Option<u32>,
    end_line: Option<u32>,
) -> FsResult<ReadReport> {
    let canon_path = safe_join(root, file)?;
    let text = tokio::fs::read_to_string(&canon_path).await?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let s = start_line.unwrap_or(1);
    let e = end_line.unwrap_or(total as u32);
    if s == 0 || e == 0 || s as usize > total || e as usize > total {
        return Err(FsError::BadArgs {
            detail: format!("line range {s}..{e} out of bounds (total: {total})"),
        });
    }
    if s > e {
        return Err(FsError::BadArgs {
            detail: format!("invalid range {s}..{e} (start > end)"),
        });
    }
    let content = lines[(s - 1) as usize..e as usize].join("\n");
    Ok(ReadReport {
        content,
        total_lines: total,
        start_line: s,
        end_line: e,
        hash: crate::content_hash(&text),
    })
}

/// 列 `root/path` 下的目录/文件。
///
/// - `max_depth`：None = 无限；Some(n) = 限制递归层数；
/// - `max_entries`：默认 500（防超大目录爆栈）。
pub fn list_dir(
    root: &Path,
    path: &str,
    max_depth: Option<usize>,
    max_entries: usize,
) -> FsResult<Vec<DirEntry>> {
    let canon_root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let target = canon_root.join(path);
    let canon_target = dunce::canonicalize(&target)?;
    if !canon_target.starts_with(&canon_root) {
        return Err(FsError::BadArgs {
            detail: format!("path escapes root: {path}"),
        });
    }
    let mut out = Vec::new();
    let mut walker = filtered_walker(&canon_target);
    if let Some(d) = max_depth {
        walker.max_depth(Some(d));
    }
    let target_rel = path.trim_end_matches('/').to_string();
    for entry in walker.build().flatten() {
        if out.len() >= max_entries {
            break;
        }
        let meta = entry.metadata().ok();
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let abs = entry.path();
        let rel = abs
            .strip_prefix(&canon_root)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/");
        // 跳过 target 自身（第一项 = target 目录本身）。
        if rel == target_rel || rel.is_empty() {
            continue;
        }
        out.push(DirEntry {
            path: rel,
            is_dir,
            size,
        });
    }
    Ok(out)
}

/// 按文件名 glob 跨目录找文件。
///
/// - `name_pattern`：glob 风格（`*` `?` `[...]`），匹配文件名（非相对路径）；
/// - `path_glob`：可选，匹配相对 root 的文件路径；
/// - `max_results`：默认 200。
pub fn find_file(
    root: &Path,
    name_pattern: &str,
    path_glob: Option<&str>,
    max_results: usize,
) -> FsResult<Vec<String>> {
    let canon_root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let pattern = glob::Pattern::new(name_pattern).map_err(|e| FsError::Glob {
        detail: format!("invalid name pattern `{name_pattern}`"),
        source: e.into(),
    })?;
    let path_filter = match path_glob {
        Some(g) => Some(glob::Pattern::new(g).map_err(|e| FsError::Glob {
            detail: format!("invalid path_glob `{g}`"),
            source: e.into(),
        })?),
        None => None,
    };
    let mut out = Vec::new();
    let walker = filtered_walker(&canon_root);
    for entry in walker.build().flatten() {
        if out.len() >= max_results {
            break;
        }
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let abs = entry.path();
        let rel = abs
            .strip_prefix(&canon_root)
            .unwrap_or(abs)
            .to_string_lossy()
            .replace('\\', "/");
        let name = abs.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if !pattern.matches(name) {
            continue;
        }
        if path_filter.as_ref().is_some_and(|pf| !pf.matches(&rel)) {
            continue;
        }
        out.push(rel);
    }
    Ok(out)
}
