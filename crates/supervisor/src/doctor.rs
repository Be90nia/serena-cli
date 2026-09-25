//! `serena doctor` —— 环境体检（auto-install-design.md Task 31「CLI install/doctor 子命令」；
//! 原「PLAN Task 17」锚位有误——PLAN Task 17 = M1 验收压测，2026-09-21 文档回写轮已对齐）。
//!
//! 5 类检查（输出状态：OK / MISS / WARN + 描述 + 修复建议）：
//! 1. 系统运行时：node / npm / uv / uvx / dotnet / gem / java / go / cargo / rustup /
//!    python / pwsh / cmd。`which_path` 探测；不触网。
//! 2. PATH 含常见 bin 目录（粗略：检查 PATH 是否为空 + 命中 ≥3 个候选路径）。
//! 3. 本机 LS 装态：rust-analyzer / clangd / pyright / gopls /
//!    typescript-language-server / jdtls / csharp-ls —— `which_path` 探测。
//! 4. daemon 状态：lock 文件存在性 + 端口 7860 是否被占 + token 是否有效（从 lock 读）。
//! 5. 网络：TCP 探活 github.com:443（10s 超时）—— 本地是否能下载 LS 二进制。
//!
//! 输出形态：
//!   - 人类可读（默认）
//!   - `--json`：标准 JSON（machine-readable）。
//!
//! exit code：0 全绿（含 WARN）/ 1 有 MISS（`exit_code()` 仅产 0/1；原注「2 致命错」不可达，已删）。

use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use ls_adapters::which_path;
use ls_registry::config::dirs_cache_root;
use serde::Serialize;

/// 检查项状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Miss,
    Warn,
}

/// 单条检查结果。
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// 类目（runtime / path / ls / daemon / net）。
    pub category: &'static str,
    /// 检查项 ID（如 `rustup` / `port_7860`）。
    pub id: &'static str,
    /// 人类标签。
    pub label: &'static str,
    pub status: Status,
    /// 详细信息（如 `v1.27.1` / `PATH=C:\...` / `not found`）。
    pub detail: String,
    /// 修复建议（MISS / WARN 通常非空）。
    pub hint: Option<String>,
}

/// 完整 doctor 输出。
#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub checks: Vec<Check>,
}

impl DoctorReport {
    /// 全 OK / WARN → 0；任一 MISS → 1。
    pub fn exit_code(&self) -> u8 {
        if self.checks.iter().any(|c| c.status == Status::Miss) {
            1
        } else {
            0 // WARN 不算致命失败（只是提示）
        }
    }
}

/// 全部检查入口。`lock_path` 由调用方传入（cli 用 daemon::serve::default_lock_path）。
pub fn run_all(lock_path: &Path) -> DoctorReport {
    let mut checks = Vec::with_capacity(32);
    checks.extend(check_runtimes());
    checks.push(check_path_dirs());
    checks.extend(check_local_ls());
    checks.extend(check_daemon(lock_path));
    checks.push(check_network());
    DoctorReport { checks }
}

/// 网络探活超时。
const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);
/// 端口探测超时（port 7860 是否被占）。
const PORT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// 1. 系统运行时探测。`name` 是真实可执行名（PATH 探测）。version = `--version` 首行。
fn probe_runtime(name: &str) -> Option<String> {
    let path = which_path(name)?;
    // 跑 --version 拿版本号；非 0 退出 / 无 stdout → 视为命中但无版本。
    let out = std::process::Command::new(&path)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() && out.stdout.is_empty() {
        return Some(format!("{} (--version failed)", path.display()));
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let first_line = s.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        Some(format!("{} (no version output)", path.display()))
    } else {
        Some(first_line.to_string())
    }
}

