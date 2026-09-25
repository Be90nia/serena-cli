//! 包管理器托管类安装器（auto-install-design.md §2.3 npm / §2.5 uvx，机制扩展）。
//!
//! 与 install.rs（A 类单二进制下载）同构：unit struct + `install(&ctx, &spec)`，
//! 无 trait 泛化（每类一个实现）。uvx 无安装步骤——uv 运行时自管缓存。

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::install::{
    InstallCtx, InstallKind, InstallOutcome, InstallSpec, Launch, acquire_install_lock, wrong_kind,
};
use crate::process::RuntimeError;

/// npm 类安装器（§2.3，与 DownloadInstaller 同构：unit struct + install）。
/// 缓存命中（`{cache}/{id}/{version}/node_modules/.bin/<bin_rel>`）短路 → 未命中 +
/// auto_install → 锁 + `npm install --prefix`（同步阻塞，daemon 调用方 spawn_blocking）。
pub struct NpmInstaller;

impl NpmInstaller {
    pub fn install(
        &self,
        ctx: &InstallCtx,
        spec: &InstallSpec,
    ) -> Result<InstallOutcome, RuntimeError> {
        let (package, version, bin_rel, npm_args, secondary) = match &spec.kind {
            InstallKind::Npm {
                package,
                version,
                bin_rel,
                npm_args,
                secondary,
            } => (package, version, bin_rel, npm_args, secondary),
            _ => return Err(wrong_kind(spec)),
        };
        let dir_name = version.clone().unwrap_or_else(|| "latest".to_string());
        let install_dir = ctx.cache_root.join(&spec.id).join(dir_name);
        if let Some(exe) = npm_bin_path(&install_dir, bin_rel) {
            return Ok(InstallOutcome::Ready(Launch::Process {
                exe,
                args: npm_args.clone().unwrap_or_default(),
            }));
        }
        if !ctx.auto_install {
            return Ok(InstallOutcome::NotInstalled {
                hint: format!(
                    "npm package `{package}` not installed under {}",
                    install_dir.display()
                ),
                install_cmd: Some(format!(
                    "npm install --prefix {} {}",
                    install_dir.display(),
                    npm_pkg_ref(package, version.as_deref())
                )),
            });
        }
        std::fs::create_dir_all(&install_dir).map_err(|e| RuntimeError::Download {
            url: String::new(),
            expected_sha: None,
            actual_sha: None,
            cause: format!(
                "npm install: create install dir {}: {e}",
                install_dir.display()
            ),
        })?;
        let _lock = acquire_install_lock(&install_dir)?;
        let mut pkg_refs = vec![npm_pkg_ref(package, version.as_deref())];
        pkg_refs.extend(secondary.iter().cloned());
        run_npm_install(&pkg_refs, &install_dir)?;
        let exe = npm_bin_path(&install_dir, bin_rel).ok_or_else(|| RuntimeError::Download {
            url: String::new(),
            expected_sha: None,
            actual_sha: None,
            cause: format!(
                "npm install {package}: bin_rel `{bin_rel}` missing under node_modules/.bin after install"
            ),
        })?;
        Ok(InstallOutcome::Ready(Launch::Process {
            exe,
            args: npm_args.clone().unwrap_or_default(),
        }))
    }
}

/// uvx 启动器（§2.5，与 DownloadInstaller 同构形态；无安装步骤——uv 自管缓存）。
/// 只做 `uvx` PATH 解析 + 启动参数组装；PATH 无 `uvx` → NotInstalled + hint。
pub struct UvxInstaller;

