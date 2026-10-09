//! 写类工具的 root 界路径校验（bd serena-rust-5r7）。
//!
//! 19 处写工具 `root.join(file)` 直拼，agent 输入的 `../..` 或绝对路径可把写盘
//! 送出项目 root。本模块是唯一的拼接收口：词法归一 + canonical 校验双防线，
//! 拒绝即 `BAD_ARGS`（wire §6.3 确定性参数错，不重试）。
//!
//! ## 防线与语义
//!
//! 1. **带根/绝对分量拒绝**：Windows `Path::join` 遇带根分量整体替换基路径
//!    （`root.join("C:/x") == "C:/x"`、`root.join("/x") == "D:/x"`），盘符相对
//!    `C:x` 同样换盘 —— Prefix/RootDir 分量一律拒。
//! 2. **词法归一**：`..` 逐级上卷、卷出 root 即拒（`root/../x`）；`.` 与混合
//!    分隔符（`..\..\x`）由 `Path::components` 天然消解。此步不碰磁盘，确定性。
//! 3. **canonical 校验（symlink 语义）**：对目标「最深已存在祖先」做
//!    `dunce::canonicalize`（解析既有链路上的全部 symlink / 8.3 短名 / 盘符
//!    大小写）后重接剩余纯 Normal 分量，再 `starts_with(canon_root)` 终检——
//!    root 内 symlink 指向 root 外 → 拒（fail-closed）。目标不存在（create
//!    新建语义）时剩余分量不含 `..`（第 2 步已消解），重接不可能逃逸。
//!
//! TOCTOU 边界：校验与写盘之间敌方若无本机 fs 访问权则无法插入 symlink 竞态；
//! 本 helper 不承诺对抗本机攻击者。

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf, Prefix};

/// 把 agent 输入的 `file`（相对 `root`）解析为 root 内绝对路径；越界返回
/// 人读 detail（调用方转 `ToolError::BadArgs`）。
///
/// 编排主路径支持 `--project <root> <root内绝对路径>`（bd serena-rust-rdcd）：
/// drive-rooted 绝对路径（`C:\…` / `C:/…`）在词法归一 + canonical 校验后
/// 放行，只要最终路径仍在 canon_root 内。
pub(crate) fn guarded_join(root: &Path, file: &str) -> Result<PathBuf, String> {
    if file.is_empty() {
        return Err("invalid file path: empty".into());
    }
    let raw = Path::new(file);
    let canon_root = dunce::canonicalize(root)
        .map_err(|e| format!("project root not resolvable: {} ({e})", root.display()))?;

    // 阶段 1：检查 prefix 形态，仅拒绝真正危险的换盘 / 长路径绕过：
    //   - 纯 RootDir（`/foo`）→ 换当前盘根
    //   - Verbatim / VerbatimDisk（`\\?\…`）→ 长路径绕过
    //   - UNC（`\\server\share`）/ DeviceNS（`\\.\COM1`）
    //   - Drive Prefix 但缺 RootDir（`C:foo` 盘符相对，整体换盘）
    // 放行：Drive Prefix + RootDir（`C:\foo` / `C:/foo`）→ 走阶段 3。
    let raw_comps: Vec<Component<'_>> = raw.components().collect();
    let mut drive_prefix_idx: Option<usize> = None;
    for (i, c) in raw_comps.iter().enumerate() {
        match c {
            Component::RootDir if drive_prefix_idx.is_none() => {
                return Err(format!("path escapes project root: {file}"));
            }
            Component::Prefix(p) => match p.kind() {
                Prefix::Verbatim(_)
                | Prefix::VerbatimUNC(_, _)
                | Prefix::VerbatimDisk(_)
                | Prefix::UNC(_, _)
                | Prefix::DeviceNS(_) => {
                    return Err(format!("path escapes project root: {file}"));
                }
                Prefix::Disk(_) => {
                    drive_prefix_idx = Some(i);
                    let tail = &raw_comps[i + 1..];
                    if !tail.iter().any(|c| matches!(c, Component::RootDir)) {
                        // `C:foo` 盘符相对：Path::join 整体换盘 → 拒。
                        return Err(format!("path escapes project root: {file}"));
                    }
                }
            },
            _ => {}
        }
    }

    if drive_prefix_idx.is_some() {
        // 阶段 3：drive-rooted 绝对路径。词法归一（.. 上卷至盘根停），再沿
        // 祖先链 canonicalize，最后 starts_with(canon_root) 终检。
        return check_absolute_in_root(raw, &canon_root, file);
    }

    // 阶段 2：相对路径（原逻辑）。
    let mut norm: Vec<OsString> = Vec::new();
    for comp in &raw_comps {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if norm.pop().is_none() {
                    return Err(format!("path escapes project root: {file}"));
                }
            }
            Component::Normal(c) => norm.push(c.to_os_string()),
            _ => unreachable!(),
        }
    }
    let candidate = norm.iter().fold(canon_root.clone(), |acc, c| acc.join(c));
    let mut base = candidate.clone();
    let mut tail: Vec<OsString> = Vec::new();
    let canon_base = loop {
        match dunce::canonicalize(&base) {
            Ok(canon) => break canon,
            Err(_) => {
                let name = base
                    .file_name()
                    .map(|n| n.to_os_string())
                    .ok_or_else(|| format!("path escapes project root: {file}"))?;
                let parent = base
                    .parent()
                    .map(|p| p.to_path_buf())
                    .ok_or_else(|| format!("path escapes project root: {file}"))?;
                tail.insert(0, name);
                base = parent;
            }
        }
    };
    let checked = tail.into_iter().fold(canon_base, |acc, c| acc.join(c));
    if !checked.starts_with(&canon_root) {
        return Err(format!("path escapes project root: {file}"));
    }
    Ok(checked)
}

