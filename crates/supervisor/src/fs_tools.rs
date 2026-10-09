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
pub(crate) fn filtered_walker(root: &Path) -> ignore::WalkBuilder {
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
    /// 客户端请求的 end_line（1-based，含）；max_tokens 截断后刷新成截断末行。
    pub end_line: u32,
    /// 全文 content-hash（sha256 前 16 位）—— 行级三件套 `expected_hash` 对账用。
    pub hash: String,
    /// 杠精 ke2a-6：content 经 lines() 归一为 \n（CRLF 被静默剥 \r）而 hash 按
    /// 原字节——据此字段判断拼接写回是否引入行尾转换。crlf | lf | mixed。
    pub line_endings: &'static str,
    /// 整文件字节数（与 content 串脱钩，永远填）。max_tokens 截断后 caller 凭
    /// `truncated:true + total_bytes` 判断损失比例。
    pub total_bytes: usize,
    /// 是否被 max_tokens 砍到（false = 全文返回）。仅 max_tokens 给定可能为 true。
    pub truncated: bool,
    /// end_line 是否被 clamp 到 EOF（critic4-F3）：clamp=true 且请求 end_line >
    /// total_lines 时 true —— caller 凭此知道请求的窗口被截短，而非文件真有
    /// 那么多行。mfxg/66al 的 clamp 行为本身不变，纯 additive 标记。
    pub clamped: bool,
    /// 整文件估算 token（4B/T，与 apply_budget 同口径）；max_tokens 未给 = None。
    pub total_tokens: Option<usize>,
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

/// bd serena-rust-p2zp：混合行尾切分（`\n` / 裸 `\r` / `\r\n` 都算终止符）。
///
/// `str::lines()` 只切 `\n`，对含裸 `\r`（Mac classic 行尾或 Windows 文件被某工具
/// 改写遗留）的混合文件会少计行数，导致 `total_lines` 与内容切片均失真。本函数：
/// - `\r\n` 整体计一个终止符（Windows 行为）；
/// - 单独的 `\n` 计一个终止符（Unix 行为）；
/// - 单独的 `\r`（紧邻非 `\n` 字符）也计一个终止符（Mac classic 与混合文件常见）；
/// - 末尾无终止符的尾段按"最后一行"补一条（与现有 `.lines()` 对 `"abc\n"` 返
///   `["abc"]` 的语义一致，但补 1 而非漏 1）。
///
/// 现有纯 LF / 纯 CRLF 文件路径输出与 `text.lines()` 完全相同（仅引入裸 `\r`
/// 与末尾无终止符两个新分支），存量测试不受影响。
fn split_lines_mixed(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(&text[start..i]);
                start = i + 1;
                i += 1;
            }
            b'\r' => {
                lines.push(&text[start..i]);
                // \r\n 整体计一个终止符；裸 \r 紧邻非 \n 也是单终止符
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    start = i + 2;
                    i += 2;
                } else {
                    start = i + 1;
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    if start < bytes.len() {
        // 末尾无终止符：保留尾段作为最后一行（与 .lines() 对 "abc" 返 ["abc"] 同源）
        lines.push(&text[start..]);
    }
    lines
}

/// 读 `root/file` 内容，可选 1-based 行切片 + max_tokens 截断（bd a14g）。
///
/// - `start_line=None, end_line=None`：全文件；
/// - `start_line=Some(s), end_line=None`：s..末；
/// - `start_line=None, end_line=Some(e)`：1..e；
/// - `start_line=Some(s), end_line=Some(e)`：s..e（含 e）；
/// - `max_tokens=Some(n)`：按 4B/T 估算 content 字节上限 = `n*4 − 32`（32 =
///   response metadata 留余），按整行切（留半行砍掉，split_lines_mixed 同源）；
///   超限置 `truncated:true` 并刷新 `end_line` 到截断末行；`max_tokens=Some(0)`
///   = BAD_ARGS（与 07u5 全局 `--max-tokens 0` 同走 rc=2）。
///
/// `clamp=true`（默认，bd mfxg）：`end_line` 超 EOF 自动收到末行（6 行文件传
/// 20 = 读到 EOF）；`clamp=false`（--no-clamp，bd 66al）保留严格越界 BAD_ARGS，
/// 供客户端探测文件真实长度。
pub async fn read_file(
    root: &Path,
    file: &str,
    start_line: Option<u32>,
    end_line: Option<u32>,
    clamp: bool,
    max_tokens: Option<usize>,
) -> FsResult<ReadReport> {
    if let Some(0) = max_tokens {
        return Err(FsError::BadArgs {
            detail: "max_tokens must be >= 1 (got 0)".into(),
        });
    }
    let canon_path = safe_join(root, file)?;
    let text = tokio::fs::read_to_string(&canon_path).await.map_err(|e| {
        // bd serena-rust-sgc0：二进制内容 InvalidData（"stream did not contain
        // valid UTF-8"）归 BAD_ARGS——文件不可读作文本是目标文件问题（exit 2），
        // 不是 Io → INTERNAL（exit 3 契约外）。
        if e.kind() == std::io::ErrorKind::InvalidData {
            FsError::BadArgs {
                detail: format!("{file}: not readable as UTF-8 text (binary content?): {e}"),
            }
        } else {
            FsError::Io(e)
        }
    })?;
    // bd serena-rust-p2zp：str::lines() 只切 \n，混合行尾（CRLF/LF/裸 CR）会少计：
    //   "line1\r\nline2\nline3\rline4\nline5\r\n".lines() = 4 段
    //   但编辑器视角是 5 行（line3/line4 用裸 \r 分隔）。改成同时识别 \n 与裸 \r
    //   作为行终止符（\r\n 整体计 1）——total_lines 与 content 切片都走同一规则，
    //   保持一致。
    let lines: Vec<&str> = split_lines_mixed(&text);
    let total = lines.len();
    let total_bytes = text.len();
    let s = start_line.unwrap_or(1);
    let raw_e = end_line.unwrap_or(total as u32);
    let mut e = if clamp {
        raw_e.min(total as u32)
    } else {
        raw_e
    };
    if s == 0 || e == 0 || s as usize > total || e as usize > total {
        return Err(FsError::BadArgs {
            detail: format!("line range {s}..{raw_e} out of bounds (total: {total})"),
        });
    }
    if s > e {
        return Err(FsError::BadArgs {
            detail: format!("invalid range {s}..{e} (start > end)"),
        });
    }
    // max_tokens 截断（bd a14g）：按整行累加到预算内，留半行砍掉（与
    // split_lines_mixed 同源）。预算 = n*4 − 32 字节；32 = response 其它字段
    // 的留余（coarse，不精确）。ponytail: 不做精确 BPE——soft limit 同 apply_budget。
    let slice = &lines[(s - 1) as usize..e as usize];
    let (content, truncated) = {
        let raw = slice.join("\n");
        match max_tokens {
            Some(n) => {
                let budget = n.saturating_mul(4).saturating_sub(32);
                if raw.len() > budget {
                    let mut bytes = 0usize;
                    let mut kept = 0usize;
                    for (i, line) in slice.iter().enumerate() {
                        // +1 给除首行外的 \n 分隔符（join 时插入）。
                        let cost = if i == 0 { line.len() } else { line.len() + 1 };
                        if bytes + cost > budget {
                            break;
                        }
                        bytes += cost;
                        kept = i + 1;
                    }
                    e = s + kept as u32 - 1;
                    (slice[..kept].join("\n"), true)
                } else {
                    (raw, false)
                }
            }
            None => (raw, false),
        }
    };
    let total_tokens = max_tokens.map(|_| total_bytes / 4);
    // critic4-F3：请求 end_line 超 EOF 且被 clamp 收短 → 显式标记（end_line=None
    // 时 raw_e=total 不会触发；clamp=false 走 BAD_ARGS 到不了这里）。
    let clamped = clamp && raw_e > total as u32;
    // 杠精 ke2a-6：CRLF 文件静默转 LF 的拼接写回防雷标记（\n 计数含 \r\n 内的）。
    let line_endings = {
        let crlf = text.matches("\r\n").count();
        let lf = text.matches('\n').count();
        if crlf == 0 {
            "lf"
        } else if crlf == lf {
            "crlf"
        } else {
            "mixed"
        }
    };
    Ok(ReadReport {
        content,
        total_lines: total,
        start_line: s,
        end_line: e,
        hash: crate::content_hash(&text),
        line_endings,
        total_bytes,
        truncated,
        clamped,
        total_tokens,
    })
}

/// bd serena-rust-sgc0：ensure_open 对二进制文件的 Core(Io InvalidData)（lsp-core
/// didOpen read_to_string 解码失败）归一为 BAD_ARGS——文件不可读作文本是目标
/// 文件问题（exit 2），不是内部故障（exit 3 契约外）。其余 CoreError 原样上抛。
pub(crate) fn ensure_open_err(
    file: &str,
) -> impl Fn(lsp_core::error::CoreError) -> crate::ToolError + '_ {
    move |e| match e {
        lsp_core::error::CoreError::Io(io) if io.kind() == std::io::ErrorKind::InvalidData => {
            crate::ToolError::BadArgs {
                detail: format!("{file}: not readable as UTF-8 text (binary content?)"),
            }
        }
        other => crate::ToolError::Core(other),
    }
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
/// - `name_pattern`：glob 风格（`*` `?` `[...]`）。**含通配符时按相对 root 的
///   路径匹配**（bd 75k3：`*` 不跨目录 → `*.rs` 只顶层，`**/*.rs` 才递归，
///   `src/*.rs` 限一层）；**纯文件名**（无 `*?[{:}`）保持任意深度文件名匹配。
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
    // bd 75k3：`*` 不得吞 `/`（shell 语义），`**` 才递归。
    const PATH_GLOB_OPTS: glob::MatchOptions = glob::MatchOptions {
        case_sensitive: true,
        require_literal_separator: true,
        require_literal_leading_dot: false,
    };
    let name_is_path_glob = name_pattern.contains(['*', '?', '[', '{']);
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
        let matched = if name_is_path_glob {
            pattern.matches_with(&rel, PATH_GLOB_OPTS)
        } else {
            pattern.matches(name)
        };
        if !matched {
            continue;
        }
        if path_filter.as_ref().is_some_and(|pf| !pf.matches(&rel)) {
            continue;
        }
        out.push(rel);
    }
    Ok(out)
}

