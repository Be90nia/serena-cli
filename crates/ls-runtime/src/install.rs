//! LS 二进制自动安装基建（auto-install-design.md v0.4 §3/§5，Task 18）。
//!
//! ↖ mirror: oraios/serena@43ae021 `dependency_provider.py` + `downloaded_dependency_hashes.json`
//! Δ 上游无 sha 门（直接信任 URL）；本设计 §2.9 按 A 类 sha 未知 → 拒绝 auto，
//!   仅 `--allow-unsigned-sha`（人类显式）越狱。
//!
//! npm/uvx 包管理器安装器见 `install_pkg`（§2.3/§2.5）。
//! 同步签名（设计 §3）：daemon(tokio) 调用方须 `spawn_blocking` 包裹（下载分钟级）。
//! HTTP 走 `reqwest::blocking`（workspace 统一版本，rustls）。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::deps::{verify_sha256, Arch, Os};
use crate::process::RuntimeError;

/// 解压形态（auto-install-design §2.2 `archive.kind`；Raw 为裸二进制扩展——
/// marksman 等直接分发无压缩 exe，Task 19 首批需要）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    TarGz,
    TarXz,
    /// 单文件 gzip（rust-analyzer release 形态：非 tar 容器）。
    SingleGz,
    /// 裸二进制（marksman release 形态）：下载即 bin，无解压。
    Raw,
}

/// A 类（单二进制下载）规格。`sha256` 空 = 未知（UnsignedRefused 门，§2.9）。
///
/// per-platform 解析由调用方（ls-registry ConfigAdapter，Task 19）完成——本类型
/// 收到的是已按 (os, arch) 选定的单条目。Task 18 只落 Download + PathOnly；
/// Npm/Uvx/... 随 servers.toml（Task 19）扩充。
#[derive(Debug, Clone)]
pub enum InstallKind {
    Download {
        version: String,
        url: String,
        sha256: String,
        archive: ArchiveKind,
        /// 解压剥掉的顶层目录层数（clangd 产物带 `clang+llvm-…/` 前缀时为 1）。
        strip_components: usize,
        /// 压缩包内可执行文件相对路径（解压+strip 后仍相对安装根）。
        bin_path: String,
        allowed_hosts: Vec<String>,
    },
    PathOnly {
        binary_name: String,
        install_hint: String,
    },
    /// npm 类（§2.3）：`npm install --prefix {cache}/{id}/{version} <pkg>[@<ver>]`，
    /// 产物 = `node_modules/.bin/<bin_rel>`；`version=None` → latest（不锁版本）。
    Npm {
        package: String,
        version: Option<String>,
        bin_rel: String,
        /// 启动时追加在 bin 之后的参数。
        npm_args: Option<Vec<String>>,
    },
    /// uvx 类（§2.5）：无安装步骤——`uvx --from <pkg>[==<ver>] <entrypoint> <args>`，
    /// uv 运行时自管缓存；PATH 无 `uvx` → NotInstalled + hint。
    Uvx {
        package: String,
        version: Option<String>,
        entrypoint: String,
        /// 启动时追加在 entrypoint 之后的参数。
        args: Option<Vec<String>>,
    },
}

/// 自足安装规格（ls-registry `ServerSpec → InstallSpec` 映射的值子集，momus Critical-1：
/// ls-runtime 禁引用 ServerSpec）。
#[derive(Debug, Clone)]
pub struct InstallSpec {
    pub id: String,
    pub kind: InstallKind,
    /// 含 `{bin}` 占位符的 exec 模板（Task 21 展开接 adapter）。
    pub exec: Vec<String>,
}

/// 安装上下文。
#[derive(Debug, Clone)]
pub struct InstallCtx {
    pub os: Os,
    pub arch: Arch,
    pub auto_install: bool,
    pub allow_unsigned_sha: bool,
    /// 安装根：`{cache_root}/{id}/{version}/`。
    pub cache_root: PathBuf,
}

/// 安装产物。
#[derive(Debug, Clone)]
pub enum Launch {
    Process { exe: PathBuf, args: Vec<String> },
    External { host: String, port: u16 },
}