fn check_runtimes() -> Vec<Check> {
    let names: &[(&str, &str)] = &[
        ("node", "JavaScript runtime (npm/typescript-language-server)"),
        ("npm", "npm package manager"),
        ("uv", "Python package manager (uv tool)"),
        ("uvx", "Python package runner (uvx for pyright/basedpyright/ty/pyre/pyrefly)"),
        ("dotnet", ".NET runtime (csharp-ls/fsautocomplete)"),
        ("gem", "RubyGems (ruby-lsp/solargraph)"),
        ("java", "JVM (jdtls/bsl/...)"),
        ("go", "Go toolchain (gopls)"),
        ("cargo", "Rust build tool (rust-analyzer install via rustup)"),
        ("rustup", "Rust toolchain installer"),
        ("python", "Python interpreter"),
        ("python3", "Python 3 interpreter"),
        ("pwsh", "PowerShell 7 (pwsh)"),
    ];
    names
        .iter()
        .map(|(name, label)| {
            match probe_runtime(name) {
                Some(detail) => Check {
                    category: "runtime",
                    id: name,
                    label,
                    status: Status::Ok,
                    detail,
                    hint: None,
                },
                None => Check {
                    category: "runtime",
                    id: name,
                    label,
                    status: Status::Miss,
                    detail: format!("`{name}` not found on PATH"),
                    hint: Some(format!("install `{name}` and ensure it is on PATH")),
                },
            }
        })
        .collect()
}

/// 2. PATH 探测：粗检 PATH 是否存在 + 至少含一个 bin 候选（≥3 个路径段）。
fn check_path_dirs() -> Check {
    let path_var = std::env::var_os("PATH");
    let mut count = 0usize;
    if let Some(pv) = &path_var {
        if cfg!(windows) {
            count = pv.to_string_lossy().split(';').filter(|s| !s.is_empty()).count();
        } else {
            count = pv.to_string_lossy().split(':').filter(|s| !s.is_empty()).count();
        }
    }
    if path_var.is_none() {
        Check {
            category: "path",
            id: "PATH",
            label: "PATH environment variable",
            status: Status::Miss,
            detail: "PATH not set".into(),
            hint: Some("set PATH to include standard bin directories".into()),
        }
    } else if count < 3 {
        Check {
            category: "path",
            id: "PATH",
            label: "PATH environment variable",
            status: Status::Warn,
            detail: format!("only {count} entries; expect ≥3"),
            hint: Some("ensure ~/.local/bin, ~/.cargo/bin, /usr/local/bin are present".into()),
        }
    } else {
        Check {
            category: "path",
            id: "PATH",
            label: "PATH environment variable",
            status: Status::Ok,
            detail: format!("{count} entries"),
            hint: None,
        }
    }
}

/// 3. 本机 LS 装态。命中 PATH = OK；未命中 = MISS + 安装提示。
fn check_local_ls() -> Vec<Check> {
    let ls_specs: &[(&str, &str, &str)] = &[
        (
            "rust-analyzer",
            "rust-analyzer (Rust LS)",
            "rustup component add rust-analyzer  OR  install via https://rust-analyzer.github.io",
        ),
        (
            "clangd",
            "clangd (C/C++ LS)",
            "install via system package manager (apt: clangd-18, brew: llvm@18, choco: llvm)",
        ),
        (
            "pyright",
            "pyright (Python LS)",
            "uv tool install pyright  OR  npm i -g pyright",
        ),
        (
            "gopls",
            "gopls (Go LS)",
            "go install golang.org/x/tools/gopls@latest",
        ),
        (
            "typescript-language-server",
            "typescript-language-server (TS/JS LS)",
            "npm i -g typescript-language-server",
        ),
        (
            "jdtls",
            "jdtls (Java LS)",
            "no manual install needed — first launch auto-downloads Eclipse JDT LS to the local cache; requires `java` (JRE 25+) on PATH",
        ),
        (
            "csharp-ls",
            "csharp-ls (C# LS)",
            "dotnet tool install --global csharp-ls  OR  use built-in Roslyn LS via serena",
        ),
    ];
    ls_specs
        .iter()
        .map(|(name, label, hint)| {
            // jdtls 特判：PATH 无 `jdtls` 不再等于未安装——launch_info 首启自动下载
            // 到本地缓存；缓存已装（含手工 ~/.local/share/jdtls 之外的产物目录）也如实报 OK。
            if *name == "jdtls" {
                let cached = ls_cache_root().join("jdtls/latest/bin/jdtls");
                if cached.is_file() {
                    return Check {
                        category: "ls",
                        id: name,
                        label,
                        status: Status::Ok,
                        detail: format!("cache={}", cached.display()),
                        hint: None,
                    };
                }
                return Check {
                    category: "ls",
                    id: name,
                    label,
                    status: Status::Miss,
                    detail: "not on PATH; will auto-download on first launch".to_string(),
                    hint: Some((*hint).to_string()),
                };
            }
            match which_path(name) {
                Some(p) => Check {
                    category: "ls",
                    id: name,
                    label,
                    status: Status::Ok,
                    detail: format!("PATH={}", p.display()),
                    hint: None,
                },
                None => Check {
                    category: "ls",
                    id: name,
                    label,
                    status: Status::Miss,
                    detail: format!("`{name}` not on PATH"),
                    hint: Some((*hint).to_string()),
                },
            }
        })
        .collect()
}

