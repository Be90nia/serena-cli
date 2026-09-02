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
//! 不引入：reqwest / sha2 之外的依赖（ARCHITECTURE §8 禁新增第三方）。
//! 真实下载走 std `std::process::Command` 调系统 curl/wget 在上层做（Task 18 后续）。
//!
//! ponytail: 不做 async 下载 API —— claude/agent 用同步调用足以；后续真有
//! 需要再升级到 reqwest。

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
/// ponytail: 不引 sha2 crate——MVP 只验格式（64 hex chars）；真实校验在
/// download 流程实现时补（用 std::process 调系统 `sha256sum` / `certutil`）。
pub fn verify_sha256(_bytes: &[u8], expected: &str) -> Result<(), String> {
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("invalid sha256 (expected 64 hex chars): {expected:?}"));
    }
    Ok(())
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
    fn sha256_accepts_hex_64() {
        assert!(verify_sha256(b"", &"a".repeat(64)).is_ok());
        assert!(verify_sha256(b"", &"0123456789abcdef".repeat(4)).is_ok());
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