/// 设计 §3 `InstallOutcome`。
#[derive(Debug, Clone)]
pub enum InstallOutcome {
    Ready(Launch),
    NotInstalled {
        hint: String,
        install_cmd: Option<String>,
    },
    /// sha 未知且未越狱（§2.9）：wire 映射 = LS_NOT_INSTALLED + hint。
    UnsignedRefused { ls_id: String, hint: String },
}

/// Task 18 下载基建。DependencySource trait 全集（含 Npm/Uvx 等）随 Task 19 落，
/// 本结构先实现 A 类 Download 的核心流程。
pub struct DownloadInstaller;

impl DownloadInstaller {
    /// A 类安装主流程：已装短路 → sha 门 → 锁 → 下载 → 校验 → 预检 → 解压 → Ready。
    pub fn install(
        &self,
        ctx: &InstallCtx,
        spec: &InstallSpec,
    ) -> Result<InstallOutcome, RuntimeError> {
        let (version, url, sha256, archive, strip, bin_path, allowed) = match &spec.kind {
            InstallKind::Download {
                version,
                url,
                sha256,
                archive,
                strip_components,
                bin_path,
                allowed_hosts,
            } => (
                version,
                url,
                sha256,
                *archive,
                *strip_components,
                bin_path,
                allowed_hosts,
            ),
            InstallKind::PathOnly { install_hint, .. } => {
                return Ok(InstallOutcome::NotInstalled {
                    hint: install_hint.clone(),
                    install_cmd: None,
                });
            }
            InstallKind::Npm { .. } | InstallKind::Uvx { .. } => {
                return Err(wrong_kind(spec));
            }
        };

        let install_dir = ctx.cache_root.join(&spec.id).join(version);
        let exe = install_dir.join(bin_path);
        if exe.is_file() {
            // 已装短路（probe 语义：文件存在即 Ready；--version 深检留给 Task 21）。
            return Ok(InstallOutcome::Ready(Launch::Process {
                exe,
                args: Vec::new(),
            }));
        }

        // §2.9 sha 门：未知 → 拒绝，除非人类显式越狱。
        match sha_gate(sha256, ctx.allow_unsigned_sha) {
            ShaGate::Verify => {} // 正常校验路径
            ShaGate::Skip => {}   // 越狱：跳过校验
            ShaGate::Refuse => {
                return Ok(InstallOutcome::UnsignedRefused {
                    ls_id: spec.id.clone(),
                    hint: format!(
                        "no trusted sha256 for {url}; re-run with --allow-unsigned-sha (human only) or pin a known hash"
                    ),
                });
            }
        }

        // 锁（§5.3）：{install_dir}/install.lock —— 同 id 同 version 唯一。
        std::fs::create_dir_all(&install_dir).map_err(|e| download_err(url, &format!("create install dir: {e}")))?;
        let _lock = acquire_install_lock(&install_dir)?;

        // 下载（§5.1：首跳显式校验 + 重定向逐跳校验在 client policy 内）。
        let client = build_download_client(allowed).map_err(|e| download_err(url, &format!("http client: {e}")))?;
        let bytes = download(&client, url, allowed)?;

        // sha 校验（Skip = 越狱跳过）。
        if sha_gate(sha256, ctx.allow_unsigned_sha) == ShaGate::Verify {
            verify_sha256(&bytes, &sha256.to_ascii_lowercase()).map_err(|cause| {
                RuntimeError::Download {
                    url: url.clone(),
                    expected_sha: Some(sha256.clone()),
                    actual_sha: None,
                    cause,
                }
            })?;
        }

        // 落临时包 + 预检 + 解压（§5.2 zip-slip）。Raw = 裸二进制，直接落 bin。
        let archive_ext = match archive {
            ArchiveKind::Zip => "zip",
            ArchiveKind::TarGz => "tar.gz",
            ArchiveKind::TarXz => "tar.xz",
            ArchiveKind::SingleGz => "gz",
            ArchiveKind::Raw => {
                std::fs::write(&exe, &bytes)
                    .map_err(|e| download_err(url, &format!("write raw binary: {e}")))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755));
                }
                return Ok(InstallOutcome::Ready(Launch::Process {
                    exe,
                    args: Vec::new(),
                }));
            }
        };
        let pkg = install_dir.join(format!("download.{archive_ext}"));
        std::fs::write(&pkg, &bytes)
            .map_err(|e| download_err(url, &format!("write archive: {e}")))?;

        extract(&pkg, archive, strip, &install_dir)
            .map_err(|cause| download_err(url, &cause))?;
        let _ = std::fs::remove_file(&pkg);

        if !exe.is_file() {
            return Err(download_err(
                url,
                &format!("bin_path `{bin_path}` missing after extract"),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755));
        }

        Ok(InstallOutcome::Ready(Launch::Process {
            exe,
            args: Vec::new(),
        }))
    }
}

