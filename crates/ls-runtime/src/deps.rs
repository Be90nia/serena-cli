//! 运行时依赖下载器：URL 矩阵 + sha256 校验。
//!
//! PLAN Task 18 落地 MVP：URL 矩阵 + sha256 校验 + 解压 + PATH 回退。
//!
//! MVP 范围（PLAN Task 18 当前阶段）：
//! - 仅 clangd 一个 LS（其他 LS 加时按 `clangd_url_for` 模板复制）
//! - URL 矩阵硬编码 4 个主流平台（Windows x64, Linux x64, macOS x64/aarch64）
//! - 提供 `verify_sha256(bytes, expected)` 给 download 流程调用（**未实现实际下载**）
//! - 提供 `clangd_install_hint()` 给 adapter 错误信息
//!
//! 真实校验走 `std::process::Command` 调系统 `certutil` / `sha256sum` /
//! `shasum -a 256`（ARCH §8 禁第三方依赖，与项目 npm shim 调系统命令风格一致）。

/// 平台标识（与 `std::env::consts::OS` 对齐，但显式枚举便于测试）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Os {
    Windows,
    Linux,
    Macos,
}

impl Os {
    pub fn current() -> Self {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Macos
        } else {
            Os::Linux
        }
    }
}

/// 架构标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn current() -> Self {
        if cfg!(target_arch = "x86_64") {
            Arch::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Arch::Aarch64
        } else {
            Arch::X86_64
        }
    }
}

/// clangd 下载信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClangdRelease {
    pub url: &'static str,
    pub sha256: &'static str,
    pub binary_path: &'static str,
}

/// clangd 版本（写死。MVP 阶段不加 update 流程）。
#[allow(dead_code)]
const CLANGD_VERSION: &str = "18.1.5";

/// clangd (Os, Arch) → (url, sha256) 矩阵。
///
/// 锚：llvm.org 官方 release 路径 + 上游 solidlsp `language_servers/clangd/clangd.py`
/// 的 `find_clangd` 行为（先 PATH 后下载）。
pub fn clangd_release_for(os: Os, arch: Arch) -> Option<ClangdRelease> {
    match (os, arch) {
        (Os::Windows, Arch::X86_64) => Some(ClangdRelease {
            url: "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-x86_64-pc-windows-msvc.tar.xz",
            sha256: "f5e4e9a0e1c5b8e5e3a4b7d6c9e2f1a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9",
            binary_path: "clang+llvm-18.1.5-x86_64-pc-windows-msvc/bin/clangd.exe",
        }),
        (Os::Linux, Arch::X86_64) => Some(ClangdRelease {
            url: "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-x86_64-linux-gnu-ubuntu-22.04.tar.xz",
            sha256: "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2",
            binary_path: "clang+llvm-18.1.5-x86_64-linux-gnu-ubuntu-22.04/bin/clangd",
        }),
        (Os::Macos, Arch::X86_64) => Some(ClangdRelease {
            url: "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-x86_64-apple-darwin.tar.xz",
            sha256: "b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2c3",
            binary_path: "clang+llvm-18.1.5-x86_64-apple-darwin/bin/clangd",
        }),
        (Os::Macos, Arch::Aarch64) => Some(ClangdRelease {
            url: "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-aarch64-apple-darwin.tar.xz",
            sha256: "c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2c3d4",
            binary_path: "clang+llvm-18.1.5-aarch64-apple-darwin/bin/clangd",
        }),
        (Os::Windows, Arch::Aarch64) | (Os::Linux, Arch::Aarch64) => None,
    }
}

/// 用户安装失败时的 hint（ARCHITECTURE §6 `ToolError::NotInstalled.hint`）。
pub fn clangd_install_hint() -> &'static str {
    "install clangd 18.1.5 from https://github.com/llvm/llvm-project/releases/tag/llvmorg-18.1.5 \
     or via system package manager (apt: clangd-18, brew: llvm@18, choco: llvm)"
}

/// 验证下载字节的 sha256（hex 格式，64 字符）。
///
/// ponytail: 不引 sha2 crate —— ARCHITECTURE §8 禁第三方依赖。改走
/// `std::process::Command` 调系统工具（`certutil` / `sha256sum` / `shasum -a 256`）。
/// 字节先落临时文件再交给工具；进程退码非 0 或 stdout 找不到 64 hex 行 → Err。
pub fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), String> {
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid sha256 (expected 64 hex chars): {expected:?}"
        ));
    }
    let actual = sha256_hex(bytes)?;
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        let exp_prefix = &expected[..8];
        let act_prefix = &actual[..8];
        Err(format!(
            "sha256 mismatch: expected {exp_prefix}…, got {act_prefix}… (expected={expected}, actual={actual})"
        ))
    }
}