/// 注释行判定：按文件扩展名查注释前缀表，匹配即注释。
///
/// ponytail: 不用 AST——粗滤够用，AST 让 LSP 做。命中=0 时返空数组；命中=1 也可能误判，
/// AI 看到命中行会自检。12+ 语言注释风格覆盖：// /* * # -- """ ''' <!-- % %% ; 。
pub fn looks_like_comment(file: &str, line_text: &str) -> bool {
    let trimmed = line_text.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    let ext = std::path::Path::new(file)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let prefixes: &[&str] = match ext {
        "rs" | "js" | "ts" | "jsx" | "tsx" | "go" | "java" | "cs" | "cpp" | "cc" | "cxx" | "c"
        | "h" | "hpp" | "swift" | "kt" | "scala" => &["//", "/*", "*"],
        "py" | "rb" | "sh" | "yaml" | "yml" | "toml" | "conf" => &["#"],
        "lua" => &["--"],
        "sql" => &["--", "/*"],
        "html" | "xml" | "vue" | "svelte" => &["<!--"],
        "tex" | "matlab" | "m" => &["%"],
        "lisp" | "clj" => &[";"],
        _ => &["//", "#", "--", "/*", "*", "<!--", "%"],
    };
    prefixes.iter().any(|p| trimmed.starts_with(p))
}

