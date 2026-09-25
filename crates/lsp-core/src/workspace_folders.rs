//! LSP `initialize.workspaceFolders` 探测（Phase 4 基建 / Task 22a）。
//!
//! ↖ mirror: ls.py@43ae021 `_create_initialize_params.workspaceFolders` 单根；
//!   `language_servers/gopls_language_server.py` 的 `go.work` 多 module 模式
//!   是上游多 module 探测的唯一现成形态。Task 22a 抽象此模式到 lsp-core：
//!   探测 root 下的 monorepo marker → 产出 N 个 `WorkspaceFolder`，让 LS 在
//!   initialize 时一次性拿到全部 module（省后续 `workspace/didChangeWorkspaceFolders`）。
//!
//! 支持的 monorepo marker（non-goal: npm/yarn/pnpm 不在此范围）：
//! 1. **Cargo workspace**（`Cargo.toml` 含 `[workspace]` + `members = [...]`）——
//!    解析 `members` 数组（含 `path = "..."` / 通配 `crates/*`）。这是 Rust
//!    monorepo 最常见形态（项目自身即 7-crate workspace）。
//! 2. **Go workspace**（`go.work` + `use (...)`）—— 每行 `path/to/module` 一个。
//!    复用上游 gopls 适配器 go.work 解析的语义。
//! 3. **Git submodules**（`.gitmodules` + `[submodule "..."] path = ...`）——
//!    git submodule 视为独立 root（每个 submodule 是独立仓库）。
//!
//! 探测策略（per-marker 互不冲突）：
//! - 探测顺序：Cargo → Go → git submodule；任一命中**不立即 return**——同一项目
//!   可能同时是 cargo workspace + 嵌套 git submodule，全部收集。
//! - 路径解析相对 root，去重 + 过滤 root 自身（避免 root 也被列入 workspaceFolders）。
//! - 不存在的路径自动过滤（防御 git submodule 残留 .gitmodules 条目）。
//!
//! 错误处理：所有 IO 错误吞掉 → 返回空 Vec（单测覆盖；探针不应阻塞冷启动）。
//!
//! 用法（supervisor session_for）：
//! ```ignore
//! use lsp_core::workspace_folders::discover_additional_workspace_folders;
//! let folders = discover_additional_workspace_folders(&key.root);
//! params.workspace_folders = Some({
//!     let mut v = vec![primary_folder(&key.root)];
//!     v.extend(folders);
//!     v
//! });
//! ```
//!
//! `workspace/didChangeWorkspaceFolders` 通知：不实现（M0 单次 initialize 即终态）。
//! Task 22a acceptance 单测断言探测出 ≥2 个 folder 即可——didChange 是后续任务。

use std::path::{Path, PathBuf};
use std::str::FromStr;

use lsp_types::{Uri, WorkspaceFolder};
use tracing::trace;

/// 探测 root 下的 monorepo modules，返回**除 root 自身外**的额外 WorkspaceFolder。
/// root 自身由 supervisor 加到结果首部——本函数只看 modules。
pub fn discover_additional_workspace_folders(root: &Path) -> Vec<WorkspaceFolder> {
    let mut folders = Vec::new();

    // Cargo workspace（[workspace] members）
    folders.extend(discover_cargo_workspace(root));

    // Go workspace (go.work)
    folders.extend(discover_go_workspace(root));

    // Git submodules
    folders.extend(discover_git_submodules(root));

    // 去重（按 URI 字符串）+ 过滤 root 自身
    dedupe_and_filter_root(folders, root)
}

/// 构造 root 自身的 WorkspaceFolder（name = 末级目录名）。
pub fn primary_workspace_folder(root: &Path) -> WorkspaceFolder {
    let uri = path_to_file_uri(root);
    let name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("root")
        .to_string();
    WorkspaceFolder { uri, name }
}

// ---------------------------------------------------------------------------
// Cargo workspace
// ---------------------------------------------------------------------------

/// 探测 `Cargo.toml` 的 `[workspace] members = [...]`。
/// 含 `path = "..."` / 通配 `crates/*` / 普通相对路径。
fn discover_cargo_workspace(root: &Path) -> Vec<WorkspaceFolder> {
    let cargo_toml = root.join("Cargo.toml");
    if !cargo_toml.is_file() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&cargo_toml) else {
        trace!(path = %cargo_toml.display(), "Cargo.toml read failed; skip");
        return Vec::new();
    };
    let members = parse_cargo_workspace_members(&text);
    if members.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::new();
    for member_str in members {
        for resolved in expand_cargo_member(root, &member_str) {
            if let Some(folder) = module_folder(root, &resolved) {
                out.push(folder);
            }
        }
    }
    out
}