impl UvxInstaller {
    pub fn install(
        &self,
        ctx: &InstallCtx,
        spec: &InstallSpec,
    ) -> Result<InstallOutcome, RuntimeError> {
        let (package, version, entrypoint, extra) = match &spec.kind {
            InstallKind::Uvx {
                package,
                version,
                entrypoint,
                args,
            } => (package, version, entrypoint, args),
            _ => return Err(wrong_kind(spec)),
        };
        let _ = ctx; // uvx 无安装步骤，ctx 预留（未来 uv 环境探测/版本 pin 用）
        let Some(exe) = find_on_path("uvx", std::env::var_os("PATH").as_deref()) else {
            return Ok(InstallOutcome::NotInstalled {
                hint: "missing runtime `uvx`: install uv (https://docs.astral.sh/uv/getting-started/installation/) and ensure it is on PATH"
                    .to_string(),
                install_cmd: Some("pip install uv".to_string()),
            });
        };
        Ok(InstallOutcome::Ready(Launch::Process {
            exe,
            args: uvx_launch_args(package, version.as_deref(), entrypoint, extra.as_deref()),
        }))
    }
}

/// `pkg` / `pkg@ver`（npm install 目标引用，纯函数；ls-registry 映射层同用）。
pub fn npm_pkg_ref(package: &str, version: Option<&str>) -> String {
    match version {
        Some(v) => format!("{package}@{v}"),
        None => package.to_string(),
    }
}

/// `["--from", <pkg>[==<ver>], entrypoint, ...extra]`（uvx 启动参数，纯函数）。
fn uvx_launch_args(
    package: &str,
    version: Option<&str>,
    entrypoint: &str,
    extra: Option<&[String]>,
) -> Vec<String> {
    let from = match version {
        Some(v) => format!("{package}=={v}"),
        None => package.to_string(),
    };
    let mut args = vec!["--from".to_string(), from, entrypoint.to_string()];
    args.extend(extra.unwrap_or_default().iter().cloned());
    args
}

/// `node_modules/.bin/<bin_rel>` 解析。Windows 下 npm 生成 `.cmd` shim（裸名是 sh
/// 脚本，CreateProcess 无法执行），严格只认 `.cmd`；Unix 用裸名。
pub fn npm_bin_path(install_dir: &Path, bin_rel: &str) -> Option<PathBuf> {
    let bin_dir = install_dir.join("node_modules").join(".bin");
    #[cfg(windows)]
    {
        let shim = bin_dir.join(format!("{bin_rel}.cmd"));
        if shim.is_file() {
            return Some(shim);
        }
        // 无 .cmd → 视为未装（裸名 sh 脚本 spawn 不了，不能当命中）。
        None
    }
    #[cfg(unix)]
    {
        let raw = bin_dir.join(bin_rel);
        raw.is_file().then_some(raw)
    }
}

/// PATH 顺序查找可执行。ls-runtime 精简版（ls-adapters::which_path 有 UNC 清洗，
/// 本 crate 无该依赖故独立实现）；uvx 官方分发仅 `uvx.exe`（Windows）/`uvx` 两形态。
fn find_on_path(name: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var?) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{name}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

/// 同步 `npm install --prefix <dir> <pkg_refs...>`（secondary 伴随包同一次 install 装齐）。
fn run_npm_install(package_refs: &[String], dir: &Path) -> Result<(), RuntimeError> {
    let program = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let mut args = vec![
        "install".to_string(),
        "--prefix".to_string(),
        dir.to_string_lossy().to_string(),
    ];
    args.extend(package_refs.iter().cloned());
    run_pkg_cmd(
        program,
        &args,
        dir,
        "npm",
        "install Node.js (https://nodejs.org) and ensure `npm` is on PATH",
    )
}

