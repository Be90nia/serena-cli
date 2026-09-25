//! 系统工具链托管安装器：dotnet tool（§2.5）/ gem（§2.6）/ 源码构建（设计 §0 非目标的
//! 受控放开，任务 Phase 3b）。与 install_pkg（npm/uvx）同构：unit struct + `install(&ctx, &spec)`。
//!
//! 完整性模型（design §2.9）：D/E 类信任官方包管理器（NuGet 签名包 / gem checksum +
//! 版本钉死）；源码构建锚 `pin`（tag/branch），无产物级 sha256（Δ A 类，如实记录）。

use std::path::{Path, PathBuf};

use crate::install::{
    InstallCtx, InstallKind, InstallOutcome, InstallSpec, Launch, acquire_install_lock, wrong_kind,
};
use crate::install_pkg::run_pkg_cmd;
use crate::process::RuntimeError;

/// dotnet tool 类安装器（上游 fsharp 适配器形态：`dotnet tool install --tool-path
/// {cache}/{id}/{version} <tool> [--version <ver>]`——装进自有缓存目录而非 `-g` 全局，
/// 按版本隔离、卸载即删目录）。产物 = `{dir}/<tool>[.exe]`。
pub struct DotnetInstaller;

impl DotnetInstaller {
    pub fn install(
        &self,
        ctx: &InstallCtx,
        spec: &InstallSpec,
    ) -> Result<InstallOutcome, RuntimeError> {
        let (tool, version, args) = match &spec.kind {
            InstallKind::Dotnet {
                tool,
                version,
                args,
            } => (tool, version, args),
            _ => return Err(wrong_kind(spec)),
        };
        let dir_name = version.clone().unwrap_or_else(|| "latest".to_string());
        let install_dir = ctx.cache_root.join(&spec.id).join(dir_name);
        if let Some(exe) = dotnet_tool_bin_path(&install_dir, tool) {
            return Ok(InstallOutcome::Ready(Launch::Process {
                exe,
                args: args.clone().unwrap_or_default(),
            }));
        }
        if !ctx.auto_install {
            return Ok(InstallOutcome::NotInstalled {
                hint: format!(
                    "dotnet tool `{tool}` not installed under {}",
                    install_dir.display()
                ),
                install_cmd: Some(dotnet_install_cmd(tool, version.as_deref(), &install_dir)),
            });
        }
        std::fs::create_dir_all(&install_dir).map_err(|e| {
            toolchain_err(&format!(
                "dotnet tool install: create install dir {}: {e}",
                install_dir.display()
            ))
        })?;
        let _lock = acquire_install_lock(&install_dir)?;
        let mut cmd_args = vec![
            "tool".to_string(),
            "install".to_string(),
            "--tool-path".to_string(),
            install_dir.to_string_lossy().to_string(),
            tool.clone(),
        ];
        if let Some(v) = version.as_deref() {
            cmd_args.push("--version".to_string());
            cmd_args.push(v.to_string());
        }
        run_pkg_cmd(
            "dotnet",
            &cmd_args,
            &install_dir,
            "dotnet",
            "install .NET SDK (https://dotnet.microsoft.com) and ensure `dotnet` is on PATH",
        )?;
        let exe = dotnet_tool_bin_path(&install_dir, tool).ok_or_else(|| {
            toolchain_err(&format!(
                "dotnet tool install {tool}: binary missing under {} after install",
                install_dir.display()
            ))
        })?;
        Ok(InstallOutcome::Ready(Launch::Process {
            exe,
            args: args.clone().unwrap_or_default(),
        }))
    }
}

/// `{dir}/<tool>[.exe]`（--tool-path 产物；Windows 为 `tool.exe`）。
pub fn dotnet_tool_bin_path(install_dir: &Path, tool: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let exe = install_dir.join(format!("{tool}.exe"));
        exe.is_file().then_some(exe)
    }
    #[cfg(unix)]
    {
        let exe = install_dir.join(tool);
        exe.is_file().then_some(exe)
    }
}