/// 手写最小解析器：定位 `[workspace]` 段后扫 `members = [ ... ]` 数组元素。
/// 不引入 toml 依赖（lsp-core 轻量层，ARCH §8 倾向少依赖）；本函数仅识别字符串
/// 元素（成员路径），忽略 inline table / 嵌套结构（这些在 monorepo member
/// 中极少出现）。
///
/// 已知边界：
/// - 多行数组 + 注释 + 引号字符串 — 全支持。
/// - 含转义或跨行字符串 — 不支持（用户 Cargo.toml 走多行数组本就不写转义）。
/// - `[workspace] members = []` 空数组 → 返空（无成员）。
fn parse_cargo_workspace_members(text: &str) -> Vec<String> {
    let mut in_workspace = false;
    let mut in_members_array = false;
    let mut members: Vec<String> = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        // 跳过注释 / 空行
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // 段切换
        if trimmed.starts_with('[') {
            in_workspace = trimmed == "[workspace]";
            in_members_array = false;
            continue;
        }
        if !in_workspace {
            continue;
        }
        // 找 `members = [` 起头
        if !in_members_array {
            if let Some(rest) = strip_array_key(trimmed, "members") {
                // `members = []` 空数组
                if rest == "[]" {
                    return members;
                }
                // 单行闭合 `["a", "b"]`（含 `]`）
                if rest.starts_with('[') && rest.contains(']') {
                    for s in extract_array_strings(rest) {
                        members.push(s);
                    }
                    return members;
                }
                // 跨行开始 `members = [` 后可能同行已有首个元素（少见）+ 换行
                in_members_array = true;
                let after_open = rest.trim_start_matches('[').trim();
                if after_open.starts_with(']') {
                    return members;
                }
                if let Some(s) = strip_quotes(after_open.trim_end_matches(',').trim()) {
                    members.push(s.to_string());
                }
                continue;
            }
        } else {
            // 跨行数组中
            if trimmed.starts_with(']') {
                return members;
            }
            if let Some(s) = strip_quotes(trimmed.trim_end_matches(',').trim()) {
                members.push(s.to_string());
            }
        }
    }
    members
}

fn strip_array_key<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let after = line.strip_prefix(key)?.trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    Some(after)
}

fn strip_quotes(s: &str) -> Option<&str> {
    s.strip_prefix('"')
        .and_then(|r| r.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')))
}

/// 从单行闭合数组 `["a", "b"]` 抽字符串元素。容忍尾逗号 + 空白。
fn extract_array_strings(s: &str) -> Vec<String> {
    let inner = s.trim().trim_start_matches('[').trim_end_matches(']');
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_str = false;
    let mut quote = '"';
    for c in inner.chars() {
        match c {
            '"' | '\'' if !in_str => {
                in_str = true;
                quote = c;
            }
            c if in_str && c == quote => {
                out.push(std::mem::take(&mut cur));
                in_str = false;
            }
            c if in_str => cur.push(c),
            _ => {}
        }
    }
    out
}

/// 展开 Cargo member glob（`*` 通配）。`crates/*` → 扫 `crates/` 一级子目录。
fn expand_cargo_member(root: &Path, member: &str) -> Vec<PathBuf> {
    if member.contains('*') {
        // 通配：取 `*` 前路径作为 base 目录，逐项扫
        let base = member.split('*').next().unwrap_or("");
        let base_path = root.join(base);
        let Ok(rd) = std::fs::read_dir(&base_path) else {
            return Vec::new();
        };
        rd.flatten()
            .filter(|e| e.file_type().as_ref().is_ok_and(|t| t.is_dir()))
            .map(|e| e.path())
            .filter(|p| p.join("Cargo.toml").is_file())
            .collect()
    } else {
        vec![root.join(member)]
    }
}

// ---------------------------------------------------------------------------
// Go workspace
// ---------------------------------------------------------------------------

/// 探测 `go.work` 的 `use (...)` block。
/// 格式（go.work v3+）：
/// ```text
/// go 1.22
///
/// use (
///     ./module-a
///     ./module-b
/// )
/// ```
/// 或单行 `use ./module-a`。
fn discover_go_workspace(root: &Path) -> Vec<WorkspaceFolder> {
    let go_work = root.join("go.work");
    if !go_work.is_file() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&go_work) else {
        return Vec::new();
    };
    let mut in_use_block = false;
    let mut out = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("use (") {
            in_use_block = true;
            // 单行 `use ./x` 紧跟开括号后的也属同一 use 块
            let rest = trimmed.trim_start_matches("use (").trim();
            if !rest.is_empty() {
                push_go_module(root, rest, &mut out);
            }
            continue;
        }
        if in_use_block {
            if trimmed == ")" {
                in_use_block = false;
                continue;
            }
            push_go_module(root, trimmed, &mut out);
        } else if let Some(rest) = trimmed.strip_prefix("use ") {
            push_go_module(root, rest.trim(), &mut out);
        }
    }
    out
}