#[cfg(test)]
mod comment_tests {
    use super::looks_like_comment;

    #[test]
    fn looks_like_comment_recognizes_major_languages() {
        assert!(looks_like_comment("a.rs", "// todo: refactor"));
        assert!(looks_like_comment("a.rs", "/* block */"));
        assert!(looks_like_comment("a.rs", " * continued block"));
        assert!(looks_like_comment("a.py", "# comment"));
        assert!(looks_like_comment("a.lua", "-- comment"));
        assert!(looks_like_comment("a.html", "<!-- comment -->"));
        assert!(looks_like_comment("a.sql", "-- comment"));
        assert!(looks_like_comment("a.tex", "% note"));
        // 反例：代码行不算注释。
        assert!(!looks_like_comment("a.rs", "fn main() {}"));
        assert!(!looks_like_comment("a.py", "def foo():"));
        assert!(!looks_like_comment("a.lua", "local x = 1"));
        assert!(!looks_like_comment("a.html", "<div>TODO</div>"));
        assert!(!looks_like_comment("a.sql", "SELECT 1"));
        // 空行/未知扩展名 fallback。
        assert!(!looks_like_comment("a.rs", "   "));
        assert!(looks_like_comment("a.unknown", "# shebang-ish"));
    }
}

#[cfg(test)]
mod binary_read_tests {
    use super::*;

