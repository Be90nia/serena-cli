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
//!
//! Task 22d sha256 真值口径（Phase 4 基建）：
//! - A 类 download 条目 24 条真值已搬到 `crates/ls-registry/servers.toml`，由
//!   `tests/servers_toml_coverage.rs::new_download_entries_parse_with_complete_fields`
//!   闸门校验 64-hex 长度 + 字符集 + url/sha 1:1 配对。锚源见 `local/ls-download-matrix.md`。
//! - rust-analyzer 4 平台已在下面 `rust_analyzer_release_for` 真值填齐（GitHub
//!   API assets[].digest）。
//! - clangd 4 平台仍**占位空串**——LLVM 官方 release 资产**无 SHA256SUMS 旁文件**
//!   （`releases.llvm.org` 不签二级制品），上游 serena
//!   `downloaded_dependency_hashes.json` 也不含 clangd 条目。设计 §2.9：sha 未知 →
//!   拒绝 auto-install，仅 `--allow-unsigned-sha` 人类显式越狱——假 hash 必败校验
//!   更危险（错把 LLVM 资产挂掉就成 P0）。
//! - 后续 LS 加进来时按 `clangd_release_for` 模板复制，sha 真值表见 ls-download-matrix。

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
///
/// sha256 **空串 = 未知**（auto-install-design §2.9：未知 → 拒绝 auto，仅
/// `--allow-unsigned-sha` 越狱）。上游 serena 的 `downloaded_dependency_hashes.json`
/// 不含 clangd 条目（无官方一手来源），故按设计留空——假 hash 必败校验更危险。
pub fn clangd_release_for(os: Os, arch: Arch) -> Option<ClangdRelease> {
    let _ = os; // 矩阵键；当前 4 条目均 x86_64/aarch64 显式列出
    match (os, arch) {
        (Os::Windows, Arch::X86_64) => Some(ClangdRelease {
            url: "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-x86_64-pc-windows-msvc.tar.xz",
            sha256: "",
            binary_path: "clang+llvm-18.1.5-x86_64-pc-windows-msvc/bin/clangd.exe",
        }),
        (Os::Linux, Arch::X86_64) => Some(ClangdRelease {
            url: "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-x86_64-linux-gnu-ubuntu-22.04.tar.xz",
            sha256: "",
            binary_path: "clang+llvm-18.1.5-x86_64-linux-gnu-ubuntu-22.04/bin/clangd",
        }),
        (Os::Macos, Arch::X86_64) => Some(ClangdRelease {
            url: "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-x86_64-apple-darwin.tar.xz",
            sha256: "",
            binary_path: "clang+llvm-18.1.5-x86_64-apple-darwin/bin/clangd",
        }),
        (Os::Macos, Arch::Aarch64) => Some(ClangdRelease {
            url: "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-aarch64-apple-darwin.tar.xz",
            sha256: "",
            binary_path: "clang+llvm-18.1.5-aarch64-apple-darwin/bin/clangd",
        }),
        (Os::Windows, Arch::Aarch64) | (Os::Linux, Arch::Aarch64) => None,
    }
}

/// rust-analyzer (Os, Arch) → (url, sha256, 压缩形态) 矩阵。
///
/// 真值锚：GitHub API `assets[].digest`（rust-lang/rust-analyzer tag **2026-09-07**，
/// 2026-09-19 查询）——rust-analyzer release 无 SHA256SUMS 文件，API digest 即官方分发锚。
/// 形态：Windows `.zip`；其余 `.gz`（单文件 gzip，非 tar 容器）。
pub fn rust_analyzer_release_for(os: Os, arch: Arch) -> Option<RustAnalyzerRelease> {
    match (os, arch) {
        (Os::Windows, Arch::X86_64) => Some(RustAnalyzerRelease {
            url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-09-07/rust-analyzer-x86_64-pc-windows-msvc.zip".into(),
            sha256: "cd3dddd580edac199c5e84c6cceb3addea32769c25cb6b597024a200b3359b05".into(),
            archive: crate::install::ArchiveKind::Zip,
            bin_name: "rust-analyzer.exe".into(),
        }),
        (Os::Linux, Arch::X86_64) => Some(RustAnalyzerRelease {
            url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-09-07/rust-analyzer-x86_64-unknown-linux-gnu.gz".into(),
            sha256: "a3500183aa08bf740c0da6e030ad262d4cfa1c19e7ce195ab5f772bdf9ddfb12".into(),
            archive: crate::install::ArchiveKind::SingleGz,
            bin_name: "rust-analyzer".into(),
        }),
        (Os::Macos, Arch::X86_64) => Some(RustAnalyzerRelease {
            url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-09-07/rust-analyzer-x86_64-apple-darwin.gz".into(),
            sha256: "41161c05bd7e2396a5cea86a691d37919ec0af331a06bbdc0f323979034f9dad".into(),
            archive: crate::install::ArchiveKind::SingleGz,
            bin_name: "rust-analyzer".into(),
        }),
        (Os::Macos, Arch::Aarch64) => Some(RustAnalyzerRelease {
            url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-09-07/rust-analyzer-aarch64-apple-darwin.gz".into(),
            sha256: "16e9b2af9db7c0ce015ffe88f85db27669b05c84887a59c737d697d2c5f8d349".into(),
            archive: crate::install::ArchiveKind::SingleGz,
            bin_name: "rust-analyzer".into(),
        }),
        _ => None,
    }
}