fn push_go_module(root: &Path, raw: &str, out: &mut Vec<WorkspaceFolder>) {
    let path = raw.trim().trim_matches('"');
    if path.is_empty() {
        return;
    }
    let resolved = root.join(path);
    if let Some(folder) = module_folder(root, &resolved) {
        out.push(folder);
    }
}

// ---------------------------------------------------------------------------
// Git submodules
// ---------------------------------------------------------------------------

/// 探测 `.gitmodules` 的 `[submodule "x"] path = y`。
fn discover_git_submodules(root: &Path) -> Vec<WorkspaceFolder> {
    let gitmodules = root.join(".gitmodules");
    if !gitmodules.is_file() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&gitmodules) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut current_path: Option<String> = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("[submodule ") {
            // 切到下一个 submodule 前把上一次的 commit
            if let Some(p) = current_path.take()
                && let Some(folder) = module_folder(root, &root.join(&p))
            {
                out.push(folder);
            }
            // 解析 "name"] 后的 name
            let name = rest.trim_end_matches(']').trim().trim_matches('"');
            current_path = Some(name.to_string());
        } else if let Some(rest) = trimmed.strip_prefix("path") {
            // 严格匹配 "path" 后跟 `=`（防 `pathology=` 等误匹配）
            let after_key = rest.trim_start();
            let Some(after_eq) = after_key.strip_prefix('=') else {
                continue;
            };
            let path = after_eq.trim().trim_matches('"');
            // path 覆盖先前 `[submodule "x"]` 暂存的 name（name 默认等同 path，
            // 但 git 允许 name ≠ path，例如 `path = vendor/lib` 但 name = `lib`）
            current_path = Some(path.to_string());
        }
    }
    if let Some(p) = current_path
        && let Some(folder) = module_folder(root, &root.join(&p))
    {
        out.push(folder);
    }
    out
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// 把 module 绝对路径转成 WorkspaceFolder（若存在 + 在 root 下）。
fn module_folder(root: &Path, module_abs: &Path) -> Option<WorkspaceFolder> {
    let canonical = dunce::canonicalize(module_abs).ok()?;
    // 必须在 root 之下（防御 `..` 越界）
    if !canonical.starts_with(root) && canonical != root {
        return None;
    }
    if !canonical.is_dir() {
        return None;
    }
    let name = canonical
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("module")
        .to_string();
    Some(WorkspaceFolder {
        uri: path_to_file_uri(&canonical),
        name,
    })
}

/// 去重（按 URI 字符串）+ 过滤 root 自身。
fn dedupe_and_filter_root(mut folders: Vec<WorkspaceFolder>, root: &Path) -> Vec<WorkspaceFolder> {
    let root_uri = path_to_file_uri(root);
    folders.retain(|f| f.uri != root_uri);
    let mut seen = std::collections::HashSet::new();
    folders.retain(|f| seen.insert(f.uri.to_string()));
    folders
}