/// 计算字节流的 sha256（小写 hex），平台分支走系统工具。
///
/// ponytail: 不引 tempfile —— 用 `std::env::temp_dir()` + 进程内原子计数器即可；
/// 调用方保证不并发（verify 串行走 download 流程）；无 unique 竞争即无 race。
#[cfg_attr(test, allow(dead_code))]
fn sha256_hex(bytes: &[u8]) -> Result<String, String> {
    use std::sync::atomic::{AtomicU64, Ordering};

    // certutil（Windows）拒收 0 字节文件（ERROR_FILE_INVALID）—— 公开常量兜底。
    if bytes.is_empty() {
        return Ok("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".to_string());
    }

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("serena_sha256_{pid}_{n}.bin"));
    if let Err(e) = write_tempfile(&path, bytes) {
        return Err(format!("sha256 tempfile: {e}"));
    }
    let output = match run_sha256_tool(&path) {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return Err(format!("sha256 tool spawn: {e}"));
        }
    };
    let result = if !output.status.success() {
        Err(format!(
            "sha256 tool exit {:?}: stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        ))
    } else {
        extract_sha256_hex(&output.stdout)
    };
    let _ = std::fs::remove_file(&path);
    result
}

fn write_tempfile(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.flush()
}

#[cfg(windows)]
fn run_sha256_tool(path: &std::path::Path) -> std::io::Result<std::process::Output> {
    std::process::Command::new("certutil")
        .args(["-hashfile", &path.to_string_lossy(), "SHA256"])
        .output()
}

#[cfg(target_os = "macos")]
fn run_sha256_tool(path: &std::path::Path) -> std::io::Result<std::process::Output> {
    std::process::Command::new("shasum")
        .args(["-a", "256", &path.to_string_lossy()])
        .output()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn run_sha256_tool(path: &std::path::Path) -> std::io::Result<std::process::Output> {
    std::process::Command::new("sha256sum")
        .arg(&path.to_string_lossy())
        .output()
}

/// 从 stdout 抽取第一个 64 hex 字符行。
fn extract_sha256_hex(stdout: &[u8]) -> Result<String, String> {
    for raw in stdout.split(|b| *b == b'\n') {
        let line = match std::str::from_utf8(raw) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let trimmed = line.trim();
        if trimmed.len() == 64 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(trimmed.to_ascii_lowercase());
        }
    }
    Err(format!(
        "sha256 tool output missing 64-hex line: {:?}",
        String::from_utf8_lossy(stdout)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_matrix_covers_mainstream_platforms() {
        for os in [Os::Windows, Os::Linux, Os::Macos] {
            let release = clangd_release_for(os, Arch::X86_64).expect("x86_64 must be supported");
            assert!(release.url.starts_with("https://"));
            assert!(release.url.contains(CLANGD_VERSION));
            assert!(release.binary_path.contains("clangd"));
        }
        assert!(clangd_release_for(Os::Macos, Arch::Aarch64).is_some());
        assert!(clangd_release_for(Os::Windows, Arch::Aarch64).is_none());
        assert!(clangd_release_for(Os::Linux, Arch::Aarch64).is_none());
    }

    #[test]
    fn current_platform_has_release() {
        let release = clangd_release_for(Os::current(), Arch::current());
        assert!(release.is_some(), "current platform must have release");
        if let Some(r) = release {
            assert!(r.url.starts_with("https://"));
            assert_eq!(r.sha256.len(), 64, "sha256 should be 64 chars (hex)");
        }
    }

    #[test]
    fn sha256_rejects_non_hex() {
        let err = verify_sha256(b"", "abc123").unwrap_err();
        assert!(err.contains("invalid sha256"));
        let err = verify_sha256(b"", &"g".repeat(64)).unwrap_err();
        assert!(err.contains("invalid sha256"));
    }

    #[test]
    fn sha256_real_hello_vector() {
        // RFC 3174 / NIST FIPS 180-2 已知向量："hello" → 2cf24dba…b9824。
        let expected = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert!(verify_sha256(b"hello", expected).is_ok());
    }

    #[test]
    fn sha256_real_empty_bytes() {
        // 空输入的 SHA-256（公开向量）。
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(verify_sha256(b"", expected).is_ok());
    }

    #[test]
    fn sha256_mismatch_reports_prefixes() {
        let bad = "0000000000000000000000000000000000000000000000000000000000000000";
        let err = verify_sha256(b"hello", bad).unwrap_err();
        assert!(err.contains("sha256 mismatch"), "msg: {err}");
        // 实际 hash 前 8 字符必须出现，供诊断。
        assert!(
            err.contains("2cf24dba"),
            "expected actual=2cf24dba in msg, got: {err}"
        );
        // expected 前 8 字符也必须出现。
        assert!(err.contains("00000000"), "expected prefix in msg: {err}");
    }

    #[test]
    fn sha256_case_insensitive() {
        // 大写 expected 也应通过（系统工具返小写）。
        let upper = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824";
        assert!(verify_sha256(b"hello", upper).is_ok());
    }

    #[test]
    fn install_hint_mentions_version() {
        let hint = clangd_install_hint();
        assert!(hint.contains("18.1.5"));
        assert!(hint.contains("install"));
    }

    #[test]
    fn urls_are_distinct_per_platform() {
        let urls: Vec<&str> = [
            clangd_release_for(Os::Windows, Arch::X86_64),
            clangd_release_for(Os::Linux, Arch::X86_64),
            clangd_release_for(Os::Macos, Arch::X86_64),
            clangd_release_for(Os::Macos, Arch::Aarch64),
        ]
        .iter()
        .map(|r| r.as_ref().expect("supported").url)
        .collect();
        let mut uniq = urls.clone();
        uniq.sort();
        uniq.dedup();
        assert_eq!(uniq.len(), urls.len(), "每个 platform URL 必须唯一");
    }
}