/// internal：installer 只认自己的 InstallKind；路由错误 = 调用方 bug。
/// （Spawn.cause 是 io::Error 只装原生 spawn 失败；非下载错误复用空 url 的
/// Download 变体，同 acquire_install_lock 先例。）
pub(crate) fn wrong_kind(spec: &InstallSpec) -> RuntimeError {
    RuntimeError::Download {
        url: String::new(),
        expected_sha: None,
        actual_sha: None,
        cause: format!(
            "internal: wrong installer routed for `{}` (InstallKind mismatch)",
            spec.id
        ),
    }
}

fn download_err(url: &str, cause: &str) -> RuntimeError {
    RuntimeError::Download {
        url: url.to_string(),
        expected_sha: None,
        actual_sha: None,
        cause: cause.to_string(),
    }
}

/// §2.9 sha 门判定（纯函数，单测锚）。
#[derive(Debug, PartialEq, Eq)]
pub enum ShaGate {
    /// 正常校验（非空合法 sha）。
    Verify,
    /// 人类越狱（`--allow-unsigned-sha`）：跳过校验。
    Skip,
    /// sha 未知且未越狱：拒绝。
    Refuse,
}

fn sha_gate(sha256: &str, allow_unsigned: bool) -> ShaGate {
    let known = sha256.len() == 64 && sha256.chars().all(|c| c.is_ascii_hexdigit());
    if known {
        ShaGate::Verify
    } else if allow_unsigned {
        ShaGate::Skip
    } else {
        ShaGate::Refuse
    }
}

/// §5.1 host 白名单（纯函数）：精确匹配 + 大小写折叠。子域**不**通配——
/// 上游 allowed_hosts 均为最终落点 host（github.com → objects.githubusercontent.com 显式列出）。
fn host_allowed(host: &str, allowed: &[String]) -> bool {
    let host = host.to_ascii_lowercase();
    allowed.iter().any(|a| a.to_ascii_lowercase() == host)
}

/// 构造带 §5.1 重定向逐跳校验的 blocking client。
pub fn build_download_client(
    allowed_hosts: &[String],
) -> Result<reqwest::blocking::Client, reqwest::Error> {
    let allowed: Vec<String> = allowed_hosts.to_vec();
    reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() > 10 {
                return attempt.error("too many redirects");
            }
            let host_ok = attempt
                .url()
                .host_str()
                .map(|h| host_allowed(h, &allowed))
                .unwrap_or(false);
            if host_ok {
                attempt.follow()
            } else {
                let url = attempt.url().to_string();
                attempt.error(format!("redirect host not in allowed_hosts: {url}"))
            }
        }))
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(30))
        .build()
}

fn download(
    client: &reqwest::blocking::Client,
    url: &str,
    allowed: &[String],
) -> Result<bytes::Bytes, RuntimeError> {
    // 首跳显式校验（policy 只覆盖重定向链）。
    let parsed: reqwest::Url = url.parse().map_err(|e| {
        RuntimeError::Download {
            url: url.to_string(),
            expected_sha: None,
            actual_sha: None,
            cause: format!("parse url: {e}"),
        }
    })?;
    let first_host_ok = parsed.host_str().map(|h| host_allowed(h, allowed)).unwrap_or(false);
    if !first_host_ok {
        return Err(RuntimeError::Download {
            url: url.to_string(),
            expected_sha: None,
            actual_sha: None,
            cause: format!(
                "host not in allowed_hosts: {} (allowed: {allowed:?})",
                parsed.host_str().unwrap_or("?")
            ),
        });
    }
    let resp = client.get(url).send().map_err(|e| RuntimeError::Download {
        url: url.to_string(),
        expected_sha: None,
        actual_sha: None,
        cause: format!("http get: {e}"),
    })?;
    if !resp.status().is_success() {
        return Err(RuntimeError::Download {
            url: url.to_string(),
            expected_sha: None,
            actual_sha: None,
            cause: format!("http status {}", resp.status()),
        });
    }
    resp.bytes().map_err(|e| RuntimeError::Download {
        url: url.to_string(),
        expected_sha: None,
        actual_sha: None,
        cause: format!("read body: {e}"),
    })
}