/// 同步外部安装命令执行（npm install / dotnet tool install / gem install / git 共用）。
/// `cwd` = 命令工作目录（调用方负责先建目录；npm --prefix 与路径参数类命令 cwd 无关，
/// 源码构建类命令依赖 cwd=clone 根）。stdout → null（进度日志无人消费）；stderr 捕获
/// （失败时带出摘要）；600s 硬超时（std 无 wait_timeout，`try_wait` 轮询 + kill 兜底防挂死）。
/// program 不在 PATH → MissingRuntime；其余失败 → Download 变体（cause 带命令与 stderr 摘要）。
pub(crate) fn run_pkg_cmd(
    program: &str,
    args: &[String],
    cwd: &Path,
    missing_what: &str,
    missing_hint: &str,
) -> Result<(), RuntimeError> {
    let cmd_label = format!("{program} {}", args.join(" "));
    let mut child = std::process::Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RuntimeError::MissingRuntime {
                    what: missing_what.to_string(),
                    install_hint: missing_hint.to_string(),
                }
            } else {
                RuntimeError::Spawn {
                    cmd: cmd_label.clone(),
                    cause: e,
                }
            }
        })?;
    let deadline = std::time::Instant::now() + Duration::from_secs(600);
    let fail = |cause: String| RuntimeError::Download {
        url: String::new(),
        expected_sha: None,
        actual_sha: None,
        cause,
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                let mut err_buf = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut stderr, &mut err_buf);
                }
                let _ = child.wait();
                let detail = if err_buf.trim().is_empty() {
                    format!("exit {status:?}")
                } else {
                    // 摘要截断：安装器失败日志可达数十 KB，错误串只需定位线索。
                    let t = err_buf.trim();
                    if t.len() > 500 {
                        format!("{}…", &t[..500])
                    } else {
                        t.to_string()
                    }
                };
                return Err(fail(format!("{cmd_label}: {detail}")));
            }
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(format!("{cmd_label}: timed out after 600s (killed)")));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => {
                return Err(fail(format!("{cmd_label}: wait failed: {e}")));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::{Arch, Os};
    use crate::install::ArchiveKind;

    #[test]
    fn npm_pkg_ref_and_uvx_args_forms() {
        assert_eq!(npm_pkg_ref("some-ls", Some("1.2.3")), "some-ls@1.2.3");
        assert_eq!(npm_pkg_ref("some-ls", None), "some-ls", "无版本 = 不锁");
        // 锁版本形态：--from pkg==ver。
        assert_eq!(
            uvx_launch_args(
                "fake-ls",
                Some("0.9.0"),
                "fake-ls",
                Some(["-v".to_string(), "--stdio".to_string()].as_slice())
            ),
            vec!["--from", "fake-ls==0.9.0", "fake-ls", "-v", "--stdio"]
        );
        // 无版本 + 无附加参数形态。
        assert_eq!(
            uvx_launch_args("fake-ls", None, "fake-ls", None),
            vec!["--from", "fake-ls", "fake-ls"]
        );
    }

    #[test]
    fn npm_bin_path_resolves_shim() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("node_modules").join(".bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        assert!(npm_bin_path(dir.path(), "nope").is_none(), "未装 → None");
        std::fs::write(bin_dir.join("some-ls"), "").unwrap();
        #[cfg(windows)]
        {
            assert!(
                npm_bin_path(dir.path(), "some-ls").is_none(),
                "Windows 裸名是 sh 脚本，无 .cmd shim 不可 spawn → 不认"
            );
            std::fs::write(bin_dir.join("some-ls.cmd"), "").unwrap();
            assert_eq!(
                npm_bin_path(dir.path(), "some-ls").unwrap(),
                bin_dir.join("some-ls.cmd"),
                "Windows 优先 .cmd shim"
            );
        }
        #[cfg(unix)]
        {
            assert_eq!(
                npm_bin_path(dir.path(), "some-ls").unwrap(),
                bin_dir.join("some-ls")
            );
        }
    }

    #[test]
    fn find_on_path_matches_and_misses() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            find_on_path("uvx", Some(dir.path().as_os_str())).is_none(),
            "空目录未命中"
        );
        #[cfg(windows)]
        let probe = dir.path().join("uvx.exe");
        #[cfg(unix)]
        let probe = dir.path().join("uvx");
        std::fs::write(&probe, "").unwrap();
        let found = find_on_path("uvx", Some(dir.path().as_os_str())).unwrap();
        assert_eq!(found, probe);
        assert!(find_on_path("uvx", None).is_none(), "PATH 缺失 → None");
    }

    /// NpmInstaller 缓存命中短路：预置 node_modules/.bin 产物 → Ready，零触网。
    #[test]
    fn npm_installer_short_circuits_on_cached_bin() {
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = dir.path().join("my-ls/latest/node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        #[cfg(windows)]
        let bin = bin_dir.join("my-bin.cmd");
        #[cfg(unix)]
        let bin = bin_dir.join("my-bin");
        std::fs::write(&bin, "").unwrap();
        let ctx = InstallCtx {
            os: Os::Windows,
            arch: Arch::X86_64,
            auto_install: true,
            allow_unsigned_sha: false,
            cache_root: dir.path().to_path_buf(),
        };
        let spec = InstallSpec {
            id: "my-ls".into(),
            kind: InstallKind::Npm {
                package: "@fake/my-ls".into(),
                version: None,
                bin_rel: "my-bin".into(),
                npm_args: Some(vec!["--stdio".into()]),
                secondary: Vec::new(),
            },
            exec: vec![],
        };
        let out = NpmInstaller
            .install(&ctx, &spec)
            .expect("缓存命中应直接 Ready");
        let InstallOutcome::Ready(Launch::Process { exe, args }) = out else {
            panic!("应 Ready，实际 {out:?}");
        };
        assert_eq!(exe, bin);
        assert_eq!(args, vec!["--stdio"], "npm_args 应透传为启动参数");
        // wrong kind 路由守卫。
        let bad = InstallSpec {
            id: "my-ls".into(),
            kind: InstallKind::Download {
                version: "1".into(),
                url: "https://x".into(),
                sha256: String::new(),
                archive: ArchiveKind::Raw,
                strip_components: 0,
                bin_path: "b".into(),
                allowed_hosts: vec![],
            },
            exec: vec![],
        };
        let err = NpmInstaller.install(&ctx, &bad).unwrap_err();
        assert!(
            err.to_string().contains("wrong installer routed"),
            "err: {err}"
        );
    }

    /// UvxInstaller：PATH 无 uvx → NotInstalled + 安装 hint（不触网）。
    #[test]
    fn uvx_installer_reports_missing_runtime_without_uv() {
        // 本测试假设 CI/多数环境无 uvx；若本机装了 uv，此测试直接通过（命中即 Ready 分支）。
        let ctx = InstallCtx {
            os: Os::Windows,
            arch: Arch::X86_64,
            auto_install: true,
            allow_unsigned_sha: false,
            cache_root: tempfile::tempdir().unwrap().path().to_path_buf(),
        };
        let spec = InstallSpec {
            id: "uvx-ls".into(),
            kind: InstallKind::Uvx {
                package: "fake-ls".into(),
                version: Some("0.9.0".into()),
                entrypoint: "fake-ls".into(),
                args: None,
            },
            exec: vec![],
        };
        let out = UvxInstaller
            .install(&ctx, &spec)
            .expect("无安装步骤，不应 Err");
        match out {
            InstallOutcome::NotInstalled { hint, install_cmd } => {
                assert!(hint.contains("uvx"), "hint: {hint}");
                assert_eq!(install_cmd.as_deref(), Some("pip install uv"));
            }
            InstallOutcome::Ready(Launch::Process { exe, args }) => {
                // 本机有 uv：参数形态断言兜底（PATH 解析真值不可注入，见 find_on_path 单测）。
                assert!(exe.ends_with("uvx.exe") || exe.ends_with("uvx"));
                assert_eq!(args, vec!["--from", "fake-ls==0.9.0", "fake-ls"]);
            }
            other => panic!("意外 outcome: {other:?}"),
        }
    }
}