/// rust-analyzer release 条目（install.rs 需要的信息平铺）。
pub struct RustAnalyzerRelease {
    pub url: String,
    pub sha256: String,
    pub archive: crate::install::ArchiveKind,
    pub bin_name: String,
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
            // §2.9 语义：空 = 未知（UnsignedRefused 门）；非空必须 64hex。
            assert!(
                r.sha256.is_empty() || r.sha256.len() == 64,
                "sha256 must be empty (unknown) or 64 hex chars"
            );
        }
    }

    #[test]
    fn rust_analyzer_matrix_pins_official_digests() {
        for (os, arch) in [
            (Os::Windows, Arch::X86_64),
            (Os::Linux, Arch::X86_64),
            (Os::Macos, Arch::X86_64),
            (Os::Macos, Arch::Aarch64),
        ] {
            let r = rust_analyzer_release_for(os, arch).expect("4 主流平台必须有条目");
            assert_eq!(r.sha256.len(), 64, "rust-analyzer 真值已锚定（GitHub digest）");
            assert!(r.url.contains("2026-09-07"), "版本钉死: {}", r.url);
            assert!(r.url.starts_with("https://github.com/rust-lang/"));
        }
        assert!(rust_analyzer_release_for(Os::Windows, Arch::Aarch64).is_none());
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

    /// Task 22d：clangd 4 平台 sha256 必须**全部为空**——这是已知的 known-unknown，
    /// 不是占位假值。设计 §2.9 门：empty → 拒绝 auto-install，仅
    /// `--allow-unsigned-sha` 越狱。锚源 LLVM 官方 release 资产无 SHA256SUMS 旁
    /// 文件（`releases.llvm.org` 不签二级制品），上游 serena 内嵌 hash-DB 也不含
    /// clangd 条目。
    #[test]
    fn clangd_known_unknown_sha256_documented() {
        for (os, arch) in [
            (Os::Windows, Arch::X86_64),
            (Os::Linux, Arch::X86_64),
            (Os::Macos, Arch::X86_64),
            (Os::Macos, Arch::Aarch64),
        ] {
            let r = clangd_release_for(os, arch).expect("4 主流平台必须有条目");
            assert_eq!(
                r.sha256, "",
                "clangd {os:?}/{arch:?} sha256 必须为空（LLVM 官方无 SHA256SUMS 端点，\
                 §2.9 已知 unknown）。若此断言失败 = 有人填了假值，违反 §2.9。",
            );
            assert!(
                r.url.contains("llvm.org-18.1.5") || r.url.contains("llvm-project"),
                "clangd URL 锚必须为 llvm-project 18.1.5"
            );
        }
    }

    /// Task 22d：rust-analyzer 4 平台 sha256 必须是 64-hex 真值（GitHub API digest）。
    /// 若 release 滚动后 digest 变，需重新查询 `gh api .../releases` 并更新。
    #[test]
    fn rust_analyzer_sha256_are_64hex_truth_values() {
        for (os, arch) in [
            (Os::Windows, Arch::X86_64),
            (Os::Linux, Arch::X86_64),
            (Os::Macos, Arch::X86_64),
            (Os::Macos, Arch::Aarch64),
        ] {
            let r = rust_analyzer_release_for(os, arch).expect("4 主流平台必须有条目");
            assert_eq!(r.sha256.len(), 64, "rust-analyzer 真值 64-hex");
            assert!(r.sha256.chars().all(|c| c.is_ascii_hexdigit()));
            // 真值不应是已知假值 000…0（防御 regression 误填零）
            assert!(
                !r.sha256.chars().all(|c| c == '0'),
                "rust-analyzer sha256 不应全 0"
            );
        }
    }
}