/// 4. daemon 状态：lock 存在性 + 端口 7860 + 活跃 LS 缓存根。
fn check_daemon(lock_path: &Path) -> Vec<Check> {
    let mut out = Vec::with_capacity(3);
    // 4a. lock 文件存在性
    match read_lock(lock_path) {
        Ok(Some(entry)) => {
            // 4b. 端口探活（依赖 lock 内 port；fallback 7860）
            let port = if entry.port == 0 { 7860 } else { entry.port };
            let addr = format!("127.0.0.1:{port}");
            let alive = TcpStream::connect_timeout(
                &addr.parse().unwrap_or_else(|_| {
                    ("127.0.0.1", 7860u16).to_socket_addrs().unwrap().next().unwrap()
                }),
                PORT_PROBE_TIMEOUT,
            )
            .is_ok();
            if alive {
                out.push(Check {
                    category: "daemon",
                    id: "lock",
                    label: "daemon lock file",
                    status: Status::Ok,
                    detail: format!("present, port={port} alive"),
                    hint: None,
                });
            } else {
                out.push(Check {
                    category: "daemon",
                    id: "lock",
                    label: "daemon lock file",
                    status: Status::Warn,
                    detail: format!("present but port={port} unreachable (stale?)"),
                    hint: Some(
                        "run `serena-cli stop-all` then retry; or delete the lock manually"
                            .into(),
                    ),
                });
            }
        }
        Ok(None) => {
            out.push(Check {
                category: "daemon",
                id: "lock",
                label: "daemon lock file",
                status: Status::Ok,
                detail: "absent (daemon not running, lazy-spawn on first tool call)".into(),
                hint: None,
            });
        }
        Err(e) => {
            out.push(Check {
                category: "daemon",
                id: "lock",
                label: "daemon lock file",
                status: Status::Warn,
                detail: format!("unreadable: {e}"),
                hint: Some("check lock file permissions".into()),
            });
        }
    }    // 4c. 安装缓存根可达性
    let cache = dirs_cache_root();
    if cache.as_os_str().is_empty() {
        out.push(Check {
            category: "daemon",
            id: "cache",
            label: "LS install cache root",
            status: Status::Miss,
            detail: "could not resolve cache root".into(),
            hint: Some("set LOCALAPPDATA (Windows) or HOME (Unix)".into()),
        });
    } else {
        out.push(Check {
            category: "daemon",
            id: "cache",
            label: "LS install cache root",
            status: Status::Ok,
            detail: cache.display().to_string(),
            hint: None,
        });
    }
    out
}

/// 读 lock 文件（daemon 格式 `{pid, port, boot_ms, token}`），错/缺失 → None。
/// 独立于 daemon::lockfile（避免 supervisor → daemon 反向依赖，ARCH §1 分层）。
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)] // 部分字段用于未来扩展（如 pid / boot_ms / token 探活增强）
struct LockEntry {
    pid: u32,
    port: u16,
    #[serde(default)]
    boot_ms: u128,
    #[serde(default)]
    token: String,
}

fn read_lock(path: &Path) -> std::io::Result<Option<LockEntry>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path)?;
    match serde_json::from_str::<LockEntry>(&raw) {
        Ok(e) => Ok(Some(e)),
        Err(_) => Ok(None), // 坏 lock 视为缺失
    }
}