/// §5.2 zip-slip 条目消毒（纯函数）：拒 `..` 组件、绝对路径、盘符。
/// tar/zip 条目名规范为 `/` 分隔；`\\` 与盘符是 Windows 恶意条目特征。
pub fn entry_is_safe(name: &str) -> bool {
    if name.is_empty() || name.starts_with('/') || name.starts_with('\\') {
        return false;
    }
    // 盘符：`X:` 形态。
    if name.as_bytes().get(1) == Some(&b':') {
        return false;
    }
    !name.split(['/', '\\']).any(|c| c == "..")
}

/// 解压到 `dest`。策略（设计 §1/§2.2）：系统 tar（bsdtar 支持 zip/tar.gz/tar.xz 与裸 gz；
/// Windows 10+ 内置，macOS 默认 bsdtar）；Linux GNU tar 不解 zip → fallback `unzip`，
/// 不解裸 gz → fallback `gunzip`。缺工具 → 带 MissingRuntime 语义的错误串。
fn extract(pkg: &Path, kind: ArchiveKind, strip_components: usize, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("create dest: {e}"))?;
    // §5.2 预检：解压前列清单逐条目消毒（裸 gz 无清单，单文件无路径语义）。
    if kind != ArchiveKind::SingleGz {
        let list = run_tool_stdout("tar", &["-tf", &pkg.to_string_lossy()])
            .map_err(|e| format!("archive list (zip-slip precheck): {e}"))?;
        for name in String::from_utf8_lossy(&list).lines() {
            if !entry_is_safe(name) {
                return Err(format!("unsafe archive entry: {name:?}"));
            }
        }
    }
    match kind {
        ArchiveKind::Zip => {
            let mut args = vec!["-xf".to_string(), pkg.to_string_lossy().to_string()];
            if strip_components > 0 {
                args.push(format!("--strip-components={strip_components}"));
            }
            args.push("-C".to_string());
            args.push(dest.to_string_lossy().to_string());
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            if run_tool("tar", &arg_refs).is_err() {
                // Linux GNU tar 不识 zip → unzip fallback。
                run_tool(
                    "unzip",
                    &["-o", &pkg.to_string_lossy(), "-d", &dest.to_string_lossy()],
                )
                .map_err(|e| format!("unzip: {e}"))?;
            }
            Ok(())
        }
        ArchiveKind::TarGz | ArchiveKind::TarXz => {
            let mut args = vec!["-xf".to_string(), pkg.to_string_lossy().to_string()];
            if strip_components > 0 {
                args.push(format!("--strip-components={strip_components}"));
            }
            args.push("-C".to_string());
            args.push(dest.to_string_lossy().to_string());
            let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
            run_tool("tar", &arg_refs).map_err(|e| format!("tar extract: {e}"))
        }
        ArchiveKind::SingleGz => {
            // 裸 gz：bsdtar（Windows/macOS）直接解；Linux GNU tar 不行 → gunzip -kc 落盘。
            if run_tool(
                "tar",
                &["-xzf", &pkg.to_string_lossy(), "-C", &dest.to_string_lossy()],
            )
            .is_ok()
            {
                return Ok(());
            }
            let out_name = pkg
                .file_name()
                .map(|n| n.to_string_lossy().trim_end_matches(".gz").to_string())
                .unwrap_or_else(|| "binary".to_string());
            let bytes = run_tool_stdout("gunzip", &["-kc", &pkg.to_string_lossy()])
                .map_err(|e| format!("gunzip: {e}"))?;
            std::fs::write(dest.join(out_name), bytes).map_err(|e| format!("write gunzipped: {e}"))
        }
        ArchiveKind::Raw => {
            // install() 对 Raw 提前返回，永不进 extract；防御性兜底。
            unreachable!("Raw binaries never reach extract()")
        }
    }
}

fn run_tool(program: &str, args: &[&str]) -> Result<(), String> {
    run_tool_stdout(program, args).map(|_| ())
}