fn dotnet_install_cmd(tool: &str, version: Option<&str>, dir: &Path) -> String {
    let ver = version
        .map(|v| format!(" --version {v}"))
        .unwrap_or_default();
    format!(
        "dotnet tool install --tool-path {} {tool}{ver}",
        dir.display()
    )
}

/// gem 类安装器：`gem install --user-install --bindir {cache}/{id}/{version}/bin
/// <gem> [-v <ver>]`。gem 库体归 RubyGems 用户目录管（--user-install），可执行物经
/// `--bindir` 钉进本工具缓存——规避 `~/.gem/ruby/<ruby版本>/bin` 的版本相关布局，
/// 探测路径确定。产物 = `{bindir}/<bin_rel>[.bat/.cmd]`。
pub struct GemInstaller;

impl GemInstaller {
    pub fn install(
        &self,
        ctx: &InstallCtx,
        spec: &InstallSpec,
    ) -> Result<InstallOutcome, RuntimeError> {
        let (gem, version, bin_rel, args) = match &spec.kind {
            InstallKind::Gem {
                gem,
                version,
                bin_rel,
                args,
            } => (gem, version, bin_rel, args),
            _ => return Err(wrong_kind(spec)),
        };
        let dir_name = version.clone().unwrap_or_else(|| "latest".to_string());
        let install_dir = ctx.cache_root.join(&spec.id).join(dir_name);
        let bindir = install_dir.join("bin");
        if let Some(exe) = gem_bin_path(&bindir, bin_rel) {
            return Ok(InstallOutcome::Ready(Launch::Process {
                exe,
                args: args.clone().unwrap_or_default(),
            }));
        }
        if !ctx.auto_install {
            return Ok(InstallOutcome::NotInstalled {
                hint: format!("gem `{gem}` not installed (bindir {})", bindir.display()),
                install_cmd: Some(gem_install_cmd(gem, version.as_deref(), &bindir)),
            });
        }
        std::fs::create_dir_all(&bindir).map_err(|e| {
            toolchain_err(&format!(
                "gem install: create bindir {}: {e}",
                bindir.display()
            ))
        })?;
        let _lock = acquire_install_lock(&install_dir)?;
        run_pkg_cmd(
            "gem",
            &gem_install_args(gem, version.as_deref(), &bindir),
            &install_dir,
            "gem",
            "install Ruby (https://www.ruby-lang.org) and ensure `gem` is on PATH",
        )?;
        let exe = gem_bin_path(&bindir, bin_rel).ok_or_else(|| {
            toolchain_err(&format!(
                "gem install {gem}: bin_rel `{bin_rel}` missing under {} after install",
                bindir.display()
            ))
        })?;
        Ok(InstallOutcome::Ready(Launch::Process {
            exe,
            args: args.clone().unwrap_or_default(),
        }))
    }
}

/// `["install", "--user-install", "--bindir", <bindir>, <gem>[, "-v", <ver>]]`。
fn gem_install_args(gem: &str, version: Option<&str>, bindir: &Path) -> Vec<String> {
    let mut args = vec![
        "install".to_string(),
        "--user-install".to_string(),
        "--bindir".to_string(),
        bindir.to_string_lossy().to_string(),
        gem.to_string(),
    ];
    if let Some(v) = version {
        args.push("-v".to_string());
        args.push(v.to_string());
    }
    args
}

fn gem_install_cmd(gem: &str, version: Option<&str>, bindir: &Path) -> String {
    format!("gem {}", gem_install_args(gem, version, bindir).join(" "))
}

/// gem --bindir 产物解析。Windows RubyGems 生成 `.cmd`/`.bat` 包装（裸名是 ruby 脚本，
/// CreateProcess 无法执行），优先 `.cmd` → `.bat`；Unix 用裸名。
pub fn gem_bin_path(bindir: &Path, bin_rel: &str) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        for ext in [".cmd", ".bat"] {
            let shim = bindir.join(format!("{bin_rel}{ext}"));
            if shim.is_file() {
                return Some(shim);
            }
        }
        None
    }
    #[cfg(unix)]
    {
        let raw = bindir.join(bin_rel);
        raw.is_file().then_some(raw)
    }
}