    /// bd serena-rust-sgc0：二进制内容 read_file → BAD_ARGS（非 Io → INTERNAL）。
    #[tokio::test]
    async fn read_file_binary_content_is_bad_args() {
        // serena-rust-nodd：tempfile 托管（Drop 即清，不再留 fs_bin_read_* 残留）。
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("bin.py"), vec![0xFFu8; 64]).expect("binary fixture");

        let err = read_file(dir.path(), "bin.py", None, None, true, None)
            .await
            .expect_err("binary content must fail");
        let FsError::BadArgs { detail } = err else {
            panic!("expect BadArgs, got {err:?}");
        };
        assert!(detail.contains("not readable as UTF-8 text"), "{detail}");

        // 同目录文本文件不受影响。
        std::fs::write(dir.path().join("good.py"), "x = 1\n").expect("text fixture");
        let ok = read_file(dir.path(), "good.py", None, None, true, None)
            .await
            .expect("read ok");
        assert_eq!(ok.content, "x = 1");
    }

    /// 杠精 ke2a-6：CRLF 文件 read_file 的 line_endings 标记（content 静默归一
    /// LF，但 hash 按原字节——标记让拼接写回方感知行尾转换）。
    #[tokio::test]
    async fn read_file_marks_line_endings() {
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("crlf.py"), "a = 1\r\nb = 2\r\n").expect("crlf fixture");
        std::fs::write(dir.path().join("lf.py"), "a = 1\nb = 2\n").expect("lf fixture");
        std::fs::write(dir.path().join("mixed.py"), "a = 1\r\nb = 2\n").expect("mixed fixture");
        let crlf = read_file(dir.path(), "crlf.py", None, None, true, None).await.expect("crlf");
        assert_eq!(crlf.line_endings, "crlf");
        assert_eq!(crlf.content, "a = 1\nb = 2", "content 保持 LF 归一（既有契约）");
        let lf = read_file(dir.path(), "lf.py", None, None, true, None).await.expect("lf");
        assert_eq!(lf.line_endings, "lf");
        let mixed = read_file(dir.path(), "mixed.py", None, None, true, None).await.expect("mixed");
        assert_eq!(mixed.line_endings, "mixed");
    }

    /// ensure_open_err 归一：InvalidData → BadArgs，其余 io → Core 原样。
    #[test]
    fn ensure_open_err_maps_only_invalid_data() {
        let binary = lsp_core::error::CoreError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        ));
        let err = ensure_open_err("bin.py")(binary);
        let crate::ToolError::BadArgs { detail } = err else {
            panic!("expect BadArgs, got {err:?}");
        };
        assert!(detail.contains("bin.py"), "{detail}");

        let missing = lsp_core::error::CoreError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such file",
        ));
        let err = ensure_open_err("gone.py")(missing);
        assert!(matches!(err, crate::ToolError::Core(_)), "{err:?}");
    }
}