fn run_tool_stdout(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!("`{program}` not found (MissingRuntime: {program})")
            } else {
                format!("`{program}` spawn: {e}")
            }
        })
        .and_then(|out| {
            if out.status.success() {
                Ok(out.stdout)
            } else {
                Err(format!(
                    "`{program}` exit {:?}: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr)
                ))
            }
        })
}

/// §5.3 锁原子获取：唯一 tmp `create_new` → 同卷 rename；目标已存在 → 陈锁判定
/// （age > 10min）删后重试，fresh 锁等待重试（3 次 × 500ms）→ Err。
pub fn acquire_install_lock(dir: &Path) -> Result<InstallLock, RuntimeError> {
    let lock = dir.join("install.lock");
    for _ in 0..3 {
        let tmp = dir.join(format!("install.lock.tmp.{}", std::process::id()));
        let created = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp);
        let rename_failed = match created {
            Ok(_) => {
                // Windows std::fs::rename = MOVEFILE_REPLACE_EXISTING，会静默覆盖
                // 已存在锁 —— 锁语义要求目标存在时绝不覆盖，先显式检查。
                if lock.exists() {
                    let _ = std::fs::remove_file(&tmp);
                    true
                } else {
                    std::fs::rename(&tmp, &lock).is_err()
                }
            }
            Err(_) => true,
        };
        if rename_failed {
            let _ = std::fs::remove_file(&tmp);
            if is_stale_lock(&lock) {
                let _ = std::fs::remove_file(&lock);
                continue;
            }
        } else {
            return Ok(InstallLock { path: lock });
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Err(RuntimeError::Download {
        url: String::new(),
        expected_sha: None,
        actual_sha: None,
        cause: format!("install.lock busy: {}", lock.display()),
    })
}

fn is_stale_lock(lock: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(lock) else {
        return false; // 锁不存在 → 不陈（rename 竞态，下轮 create_new 决胜）
    };
    let age = meta
        .modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .unwrap_or_default();
    age > Duration::from_secs(10 * 60)
}

/// RAII 锁守卫：Drop 时删锁文件（进程崩溃则留给陈锁判定回收）。
#[derive(Debug)]
pub struct InstallLock {
    path: PathBuf,
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha_gate_decides_by_known_hash_and_jail_flag() {
        let good = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        assert_eq!(sha_gate(good, false), ShaGate::Verify);
        assert_eq!(sha_gate(good, true), ShaGate::Verify, "已知 sha 不受越狱 flag 影响");
        // 未知（空/假形态）：默认拒绝；越狱跳过。
        assert_eq!(sha_gate("", false), ShaGate::Refuse);
        assert_eq!(sha_gate("", true), ShaGate::Skip);
        assert_eq!(sha_gate("abcd", false), ShaGate::Refuse);
        assert_eq!(
            sha_gate(&"0".repeat(64), false),
            ShaGate::Verify,
            "64hex 形态一律走校验路径（假值必败，不作 unsigned 放行）"
        );
    }

    #[test]
    fn host_allowed_exact_match_only() {
        let allowed = vec![
            "github.com".to_string(),
            "objects.githubusercontent.com".to_string(),
        ];
        assert!(host_allowed("github.com", &allowed));
        assert!(host_allowed("objects.githubusercontent.com", &allowed));
        assert!(host_allowed("GITHUB.com", &allowed), "host 大小写折叠");
        assert!(!host_allowed("evil.github.com", &allowed), "子域不通配");
        assert!(!host_allowed("github.com.evil.io", &allowed), "后缀伪装不通配");
        assert!(!host_allowed("gitlab.com", &allowed));
    }

    #[test]
    fn entry_is_safe_blocks_zip_slip() {
        assert!(entry_is_safe("bin/clangd.exe"));
        assert!(entry_is_safe("clang+llvm-18/bin/clangd"));
        assert!(!entry_is_safe("../evil"));
        assert!(!entry_is_safe("a/../../evil"));
        assert!(!entry_is_safe("/abs/path"));
        assert!(!entry_is_safe("\\abs\\path"));
        assert!(!entry_is_safe("C:\\Windows\\evil.exe"), "盘符");
        assert!(!entry_is_safe("a\\b\\..\\evil"), "反斜杠分隔的 ..");
        assert!(!entry_is_safe(""));
    }

    #[test]
    fn install_lock_blocks_fresh_and_recovers_stale() {
        let dir = tempfile::tempdir().unwrap();
        // fresh 锁 → 3 轮重试后 busy 失败。
        std::fs::write(dir.path().join("install.lock"), "").unwrap();
        let err = acquire_install_lock(dir.path()).unwrap_err();
        assert!(err.to_string().contains("busy"));
        // 陈锁（mtime 拨回 11min 前）→ 回收重建成功。
        let old = SystemTime::now() - Duration::from_secs(11 * 60);
        let f = std::fs::File::options()
            .write(true)
            .open(dir.path().join("install.lock"))
            .unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        let guard = acquire_install_lock(dir.path()).expect("stale lock must be recovered");
        assert!(guard.path.is_file());
        drop(guard);
        assert!(!dir.path().join("install.lock").exists(), "Drop 删锁");
    }

    /// §5.1 + §2.9 端到端（真下载 + sha 校验，门控）：默认跳过（CI/离线环境）。
    /// 资产真值锚 GitHub API `assets[].digest`（2026-09-19 查询，tag 2026-09-07）。
    #[test]
    fn download_verify_e2e_real_file_when_gated() {
        if std::env::var_os("SERENA_TEST_DOWNLOAD").is_none() {
            return;
        }
        let allowed = vec![
            "github.com".into(),
            "release-assets.githubusercontent.com".into(),
            "objects.githubusercontent.com".into(),
        ];
        let client = build_download_client(&allowed).unwrap();
        let bytes = download(
            &client,
            "https://github.com/rust-lang/rust-analyzer/releases/download/2026-09-07/rust-analyzer-x86_64-unknown-linux-musl.gz",
            &allowed,
        )
        .expect("真实下载应成功");
        verify_sha256(
            &bytes,
            "d05af6fdc2eab4e2b348f53029b6557369c910ba6cb2006d85cc1fb6ea400c35",
        )
        .expect("官方 digest 应匹配");
        // 白名单外 host 拒绝。
        let err = download(&client, "https://gitlab.com/x", &allowed).unwrap_err();
        assert!(err.to_string().contains("allowed_hosts"));
    }

    /// install() 全流程端到端（真下载 + 校验 + 解压 + bin 落地，门控）。
    /// 走 InstallKind::Zip 路径（Windows tar.exe/bsdtar 解压）。
    #[test]
    fn install_full_pipeline_e2e_when_gated() {
        if std::env::var_os("SERENA_TEST_DOWNLOAD").is_none() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ctx = InstallCtx {
            os: Os::Windows,
            arch: Arch::X86_64,
            auto_install: true,
            allow_unsigned_sha: false,
            cache_root: dir.path().to_path_buf(),
        };
        let spec = InstallSpec {
            id: "rust-analyzer".into(),
            kind: InstallKind::Download {
                version: "2026-09-07".into(),
                url: "https://github.com/rust-lang/rust-analyzer/releases/download/2026-09-07/rust-analyzer-x86_64-pc-windows-msvc.zip".into(),
                sha256: "cd3dddd580edac199c5e84c6cceb3addea32769c25cb6b597024a200b3359b05".into(),
                archive: ArchiveKind::Zip,
                strip_components: 0,
                bin_path: "rust-analyzer.exe".into(),
                allowed_hosts: vec![
                    "github.com".into(),
                    "release-assets.githubusercontent.com".into(),
                    "objects.githubusercontent.com".into(),
                ],
            },
            exec: vec!["{bin}".into()],
        };
        let out = DownloadInstaller.install(&ctx, &spec).expect("全流程应成功");
        let InstallOutcome::Ready(Launch::Process { exe, .. }) = out else {
            panic!("应 Ready，实际 {out:?}");
        };
        assert!(exe.is_file(), "bin 应落地: {}", exe.display());
        // 已装短路：二调不再下载（直接 Ready）。
        let out2 = DownloadInstaller.install(&ctx, &spec).expect("二调应成功");
        assert!(
            matches!(out2, InstallOutcome::Ready(Launch::Process { .. })),
            "二调应走已装短路，实际 {out2:?}"
        );
    }

}