/// 源码构建类安装器：`git clone --depth 1 <repo> src`（pin 在场再加 `--branch <pin>`），
/// 在 clone 根执行单步 `build_cmd`，产物 = `src/<bin_rel>`。
/// ponytail: pin 仅支持 tag/branch（`--branch` 原生语义）；按 SHA 拉取等有真实条目
/// 需要时再加 fetch-by-sha。clone 成功但 build 失败时保留 clone——重试跳过 clone 直接重跑
/// build（cargo/shards/nix build 均幂等）。
pub struct SourceInstaller;

impl SourceInstaller {
    pub fn install(
        &self,
        ctx: &InstallCtx,
        spec: &InstallSpec,
    ) -> Result<InstallOutcome, RuntimeError> {
        let (repo, pin, build_cmd, bin_rel) = match &spec.kind {
            InstallKind::Source {
                repo,
                pin,
                build_cmd,
                bin_rel,
            } => (repo, pin, build_cmd, bin_rel),
            _ => return Err(wrong_kind(spec)),
        };
        let dir_name = pin.clone().unwrap_or_else(|| "default".to_string());
        let install_dir = ctx.cache_root.join(&spec.id).join(dir_name);
        let clone_dir = install_dir.join("src");
        let exe = clone_dir.join(bin_rel);
        if exe.is_file() {
            return Ok(InstallOutcome::Ready(Launch::Process {
                exe,
                args: Vec::new(),
            }));
        }
        if !ctx.auto_install {
            return Ok(InstallOutcome::NotInstalled {
                hint: format!(
                    "source build of `{repo}` not present under {}",
                    install_dir.display()
                ),
                install_cmd: Some(format!(
                    "git clone {repo} && (cd src && {})",
                    build_cmd.join(" ")
                )),
            });
        }
        std::fs::create_dir_all(&install_dir).map_err(|e| {
            toolchain_err(&format!(
                "source build: create install dir {}: {e}",
                install_dir.display()
            ))
        })?;
        let _lock = acquire_install_lock(&install_dir)?;
        if !clone_dir.join(".git").exists() {
            let mut args = vec!["clone".to_string(), "--depth".to_string(), "1".to_string()];
            if let Some(p) = pin.as_deref() {
                args.push("--branch".to_string());
                args.push(p.to_string());
            }
            args.push(repo.clone());
            args.push(clone_dir.to_string_lossy().to_string());
            run_pkg_cmd(
                "git",
                &args,
                &install_dir,
                "git",
                "install git (https://git-scm.com) and ensure it is on PATH",
            )?;
        }
        let (program, build_args) = build_cmd
            .split_first()
            .ok_or_else(|| toolchain_err("source build: empty build_cmd (InvalidSpec)"))?;
        run_pkg_cmd(
            program,
            build_args,
            &clone_dir,
            program,
            "check the LS entry's build toolchain requirements",
        )?;
        if !exe.is_file() {
            return Err(toolchain_err(&format!(
                "source build: bin_rel `{bin_rel}` missing under {} after build",
                clone_dir.display()
            )));
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

/// 非下载类安装失败的统一 Download 变体形态（同 npm/install.lock 先例：空 url）。
fn toolchain_err(cause: &str) -> RuntimeError {
    RuntimeError::Download {
        url: String::new(),
        expected_sha: None,
        actual_sha: None,
        cause: cause.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deps::{Arch, Os};

    fn ctx(dir: &Path, auto_install: bool) -> InstallCtx {
        InstallCtx {
            os: Os::Windows,
            arch: Arch::X86_64,
            auto_install,
            allow_unsigned_sha: false,
            cache_root: dir.to_path_buf(),
        }
    }

    fn kind_spec(kind: InstallKind) -> InstallSpec {
        InstallSpec {
            id: "my-ls".into(),
            kind,
            exec: vec![],
        }
    }

    #[test]
    fn dotnet_installer_short_circuits_on_cached_tool() {
        let dir = tempfile::tempdir().unwrap();
        // kind_spec 助手的 spec id 固定 "my-ls"——缓存目录必须一致。
        let install_dir = dir.path().join("my-ls/0.83.0");
        std::fs::create_dir_all(&install_dir).unwrap();
        #[cfg(windows)]
        let bin = install_dir.join("fsautocomplete.exe");
        #[cfg(unix)]
        let bin = install_dir.join("fsautocomplete");
        std::fs::write(&bin, "").unwrap();
        let out = DotnetInstaller
            .install(
                &ctx(dir.path(), true),
                &kind_spec(InstallKind::Dotnet {
                    tool: "fsautocomplete".into(),
                    version: Some("0.83.0".into()),
                    args: Some(vec!["--stdio".into()]),
                }),
            )
            .expect("缓存命中应直接 Ready");
        let InstallOutcome::Ready(Launch::Process { exe, args }) = out else {
            panic!("应 Ready，实际 {out:?}");
        };
        assert_eq!(exe, bin);
        assert_eq!(args, vec!["--stdio"], "args 应透传为启动参数");
    }

    #[test]
    fn dotnet_install_cmd_forms_with_and_without_version() {
        let with = dotnet_install_cmd("fsautocomplete", Some("0.83.0"), Path::new("/t"));
        assert_eq!(
            with,
            "dotnet tool install --tool-path /t fsautocomplete --version 0.83.0"
        );
        let without = dotnet_install_cmd("csharp-ls", None, Path::new("/t"));
        assert_eq!(
            without, "dotnet tool install --tool-path /t csharp-ls",
            "无版本 = latest，不带 --version"
        );
    }

    #[test]
    fn dotnet_installer_reports_not_installed_without_auto() {
        let dir = tempfile::tempdir().unwrap();
        let out = DotnetInstaller
            .install(
                &ctx(dir.path(), false),
                &kind_spec(InstallKind::Dotnet {
                    tool: "fsautocomplete".into(),
                    version: Some("0.83.0".into()),
                    args: None,
                }),
            )
            .expect("未装 + auto_install=false 应 NotInstalled 而非 Err");
        let InstallOutcome::NotInstalled { hint, install_cmd } = out else {
            panic!("应 NotInstalled，实际 {out:?}");
        };
        assert!(hint.contains("fsautocomplete"), "hint: {hint}");
        let cmd = install_cmd.expect("install_cmd 应在");
        assert!(
            cmd.contains("dotnet tool install --tool-path"),
            "cmd: {cmd}"
        );
        assert!(cmd.contains("--version 0.83.0"), "cmd: {cmd}");
        // wrong kind 路由守卫。
        let bad = kind_spec(InstallKind::PathOnly {
            binary_name: "x".into(),
            install_hint: "y".into(),
        });
        let err = DotnetInstaller
            .install(&ctx(dir.path(), true), &bad)
            .unwrap_err();
        assert!(
            err.to_string().contains("wrong installer routed"),
            "err: {err}"
        );
    }

    #[test]
    fn gem_bin_path_resolves_shim() {
        let dir = tempfile::tempdir().unwrap();
        let bindir = dir.path().join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        assert!(gem_bin_path(&bindir, "ruby-lsp").is_none(), "未装 → None");
        std::fs::write(bindir.join("ruby-lsp"), "").unwrap();
        #[cfg(windows)]
        {
            assert!(
                gem_bin_path(&bindir, "ruby-lsp").is_none(),
                "Windows 裸名是 ruby 脚本，无 shim 不可 spawn → 不认"
            );
            std::fs::write(bindir.join("ruby-lsp.bat"), "").unwrap();
            assert_eq!(
                gem_bin_path(&bindir, "ruby-lsp").unwrap(),
                bindir.join("ruby-lsp.bat"),
                "无 .cmd 时取 .bat"
            );
            std::fs::write(bindir.join("ruby-lsp.cmd"), "").unwrap();
            assert_eq!(
                gem_bin_path(&bindir, "ruby-lsp").unwrap(),
                bindir.join("ruby-lsp.cmd"),
                "Windows 优先 .cmd"
            );
        }
        #[cfg(unix)]
        {
            assert_eq!(
                gem_bin_path(&bindir, "ruby-lsp").unwrap(),
                bindir.join("ruby-lsp")
            );
        }
    }

    /// GemInstaller 缓存命中短路：预置 bindir 产物 → Ready + args 透传，零触网。
    #[test]
    fn gem_installer_short_circuits_on_cached_bin() {
        let dir = tempfile::tempdir().unwrap();
        let bindir = dir.path().join("my-ls/latest/bin");
        std::fs::create_dir_all(&bindir).unwrap();
        #[cfg(windows)]
        let bin = bindir.join("ruby-lsp.cmd");
        #[cfg(unix)]
        let bin = bindir.join("ruby-lsp");
        std::fs::write(&bin, "").unwrap();
        let out = GemInstaller
            .install(
                &ctx(dir.path(), true),
                &kind_spec(InstallKind::Gem {
                    gem: "ruby-lsp".into(),
                    version: None,
                    bin_rel: "ruby-lsp".into(),
                    args: Some(vec!["--stdio".into()]),
                }),
            )
            .expect("缓存命中应直接 Ready");
        let InstallOutcome::Ready(Launch::Process { exe, args }) = out else {
            panic!("应 Ready，实际 {out:?}");
        };
        assert_eq!(exe, bin);
        assert_eq!(args, vec!["--stdio"]);
    }

    #[test]
    fn gem_installer_not_installed_and_args_forms() {
        let dir = tempfile::tempdir().unwrap();
        let out = GemInstaller
            .install(
                &ctx(dir.path(), false),
                &kind_spec(InstallKind::Gem {
                    gem: "solargraph".into(),
                    version: Some("0.51.1".into()),
                    bin_rel: "solargraph".into(),
                    args: Some(vec!["stdio".into()]),
                }),
            )
            .expect("未装 + auto_install=false 应 NotInstalled 而非 Err");
        let InstallOutcome::NotInstalled { hint, install_cmd } = out else {
            panic!("应 NotInstalled，实际 {out:?}");
        };
        assert!(hint.contains("solargraph"), "hint: {hint}");
        let cmd = install_cmd.expect("install_cmd 应在");
        assert!(cmd.contains("gem install --user-install"), "cmd: {cmd}");
        assert!(cmd.contains("--bindir"), "cmd: {cmd}");
        assert!(cmd.contains("solargraph -v 0.51.1"), "cmd: {cmd}");
        // gem_install_args 无版本形态：不带 -v。
        let no_ver = gem_install_args("ruby-lsp", None, Path::new("/b"));
        assert_eq!(
            no_ver,
            vec!["install", "--user-install", "--bindir", "/b", "ruby-lsp"]
        );
    }

    /// SourceInstaller 全链路（无网络）：本地 git 仓库作 clone 源 + 幂等"build"
    /// 命令落 bin_rel 产物。git 不在场 → MissingRuntime（不算失败）。
    #[test]
    fn source_installer_clones_local_repo_and_builds() {
        let src_repo = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(src_repo.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .expect("git 应可执行");
            assert!(status.success(), "git {args:?} 失败");
        };
        git(&["init", "-q"]);
        std::fs::write(src_repo.path().join("README"), "x").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        let repo_url = src_repo.path().to_string_lossy().to_string();

        let cache = tempfile::tempdir().unwrap();
        // "build" 步骤跨平台落一个文件到 bin_rel（git init 在任意平台可用）。
        #[cfg(windows)]
        let build_cmd = vec![
            "cmd".to_string(),
            "/c".to_string(),
            "mkdir bin 2>nul & type nul > bin\\tool.txt".to_string(),
        ];
        #[cfg(unix)]
        let build_cmd = vec![
            "sh".to_string(),
            "-c".to_string(),
            "mkdir -p bin && : > bin/tool.txt".to_string(),
        ];
        let spec = kind_spec(InstallKind::Source {
            repo: repo_url.clone(),
            pin: None,
            build_cmd,
            bin_rel: "bin/tool.txt".into(),
        });
        match SourceInstaller.install(&ctx(cache.path(), true), &spec) {
            Ok(InstallOutcome::Ready(Launch::Process { exe, args })) => {
                assert!(exe.is_file(), "build 产物应落地: {}", exe.display());
                assert!(args.is_empty());
                // 二调：缓存命中短路（Ready 且零重跑）。
                let out2 = SourceInstaller
                    .install(&ctx(cache.path(), true), &spec)
                    .unwrap();
                assert!(matches!(out2, InstallOutcome::Ready(_)), "{out2:?}");
            }
            Ok(other) => panic!("应 Ready，实际 {other:?}"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("missing runtime `git`") || msg.contains("MissingRuntime"),
                    "git 在场却失败，须排查: {msg}"
                );
                eprintln!("SKIP: git not available on this machine ({msg})");
            }
        }
    }

    #[test]
    fn source_installer_pin_routes_to_versioned_dir() {
        let dir = tempfile::tempdir().unwrap();
        let out = SourceInstaller
            .install(
                &ctx(dir.path(), false),
                &kind_spec(InstallKind::Source {
                    repo: "https://github.com/elbywan/crystalline".into(),
                    pin: Some("v0.2.0".into()),
                    build_cmd: vec!["shards".into(), "build".into()],
                    bin_rel: "bin/crystalline".into(),
                }),
            )
            .expect("未装 + auto_install=false 应 NotInstalled 而非 Err");
        let InstallOutcome::NotInstalled { hint, install_cmd } = out else {
            panic!("应 NotInstalled，实际 {out:?}");
        };
        // pin → 缓存目录名用 pin（非 default），探测/落位按版本隔离。
        assert!(hint.contains("v0.2.0"), "hint 应含 pin 版本目录: {hint}");
        assert!(
            !hint.contains("default"),
            "pin 在场不应落 default 目录: {hint}"
        );
        assert!(
            install_cmd
                .unwrap()
                .contains("git clone https://github.com/elbywan/crystalline")
        );
    }

    #[test]
    fn source_installer_reports_not_installed_and_wrong_kind() {
        let dir = tempfile::tempdir().unwrap();
        let out = SourceInstaller
            .install(
                &ctx(dir.path(), false),
                &kind_spec(InstallKind::Source {
                    repo: "https://github.com/nix-community/nixd".into(),
                    pin: None,
                    build_cmd: vec!["nix".into(), "build".into()],
                    bin_rel: "result/bin/nixd".into(),
                }),
            )
            .expect("未装 + auto_install=false 应 NotInstalled 而非 Err");
        let InstallOutcome::NotInstalled { hint, install_cmd } = out else {
            panic!("应 NotInstalled，实际 {out:?}");
        };
        assert!(hint.contains("nixd"), "hint: {hint}");
        let cmd = install_cmd.expect("install_cmd 应在");
        assert!(
            cmd.contains("git clone https://github.com/nix-community/nixd"),
            "cmd: {cmd}"
        );
        // wrong kind 路由守卫。
        let bad = kind_spec(InstallKind::Uvx {
            package: "x".into(),
            version: None,
            entrypoint: "x".into(),
            args: None,
        });
        let err = SourceInstaller
            .install(&ctx(dir.path(), true), &bad)
            .unwrap_err();
        assert!(
            err.to_string().contains("wrong installer routed"),
            "err: {err}"
        );
    }
}