/// 本地路径 → `file://` URI（与 docsync::path_to_uri 语义对齐）。
fn path_to_file_uri(path: &Path) -> Uri {
    crate::docsync::path_to_uri(path).unwrap_or_else(|_| {
        // 兜底：canonicalize 失败时回退到 path_to_uri_str 直转。
        let raw = crate::docsync::path_to_uri_str(path);
        Uri::from_str(&raw).expect("path_to_uri_str returns valid file:// URI")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fixture helper：在 tempdir 建 monorepo 布局。
    fn write_cargo_workspace(dir: &Path, members: &[&str]) {
        let members_toml = members
            .iter()
            .map(|m| format!("    \"{m}\","))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"root\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                 [workspace]\nmembers = [\n{members_toml}\n]\n"
            ),
        )
        .unwrap();
        for m in members {
            let sub = dir.join(m);
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(
                sub.join("Cargo.toml"),
                "[package]\nname = \"m\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
            )
            .unwrap();
        }
    }

    fn write_go_work(dir: &Path, modules: &[&str]) {
        let body: String = modules
            .iter()
            .map(|m| format!("    ./{}", m))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(
            dir.join("go.work"),
            format!("go 1.22\n\nuse (\n{body}\n)\n"),
        )
        .unwrap();
        for m in modules {
            std::fs::create_dir_all(dir.join(m)).unwrap();
        }
    }

    fn write_gitmodules(dir: &Path, paths: &[(&str, &str)]) {
        let body: String = paths
            .iter()
            .map(|(name, path)| {
                format!(
                    "[submodule \"{name}\"]\n\tpath = {path}\n\turl = https://example.com/{name}.git\n"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join(".gitmodules"), body).unwrap();
        for (_name, path) in paths {
            std::fs::create_dir_all(dir.join(path)).unwrap();
        }
    }

    #[test]
    fn cargo_workspace_discovers_member_folders() {
        let tmp = tempfile::tempdir().unwrap();
        write_cargo_workspace(tmp.path(), &["crates/a", "crates/b"]);
        let folders = discover_additional_workspace_folders(tmp.path());
        assert_eq!(folders.len(), 2, "应探测 2 个 cargo workspace member");
        let names: std::collections::BTreeSet<_> =
            folders.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains("a"));
        assert!(names.contains("b"));
    }

    #[test]
    fn go_work_discovers_use_block_modules() {
        let tmp = tempfile::tempdir().unwrap();
        write_go_work(tmp.path(), &["mod-a", "mod-b"]);
        let folders = discover_additional_workspace_folders(tmp.path());
        assert_eq!(folders.len(), 2, "应探测 2 个 go.work use 模块");
        let names: std::collections::BTreeSet<_> =
            folders.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains("mod-a"));
        assert!(names.contains("mod-b"));
    }

    #[test]
    fn git_submodules_are_each_a_workspace_folder() {
        let tmp = tempfile::tempdir().unwrap();
        write_gitmodules(
            tmp.path(),
            &[("sub1", "vendor/sub1"), ("sub2", "vendor/sub2")],
        );
        let folders = discover_additional_workspace_folders(tmp.path());
        assert_eq!(folders.len(), 2, "应探测 2 个 git submodule");
        let names: std::collections::BTreeSet<_> =
            folders.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains("sub1"));
        assert!(names.contains("sub2"));
    }

    #[test]
    fn glob_cargo_member_wildcard() {
        let tmp = tempfile::tempdir().unwrap();
        // crates/* 通配（项目自身 7-crate workspace 即此形态）
        write_cargo_workspace(tmp.path(), &["crates/alpha", "crates/beta", "crates/gamma"]);
        // 改用 crates/* 形态
        let cargo = tmp.path().join("Cargo.toml");
        let mut text = std::fs::read_to_string(&cargo).unwrap();
        text = text.replace(
            "    \"crates/alpha\",\n    \"crates/beta\",\n    \"crates/gamma\",\n",
            "    \"crates/*\",\n",
        );
        std::fs::write(&cargo, text).unwrap();
        let folders = discover_additional_workspace_folders(tmp.path());
        assert_eq!(folders.len(), 3, "crates/* 通配应展开为 3 个");
        let names: std::collections::BTreeSet<_> =
            folders.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains("alpha") && names.contains("beta") && names.contains("gamma"));
    }

    #[test]
    fn no_marker_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let folders = discover_additional_workspace_folders(tmp.path());
        assert!(folders.is_empty(), "空 dir 应返空 Vec");
    }

    #[test]
    fn dedupe_filters_root_and_duplicates() {
        let tmp = tempfile::tempdir().unwrap();
        // Cargo workspace with a member pointing back at root (invalid)
        write_cargo_workspace(tmp.path(), &["crates/a"]);
        // 手工把 root 也加进 members（不合法但模拟 dup）
        let cargo = tmp.path().join("Cargo.toml");
        let mut text = std::fs::read_to_string(&cargo).unwrap();
        text = text.replace("    \"crates/a\",", "    \"crates/a\",\n    \".\",");
        std::fs::write(&cargo, text).unwrap();
        let folders = discover_additional_workspace_folders(tmp.path());
        // 仅 crates/a 一个有效，root 自己被过滤
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].name, "a");
    }

    #[test]
    fn primary_folder_uses_basename() {
        let tmp = tempfile::tempdir().unwrap();
        let f = primary_workspace_folder(tmp.path());
        // tmpdir basename 一般是 hex 字符串
        assert!(!f.name.is_empty());
        assert!(f.uri.as_str().starts_with("file:///"));
    }
}