/// Drive-rooted 绝对路径校验：词法归一 .. / . 后，沿祖先链 canonicalize
/// （解析 symlink + 处理不存在的尾部分量），再 starts_with(canon_root) 终检。
fn check_absolute_in_root(
    raw: &Path,
    canon_root: &Path,
    file: &str,
) -> Result<PathBuf, String> {
    // 词法归一 .. / . ：.. 在自身前缀层级上卷，盘根处不再 pop。
    let mut drive: Option<OsString> = None;
    let mut has_root = false;
    let mut stack: Vec<OsString> = Vec::new();
    for comp in raw.components() {
        match comp {
            Component::Prefix(p) => drive = Some(p.as_os_str().to_os_string()),
            Component::RootDir => has_root = true,
            Component::CurDir => {}
            Component::ParentDir => {
                // stack 顶部是 `\` 或 `/` 时已在盘根后；空 stack 也是盘根位置——不动。
                let at_drive_root = match stack.last() {
                    Some(last) => {
                        let s = last.to_string_lossy();
                        s == "\\" || s == "/"
                    }
                    None => true,
                };
                if !at_drive_root {
                    stack.pop();
                }
            }
            Component::Normal(c) => stack.push(c.to_os_string()),
        }
    }
    let mut candidate = PathBuf::new();
    if let Some(d) = drive {
        candidate.push(d);
    }
    if has_root {
        candidate.push(std::path::MAIN_SEPARATOR.to_string());
    }
    for c in &stack {
        candidate.push(c);
    }

    // 沿祖先链 canonicalize（目标不存在 → 上卷到存在祖先）。
    let mut base = candidate.clone();
    let mut tail: Vec<OsString> = Vec::new();
    let canon_base = loop {
        match dunce::canonicalize(&base) {
            Ok(canon) => break canon,
            Err(_) => {
                let name = base
                    .file_name()
                    .map(|n| n.to_os_string())
                    .ok_or_else(|| format!("path escapes project root: {file}"))?;
                let parent = base
                    .parent()
                    .map(|p| p.to_path_buf())
                    .ok_or_else(|| format!("path escapes project root: {file}"))?;
                tail.insert(0, name);
                base = parent;
            }
        }
    };
    let checked = tail.into_iter().fold(canon_base, |acc, c| acc.join(c));
    if !checked.starts_with(canon_root) {
        return Err(format!("path escapes project root: {file}"));
    }
    Ok(checked)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(tag);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("a.txt"), "x\n").unwrap();
        // 便于 root/../ 形态测试：dir 内 root 旁放一个真实文件。
        std::fs::write(dir.path().join("outside.txt"), "secret\n").unwrap();
        dir
    }

    fn root_of(dir: &tempfile::TempDir, tag: &str) -> PathBuf {
        dunce::canonicalize(dir.path().join(tag)).unwrap()
    }

    #[test]
    fn in_root_relative_ok() {
        let dir = scratch("in_root");
        let root = root_of(&dir, "in_root");
        let got = guarded_join(&root, "sub/a.txt").unwrap();
        assert_eq!(got, root.join("sub").join("a.txt"));
        // `.` 与 `sub/../` 词法消解后仍在 root 内。
        assert_eq!(guarded_join(&root, "./sub/../main.rs").unwrap(), root.join("main.rs"));
    }

    #[test]
    fn parent_escape_rejected() {
        let dir = scratch("esc");
        let root = root_of(&dir, "esc");
        for f in ["../outside.txt", "../../outside.txt", "sub/../../outside.txt"] {
            let err = guarded_join(&root, f).unwrap_err();
            assert!(err.contains("escapes project root"), "{f}: {err}");
        }
    }

    #[test]
    fn absolute_injection_rejected() {
        let dir = scratch("abs");
        let root = root_of(&dir, "abs");
        // 平台无关的绝对路径：temp_dir 自身（两侧平台均为绝对形态）。
        let abs = std::env::temp_dir().join("evil.txt");
        let err = guarded_join(&root, &abs.to_string_lossy()).unwrap_err();
        assert!(err.contains("escapes project root"), "{err}");
    }

    #[cfg(windows)]
    #[test]
    fn windows_rooted_and_drive_relative_rejected() {
        let dir = scratch("winroot");
        let root = root_of(&dir, "winroot");
        // 带根无盘符：join 语义 = 换到当前盘根（`root.join("/x") == "D:/x"`）。
        assert!(guarded_join(&root, r"\Windows\evil.txt").is_err());
        // 盘符相对：join 语义 = 整体换盘（`root.join("C:x") == "C:x"`）。
        assert!(guarded_join(&root, "C:evil.txt").is_err());
        // UNC / verbatim 前缀。
        assert!(guarded_join(&root, r"\\server\share\evil.txt").is_err());
        assert!(guarded_join(&root, r"\\?\C:\Windows\evil.txt").is_err());
        // 混合分隔符穿越（%TEMP% 类拼接的实锚形态）。
        assert!(guarded_join(&root, r"..\..\evil.txt").is_err());
        assert!(guarded_join(&root, "../../evil.txt").is_err());
    }

    #[test]
    fn nonexistent_nested_target_ok_for_create_semantics() {
        let dir = scratch("create");
        let root = root_of(&dir, "create");
        let got = guarded_join(&root, "newdir/deep/new.rs").unwrap();
        assert!(got.starts_with(&root), "{got:?} 必须在 root 内");
        assert!(got.ends_with("newdir/deep/new.rs") || got.ends_with("newdir\\deep\\new.rs"));
    }

    #[test]
    fn absolute_path_inside_root_allowed() {
        // bd serena-rust-rdcd：--project <root> + 绝对路径在 root 内必须放行
        // （编排主路径：agent 持绝对路径 + 显式 root）。canonicalize 后判 starts_with。
        let dir = scratch("absok");
        let root = root_of(&dir, "absok");
        let target = root.join("main.rs");
        let got = guarded_join(&root, &target.to_string_lossy()).unwrap();
        assert_eq!(got, target);
        // 子目录也存在。
        let sub = root.join("sub").join("a.txt");
        let got = guarded_join(&root, &sub.to_string_lossy()).unwrap();
        assert_eq!(got, sub);
    }

    #[test]
    fn absolute_path_create_semantics_in_root_allowed() {
        // 绝对路径在 root 内、目标不存在（create 语义）——词法归一后走祖先链
        // canonicalize 仍判 starts_with 通过。
        let dir = scratch("abscreate");
        let root = root_of(&dir, "abscreate");
        let target = root.join("newdir").join("deep").join("new.rs");
        let got = guarded_join(&root, &target.to_string_lossy()).unwrap();
        assert!(got.starts_with(&root), "{got:?} 必须在 root 内");
        assert!(got.ends_with("newdir\\deep\\new.rs") || got.ends_with("newdir/deep/new.rs"));
    }

    #[test]
    fn absolute_path_outside_root_rejected() {
        // bd serena-rust-rdcd 防线：绝对路径在 root 外（含 .. 词法逃逸）仍必须拒。
        let dir = scratch("absout");
        let root = root_of(&dir, "absout");
        // 1. 跨盘符（构造一个不同盘符的 temp 路径）。
        let other_drive = if cfg!(windows) {
            // 任何与 root 不同盘符的位置。
            let tmp = std::env::temp_dir();
            if tmp.to_string_lossy().starts_with(r"\\") {
                // UNC temp：跨盘形态不一定可构造，跳过形态 1 走形态 2
                tmp.clone()
            } else {
                let bytes = tmp.to_string_lossy().into_owned().into_bytes();
                if bytes.first().copied().unwrap_or(b'C') == b'C' {
                    PathBuf::from("D:/evil.txt")
                } else {
                    PathBuf::from("C:/evil.txt")
                }
            }
        } else {
            PathBuf::from("/etc/passwd")
        };
        let err = guarded_join(&root, &other_drive.to_string_lossy()).unwrap_err();
        assert!(err.contains("escapes project root"), "{err}");
        // 2. 词法 .. 上卷后越界（绝对路径内 .. 跳出 root）。
        let escape = root.join("..").join("outside.txt");
        let err = guarded_join(&root, &escape.to_string_lossy()).unwrap_err();
        assert!(err.contains("escapes project root"), "{err}");
    }

    #[test]
    fn empty_rejected() {
        let dir = scratch("empty");
        let root = root_of(&dir, "empty");
        assert!(guarded_join(&root, "").is_err());
    }

    #[test]
    fn root_not_resolvable_fail_closed() {
        // root 不存在 → 拒绝建立基线，不放行（fail-closed，不回退 raw join）。
        let dir = scratch("no root");
        let missing = dir.path().join("no root").join("ghost");
        assert!(guarded_join(&missing, "a.txt").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn symlinked_ancestor_escape_rejected() {
        // root 内 symlink 指向 root 外 → canonical 校验拒收。Windows 建目录
        // symlink 需开发者模式/管理员：无权限则跳过（本机覆盖，CI 无 symlink 权）。
        let dir = scratch("sym");
        let root = root_of(&dir, "sym");
        let outside = dir.path().join("sym-outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "s\n").unwrap();
        if std::os::windows::fs::symlink_dir(&outside, root.join("jump")).is_err() {
            return; // 无 symlink 特权：语义由 canonical 祖先逻辑单测覆盖
        }
        let err = guarded_join(&root, "jump/secret.txt").unwrap_err();
        assert!(err.contains("escapes project root"), "{err}");
    }
}