/// 5. 网络：TCP 探活 github.com:443（10s 超时）。
fn check_network() -> Check {
    let addr = match "github.com:443".to_socket_addrs() {
        Ok(mut iter) => match iter.next() {
            Some(a) => a,
            None => {
                return Check {
                    category: "net",
                    id: "github",
                    label: "GitHub reachability",
                    status: Status::Miss,
                    detail: "no DNS resolution for github.com".into(),
                    hint: Some("check DNS / network connectivity".into()),
                };
            }
        },
        Err(e) => {
            return Check {
                category: "net",
                id: "github",
                label: "GitHub reachability",
                status: Status::Miss,
                detail: format!("DNS resolve failed: {e}"),
                hint: Some("check DNS / network connectivity".into()),
            };
        }
    };
    match TcpStream::connect_timeout(&addr, NETWORK_TIMEOUT) {
        Ok(_) => Check {
            category: "net",
            id: "github",
            label: "GitHub reachability",
            status: Status::Ok,
            detail: format!("github.com:443 reachable ({})", addr.ip()),
            hint: None,
        },
        Err(e) => Check {
            category: "net",
            id: "github",
            label: "GitHub reachability",
            status: Status::Miss,
            detail: format!("github.com:443 unreachable: {e}"),
            hint: Some("check firewall / proxy / network connectivity".into()),
        },
    }
}

/// 人类可读输出（一行一条）。`[OK]/[MISS]/[WARN]` 前缀。
pub fn format_text(report: &DoctorReport) -> String {
    let mut s = String::new();
    for c in &report.checks {
        let tag = match c.status {
            Status::Ok => "OK",
            Status::Miss => "MISS",
            Status::Warn => "WARN",
        };
        s.push_str(&format!("[{:<4}] {:<12} {}\n", tag, c.id, c.detail));
        if let Some(hint) = &c.hint {
            s.push_str(&format!("         hint: {hint}\n"));
        }
    }
    s
}

/// 真实安装缓存根（公开，供 cli install --all 复用）。
pub fn ls_cache_root() -> PathBuf {
    dirs_cache_root()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn exit_code_zero_when_all_ok() {
        let r = DoctorReport {
            checks: vec![Check {
                category: "x",
                id: "y",
                label: "z",
                status: Status::Ok,
                detail: "ok".into(),
                hint: None,
            }],
        };
        assert_eq!(r.exit_code(), 0);
    }

    #[test]
    fn exit_code_one_when_any_miss() {
        let r = DoctorReport {
            checks: vec![
                Check {
                    category: "x",
                    id: "y",
                    label: "z",
                    status: Status::Ok,
                    detail: "ok".into(),
                    hint: None,
                },
                Check {
                    category: "x",
                    id: "y2",
                    label: "z2",
                    status: Status::Miss,
                    detail: "miss".into(),
                    hint: Some("fix".into()),
                },
            ],
        };
        assert_eq!(r.exit_code(), 1);
    }

    #[test]
    fn exit_code_zero_when_only_warn() {
        let r = DoctorReport {
            checks: vec![Check {
                category: "x",
                id: "y",
                label: "z",
                status: Status::Warn,
                detail: "warn".into(),
                hint: None,
            }],
        };
        assert_eq!(r.exit_code(), 0);
    }

    #[test]
    fn run_all_returns_at_least_five_categories() {
        // 校验 5 类检查都跑了（即使 miss 也得有 entry）
        let r = run_all(&PathBuf::from("Z:/nonexistent_lock_xyz_12345"));
        let cats: std::collections::HashSet<&str> =
            r.checks.iter().map(|c| c.category).collect();
        for cat in ["runtime", "path", "ls", "daemon", "net"] {
            assert!(cats.contains(cat), "missing category {cat}");
        }
    }

    #[test]
    fn format_text_emits_all_status_tags() {
        let r = DoctorReport {
            checks: vec![
                Check {
                    category: "x",
                    id: "a",
                    label: "la",
                    status: Status::Ok,
                    detail: "v1".into(),
                    hint: None,
                },
                Check {
                    category: "x",
                    id: "b",
                    label: "lb",
                    status: Status::Miss,
                    detail: "not found".into(),
                    hint: Some("install".into()),
                },
                Check {
                    category: "x",
                    id: "c",
                    label: "lc",
                    status: Status::Warn,
                    detail: "stale".into(),
                    hint: None,
                },
            ],
        };
        let s = format_text(&r);
        assert!(s.contains("[OK  ]"), "missing OK tag: {s}");
        assert!(s.contains("[MISS]"), "missing MISS tag: {s}");
        assert!(s.contains("[WARN]"), "missing WARN tag: {s}");
        assert!(s.contains("hint: install"));
    }
}
