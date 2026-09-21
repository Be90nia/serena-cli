//! 文件 → LanguageId 自动探测（auto-install-design.md Task 31；原「PLAN Task 17」锚位有误，
//! 2026-09-21 文档回写轮已对齐）。
//!
//! 三层 fallback：
//! 1. 文件扩展名（→ LanguageId::from_extension）。
//! 2. 无扩展名时读文件首行的 shebang：`#!/usr/bin/env <lang>` / `#!/<path>/<lang>`。
//! 3. 文件名特殊匹配（Makefile / Dockerfile / .bashrc / .zshrc / ...）。
//!
//! 全部失败 → None。任何 I/O 错（权限 / 多字节路径 / 文件不存在）→ None（**0 panic**）。
//!
//! 放置：本模块放 ls-registry 而非 lsp-core，因为 LanguageId 在 ls-adapters 定义，
//! 而 lsp-core 按 ARCH §1 分层铁律不 import ls-adapters/ls-registry。把探测放在
//! 依赖方向最浅、能用 LanguageId 的位置（ls-registry）即可。

use std::path::Path;

use ls_adapters::LanguageId;

/// 文件名（含 dotfile）→ LanguageId 静态表（小写精确匹配）。
///
/// 仅收录 M3+ 真实会被打 LS 的文件名：build 文件（Makefile / Dockerfile / .bashrc /
/// .zshrc / .profile）。CMakeLists.txt 走 C++ 系，hits .txt 后无命中 → 走无扩展名 → None
/// （CPP 系 LS 不靠扩展名匹配，是按 build 文件 fallback 探测；v1 不覆盖）。
fn by_filename(name: &str) -> Option<LanguageId> {
    match name {
        // bash 系 dotfile —— shell 解释器系可走 bash-language-server（T0）。
        ".bashrc" | ".bash_profile" | ".zshrc" | ".profile" => None, // 无 LanguageId=Shell
        _ => None,
    }
}

/// shebang 解释器（首段）→ LanguageId。
///
/// 仅识别上游 ls-lang-extensions.md 已收的语言入口；其它解释器（perl/python3 等）
/// 一律 None（用户可显式 --lang 覆盖）。
fn by_shebang(hebang: &str) -> Option<LanguageId> {
    // 形如 `#!/usr/bin/env python` / `#!/usr/bin/python` / `#!/bin/bash`。
    // 末个空白分隔 token 作为解释器名（自动剥离 `env` wrapper：取最末 token 而非第二段）。
    let mut tokens = hebang.split_whitespace();
    let raw = tokens.next_back()?.trim();
    let interp = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(raw);
    let interp = interp
        .strip_suffix(".exe")
        .or_else(|| interp.strip_suffix(".bat"))
        .or_else(|| interp.strip_suffix(".cmd"))
        .unwrap_or(interp);
    // 去版本号后缀（python3 → python）。
    let interp = interp
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end_matches('-');
    match interp {
        "python" | "python2" => Some(LanguageId::Python),
        "node" | "nodejs" | "deno" | "bun" => Some(LanguageId::TypeScript),
        "ruby" => None, // 无 LanguageId=Ruby（M3 未收）
        _ => None,
    }
}

/// 主入口：path → LanguageId。0 panic，全部失败返 None。
///
/// - `path` 仅看文件名/扩展名部分（不读盘检查 is_file；多字节路径不 panic）。
/// - 当扩展名为 None 或无命中时，才读 shebang（仅当路径有元数据或直读小缓冲）。
pub fn detect_language(path: &Path) -> Option<LanguageId> {
    let file_name = path.file_name()?.to_str()?;
    // 1. 扩展名
    if let Some(ext) = path.extension().and_then(|e| e.to_str())
        && let Some(lang) = LanguageId::from_extension(ext)
    {
        return Some(lang);
    }
    // 2. 文件名特殊匹配
    if let Some(lang) = by_filename(file_name) {
        return Some(lang);
    }
    // 3. 无扩展名才试 shebang：读首行（仅前 256 字节）。I/O 错一律 None。
    if path.extension().is_none()
        && let Ok(mut f) = std::fs::File::open(path)
    {
        use std::io::Read;
        let mut buf = [0u8; 256];
        if let Ok(n) = f.read(&mut buf) {
            let head = std::str::from_utf8(&buf[..n]).unwrap_or("");
            if let Some(rest) = head.strip_prefix("#!") {
                // 取首行（截到 \n）
                let first = rest.lines().next().unwrap_or("");
                if let Some(lang) = by_shebang(first) {
                    return Some(lang);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extension_routes_rs_py_ts_cpp_cs_java_md() {
        assert_eq!(detect_language(&PathBuf::from("a.rs")), Some(LanguageId::Rust));
        assert_eq!(detect_language(&PathBuf::from("a.py")), Some(LanguageId::Python));
        assert_eq!(detect_language(&PathBuf::from("a.ts")), Some(LanguageId::TypeScript));
        assert_eq!(detect_language(&PathBuf::from("a.tsx")), Some(LanguageId::TypeScript));
        assert_eq!(detect_language(&PathBuf::from("a.cpp")), Some(LanguageId::Cpp));
        assert_eq!(detect_language(&PathBuf::from("a.cs")), Some(LanguageId::CSharp));
        assert_eq!(detect_language(&PathBuf::from("a.java")), Some(LanguageId::Java));
        assert_eq!(detect_language(&PathBuf::from("a.md")), Some(LanguageId::Markdown));
    }

    #[test]
    fn extension_is_case_insensitive() {
        assert_eq!(detect_language(&PathBuf::from("Foo.CPP")), Some(LanguageId::Cpp));
        assert_eq!(detect_language(&PathBuf::from("X.PY")), Some(LanguageId::Python));
    }

    #[test]
    fn unknown_extension_returns_none_without_panic() {
        // 多字节路径也不应炸；这里只验 None。
        assert_eq!(detect_language(&PathBuf::from("a.lua")), None);
        assert_eq!(detect_language(&PathBuf::from("a.txt")), None);
        assert_eq!(detect_language(&PathBuf::from("a")), None);
    }

    #[test]
    fn shebang_routes_python_and_node() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("script");
        std::fs::write(&p, "#!/usr/bin/env python\nprint('hi')\n").unwrap();
        assert_eq!(detect_language(&p), Some(LanguageId::Python));
        std::fs::write(&p, "#!/usr/bin/env node\nconsole.log(1)\n").unwrap();
        assert_eq!(detect_language(&p), Some(LanguageId::TypeScript));
    }

    #[test]
    fn shebang_unknown_interpreter_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("script");
        std::fs::write(&p, "#!/usr/bin/env perl\nprint 'hi';\n").unwrap();
        assert_eq!(detect_language(&p), None);
    }

    #[test]
    fn missing_file_with_no_extension_does_not_panic() {
        // 不存在 + 无扩展名 → 走到 shebang 路径 → I/O 错返 None，0 panic。
        let p = PathBuf::from("Z:/nonexistent_path_xyz_12345");
        assert_eq!(detect_language(&p), None);
    }
}