/// bd serena-rust-p2zp：混合行尾（CRLF/LF/裸 CR）read_file 必须按行终止符
/// 计 total_lines 并对 content 切片对齐——`text.lines()` 只切 \n 会少计。
/// 端点 1：input = `line1\r\nline2\nline3\rline4\nline5\r\n`，5 行，content
/// 按 \n 拼接归一（既有 ke2a-6 LF 归一契约）；端点 2：纯 CRLF 行为不变；端点 3：
/// 末尾无终止符保留尾段；端点 4：纯 LF 行为不变；端点 5：空文件 = 0 行。
#[cfg(test)]
mod mixed_line_endings_tests {
    use super::*;
    #[tokio::test]
    async fn read_file_counts_mixed_endings_as_five_lines() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let path = dir.path().join("mixed.py");
        std::fs::write(&path, b"line1\r\nline2\nline3\rline4\nline5\r\n").expect("fixture");
        let r = read_file(dir.path(), "mixed.py", None, None, true, None)
            .await
            .expect("read ok");
        // 修复前: total_lines=4（lines() 只切 \n，少计裸 \r 分隔的 line3/line4）
        // 修复后: total_lines=5（line1/line2/line3/line4/line5 各占一行）
        assert_eq!(r.total_lines, 5, "裸 \\r 与 \\r\\n/\\n 同视为行终止符");
        assert_eq!(r.line_endings, "mixed");
        // content 切片按行号也对齐（lines() 同样语义升级）
        assert_eq!(r.content, "line1\nline2\nline3\nline4\nline5");
        // 单行切片 line3 单独取出（裸 \r 被剥，等价 .lines() 对 "line3\r" 的处理）
        let r3 = read_file(dir.path(), "mixed.py", Some(3), Some(3), true, None)
            .await
            .expect("slice ok");
        assert_eq!(r3.content, "line3");
        let r5 = read_file(dir.path(), "mixed.py", Some(5), Some(5), true, None)
            .await
            .expect("slice ok");
        assert_eq!(r5.content, "line5");
    }

    #[tokio::test]
    async fn read_file_pure_crlf_unchanged() {
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("crlf.py"), "a = 1\r\nb = 2\r\n").expect("fixture");
        let r = read_file(dir.path(), "crlf.py", None, None, true, None)
            .await
            .expect("read ok");
        assert_eq!(r.total_lines, 2);
        assert_eq!(r.line_endings, "crlf");
        assert_eq!(r.content, "a = 1\nb = 2");
    }

    #[tokio::test]
    async fn read_file_pure_lf_unchanged() {
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("lf.py"), "a = 1\nb = 2\n").expect("fixture");
        let r = read_file(dir.path(), "lf.py", None, None, true, None)
            .await
            .expect("read ok");
        assert_eq!(r.total_lines, 2);
        assert_eq!(r.line_endings, "lf");
        assert_eq!(r.content, "a = 1\nb = 2");
    }

    #[tokio::test]
    async fn read_file_no_trailing_newline_keeps_tail() {
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("notail.py"), "a\nb").expect("fixture");
        let r = read_file(dir.path(), "notail.py", None, None, true, None)
            .await
            .expect("read ok");
        assert_eq!(r.total_lines, 2, "末尾无终止符 = 仍有 2 行");
        assert_eq!(r.content, "a\nb");
    }

    #[test]
    fn split_lines_mixed_unit_table() {
        // 直接验证 split_lines_mixed 的纯函数契约——所有典型边界全表。
        assert_eq!(split_lines_mixed(""), Vec::<&str>::new());
        assert_eq!(split_lines_mixed("\n"), vec![""]);
        assert_eq!(split_lines_mixed("\r\n"), vec![""]);
        assert_eq!(split_lines_mixed("a"), vec!["a"]);
        assert_eq!(split_lines_mixed("a\n"), vec!["a"]);
        assert_eq!(split_lines_mixed("a\nb"), vec!["a", "b"]);
        assert_eq!(split_lines_mixed("a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines_mixed("a\r\nb"), vec!["a", "b"]);
        assert_eq!(split_lines_mixed("a\rb"), vec!["a", "b"], "裸 \\r 终止");
        assert_eq!(
            split_lines_mixed("line1\r\nline2\nline3\rline4\nline5\r\n"),
            vec!["line1", "line2", "line3", "line4", "line5"],
            "混合行尾 = 5 行"
        );
    }
}

// bd serena-rust-a14g：max_tokens 截断 read_file content。
//
// - None = 现行为不变（无 truncated/total_bytes/total_tokens 改字节）；
//   但 total_bytes 总是填，total_tokens 仍 None。
// - Some(0) = BAD_ARGS rc=2（与 07u5 路径同形 —— 禁静默吐空）；
// - Some(n)：content 超 n*4-32 字节按整行砍、刷新 end_line、写 truncated:true
//   + total_bytes 总文件字节 + total_tokens=总文件字节/4；
// - hash 始终按文件全文算（写门 `expected_hash` 契约不受截断影响）。
#[cfg(test)]
mod max_tokens_tests {
    use super::*;

    /// max_tokens=None 不截——回归 + 验证 total_bytes 字段始终填、total_tokens=None。
    #[tokio::test]
    async fn read_file_max_tokens_none_does_not_truncate() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let body = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(dir.path().join("a.txt"), &body).expect("fixture");
        let r = read_file(dir.path(), "a.txt", None, None, true, None)
            .await
            .expect("read ok");
        assert!(!r.truncated, "None → 不截");
        // content 串按 split_lines_mixed 归一（无尾 \n，与既有契约一致）。
        let expected_content = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(r.content, expected_content);
        assert_eq!(r.end_line, 10);
        assert_eq!(r.total_lines, 10);
        assert_eq!(r.total_bytes, body.len(), "total_bytes 永远填");
        assert!(
            r.total_tokens.is_none(),
            "max_tokens 未给 = total_tokens None"
        );
    }

    /// max_tokens=0 → BadArgs（与 07u5 全局 `--max-tokens 0` rc=2 同形）。
    #[tokio::test]
    async fn read_file_max_tokens_zero_is_bad_args() {
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("a.txt"), "x = 1\n").expect("fixture");
        let err = read_file(dir.path(), "a.txt", None, None, true, Some(0))
            .await
            .expect_err("max_tokens=0 → BadArgs");
        let FsError::BadArgs { detail } = err else {
            panic!("expect BadArgs, got {err:?}");
        };
        assert!(detail.contains("max_tokens must be >= 1"), "{detail}");
    }

    /// 200 行小文件 max_tokens=50：按整行砍，truncated:true + total_bytes=整文件。
    ///   预算 = 50*4-32 = 168 字节；
    ///   `line{i:03}` 8 字符 + \n = 9 字节/行（首行 8 字节）。
    ///   8 行 8 + 7*9 = 71 字节 ≤ 168；19 行 8 + 18*9 = 170 > 168。
    ///   留半行砍 = 18 行 167 字节（最后候选行被放弃）→ 实际尽量精确边界卡死。
    #[tokio::test]
    async fn read_file_max_tokens_truncates_large_file_to_budget_lines() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let body: String = (1..=200)
            .map(|i| format!("line{i:03}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(dir.path().join("big.txt"), &body).expect("fixture");
        let r = read_file(dir.path(), "big.txt", None, None, true, Some(50))
            .await
            .expect("read ok");
        assert!(r.truncated, "200 行小文件 max_tokens=50 必须截");
        assert_eq!(r.total_bytes, body.len(), "total_bytes 永远 = 整文件");
        assert_eq!(r.total_lines, 200, "total_lines 永远 = 整文件行数");
        let got_lines = r.content.split('\n').filter(|s| !s.is_empty()).count();
        assert!(got_lines < 200, "截后行数 < 全量: {got_lines}");
        assert_eq!(
            r.end_line as usize,
            got_lines,
            "end_line = 截后末行（契约：content 行数 = end_line - start_line + 1）"
        );
        assert!(r.total_tokens.is_some(), "max_tokens 给 = total_tokens Some");
    }

    /// 单大行 max_tokens=1：整行超预算被整行丢（留半行砍），content 串=""
    ///   truncated:true + total_bytes=整文件。验证单行文件不被「半行」截走。
    #[tokio::test]
    async fn read_file_max_tokens_truncates_single_huge_line_to_empty() {
        let dir = tempfile::tempdir().expect("tmpdir");
        // 单行很长，无 \n —— 任何 max_tokens < len(line)/4 必被整行砍。
        let big = "X".repeat(10 * 1024);
        std::fs::write(dir.path().join("oneline.bin"), &big).expect("fixture");
        let r = read_file(dir.path(), "oneline.bin", None, None, true, Some(1))
            .await
            .expect("read ok");
        assert!(r.truncated);
        assert_eq!(r.total_bytes, 10 * 1024);
        assert_eq!(r.total_lines, 1, "单行无 \n = total_lines=1");
        assert_eq!(r.end_line, 0, "整行被丢 → end_line = start_line - 1 = 0");
        assert!(
            r.content.is_empty(),
            "首行整行超预算被砍 → content=\"\": got {} bytes",
            r.content.len()
        );
        assert_eq!(r.total_tokens, Some(10 * 1024 / 4), "估算 = 字节/4");
    }

    /// 截断后契约自洽：content 行数 = end_line − start_line + 1；hash 仍按整
    /// 文件算（行级三件套 `expected_hash` 不受 max_tokens 影响）。
    #[tokio::test]
    async fn read_file_max_tokens_keeps_hash_and_line_invariant() {
        let dir = tempfile::tempdir().expect("tmpdir");
        let full: String = (1..=100)
            .map(|i| format!("line{i:03}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(dir.path().join("f.txt"), &full).expect("fixture");
        let unrestricted =
            crate::content_hash(&full);
        let r = read_file(dir.path(), "f.txt", Some(1), Some(50), true, Some(80))
            .await
            .expect("read ok");
        assert!(r.truncated);
        let got_lines = r.content.split('\n').filter(|s| !s.is_empty()).count();
        assert_eq!(
            r.end_line as usize - r.start_line as usize + 1,
            got_lines,
            "行号契约：end_line - start_line + 1 = content 行数"
        );
        assert_eq!(
            r.hash, unrestricted,
            "hash 仍按整文件算（写门 expected_hash 契约保持）"
        );
        // start_line 不动（用户显式传 1）。
        assert_eq!(r.start_line, 1);
    }

    /// critic4-F3：clamp=true 且请求 end_line 超 EOF → 收到末行 + `clamped:true`
    /// 显式标记（mfxg 的 clamp 行为本身不变，纯 additive 元数据）。
    #[tokio::test]
    async fn read_file_end_line_past_eof_sets_clamped_flag() {
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("f.txt"), "l1\nl2\nl3\n").expect("fixture");
        let r = read_file(dir.path(), "f.txt", Some(1), Some(99), true, None)
            .await
            .expect("read ok");
        assert!(r.clamped, "end_line 99 > total 3 必须标记 clamped");
        assert_eq!(r.end_line, 3, "clamp 行为不变：收到末行");
        assert_eq!(r.total_lines, 3);
        assert_eq!(r.content, "l1\nl2\nl3");
        assert!(!r.truncated, "clamped ≠ max_tokens 截断，两旗互不相干");
    }

    /// critic4-F3：窗口在 EOF 内 / 未指定 end_line → `clamped:false`（无截，
    /// 客户端可凭 false 判「拿到请求的完整窗口」）。
    #[tokio::test]
    async fn read_file_clamped_false_when_window_within_file() {
        let dir = tempfile::tempdir().expect("tmpdir");
        std::fs::write(dir.path().join("f.txt"), "l1\nl2\nl3\n").expect("fixture");
        let r = read_file(dir.path(), "f.txt", Some(1), Some(2), true, None)
            .await
            .expect("read ok");
        assert!(!r.clamped, "窗口内未截");
        assert_eq!(r.content, "l1\nl2");
        let r = read_file(dir.path(), "f.txt", None, None, true, None)
            .await
            .expect("read ok");
        assert!(!r.clamped, "未指定 end_line = 全文，无 clamp 发生");
    }
}
