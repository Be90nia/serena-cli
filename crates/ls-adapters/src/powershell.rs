//! PowerShell (PowerShellEditorServices) 适配器（Wave 2，download 型 T2）。
//!
//! ↖ mirror: oraios/serena@43ae021 `solidlsp/language_servers/powershell_language_server.py`
//!
//! PSES 是微软官方 PowerShell LSP（vscode-powershell 内核）。启动形态（上游
//! `_create_launch_command`）：单条 `-Command` 字符串调 Start-EditorServices.ps1，
//! `-Stdio` 走 stdio 传输。
//!
//! ## 安装与启动分工（与 bash/json npm 型同构，触网归 install 命令）
//!
//! - 安装：`serena-cli install powershell` → servers.toml `[servers.powershell]`
//!   （download v4.4.0 zip，sha256 钉死 → `{cache}/powershell/4.4.0/`）。zip 顶层
//!   即包体：`PowerShellEditorServices/`（含启动脚本）、`PSScriptAnalyzer/`、
//!   `PSReadLine/`。
//! - 启动：pwsh 探测（PATH → 常见安装位置，↖ mirror `_get_pwsh_path`）+
//!   缓存脚本探测 + 会话级临时 log/session-details 文件。
//!
//! ## 就绪（无 documentSymbol 快路径）
//!
//! PSES 用动态能力注册（`client/registerCapability`），initialize 响应基本为空。
//! 上游等 registerCapability(documentSymbol) 或 window/logMessage ready 信号
//! （10s 超时兜底）；本侧 client 层不消费 server→client 请求内容，与 jdtls/bash
//! 先例一致走轻探针（`workspace/configuration` 能应答 = 事件循环活着）——
//! PowerShell 是脚本语言无 workspace 索引期，documentSymbol/hover 在 didOpen 后
//! 即时可答，索引等待交给工具层 `wait_for_index` 默认探针。
//!
//! ponytail: PSScriptAnalyzer 附加模块不装（与 bash 的 ShellCheck 同款取舍：
//! 无它仍有 PSES 内建解析诊断，overview/hover/diagnostics 三关不受影响）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::install::default_cache_root;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

/// 就绪探针超时（无索引期，短超时足够；对齐 bash）。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// 安装缓存 id + 版本 pin（对齐 servers.toml [servers.powershell]，禁随意改）。
const CACHE_ID: &str = "powershell";
const CACHE_VERSION: &str = "4.4.0";

/// zip 解压后的启动脚本相对路径（= servers.toml bin_path；↖ mirror `_get_pses_path`）。
const START_SCRIPT_REL: &str = "PowerShellEditorServices/Start-EditorServices.ps1";

#[derive(Debug, Default, Clone, Copy)]
pub struct PowerShellAdapter;

/// pwsh 探测：PATH → 常见安装位置（↖ mirror `_get_pwsh_path`）。
/// 都未命中 → `not_installed_error`（session_for 按消息归类 LS_NOT_INSTALLED）。
fn resolve_pwsh() -> anyhow::Result<PathBuf> {
    if let Some(pwsh) = which_no_unc("pwsh") {
        return Ok(pwsh);
    }
    // PATH 无 → 常见安装位置（↖ mirror `_get_pwsh_path` 三平台 fallback 表）。
    let pf = std::env::var_os("PROGRAMFILES").unwrap_or_else(|| "C:\\Program Files".into());
    let mut candidates = vec![
        PathBuf::from(&pf).join("PowerShell/7/pwsh.exe"),
        PathBuf::from(&pf).join("PowerShell/7-preview/pwsh.exe"),
        PathBuf::from("/usr/local/bin/pwsh"),
        PathBuf::from("/usr/bin/pwsh"),
        PathBuf::from("/opt/homebrew/bin/pwsh"),
        PathBuf::from("/opt/microsoft/powershell/7/pwsh"),
    ];
    if let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
    {
        candidates.push(home.join("AppData/Local/Microsoft/PowerShell/pwsh.exe"));
        candidates.push(home.join(".dotnet/tools/pwsh"));
    }
    if let Some(found) = candidates.into_iter().find(|c| c.is_file()) {
        return Ok(found);
    }
    Err(not_installed_error(
        "pwsh",
        "install PowerShell 7+ from https://github.com/PowerShell/PowerShell (`pwsh` on PATH); \
         the PowerShellEditorServices distribution itself is installed by `serena-cli install powershell`",
    ))
}

/// 缓存产物探测：`{cache}/{id}/{version}/{START_SCRIPT_REL}`（= servers.toml
/// download 布局）。未装 → `not_installed_error`（不触网，安装归 install 命令）。
fn resolve_start_script() -> anyhow::Result<PathBuf> {
    let script = default_cache_root()
        .join(CACHE_ID)
        .join(CACHE_VERSION)
        .join(START_SCRIPT_REL);
    if script.is_file() {
        return Ok(script);
    }
    Err(not_installed_error(
        "powershell",
        "run `serena-cli install powershell` (downloads PowerShellEditorServices v4.4.0, ~50MB)",
    ))
}

/// 会话级 PSES 文件（log + session-details）：临时目录下按 (pid, 进程序数) 唯一命名。
/// ↖ mirror 上游 `{temp}/solidlsp_pses/`（固定名多实例互踩；本侧唯一名规避）。
/// 用完不主动删——LS 进程持句柄期间不可删，留给 OS temp 清理。
fn pses_session_files() -> (PathBuf, PathBuf) {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = format!(
        "{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let dir = std::env::temp_dir().join("serena-powershell");
    (
        dir.join(format!("pses-{unique}.log")),
        dir.join(format!("session-{unique}.json")),
    )
}

/// 拼 PSES 启动 argv（↖ mirror `_create_launch_command`）：
/// `[pwsh, -NoLogo, -NoProfile, -Command, "& '<script>' ... -Stdio"]`。
/// 路径一律单引号包裹（Windows temp/用户目录常含空格；pwsh -Command 再 tokenize）。
fn launch_command(
    pwsh: &Path,
    script: &Path,
    bundled: &Path,
    log: &Path,
    session: &Path,
) -> Vec<String> {
    let command = format!(
        "& '{}' -HostName SolidLSP -HostProfileId solidlsp -HostVersion 1.0.0 \
         -BundledModulesPath '{}' -LogPath '{}' -LogLevel Information \
         -SessionDetailsPath '{}' -Stdio",
        script.display(),
        bundled.display(),
        log.display(),
        session.display(),
    );
    vec![
        pwsh.to_string_lossy().into_owned(),
        "-NoLogo".to_string(),
        "-NoProfile".to_string(),
        "-Command".to_string(),
        command,
    ]
}

#[async_trait]
impl LanguageServerAdapter for PowerShellAdapter {
    fn id(&self) -> &'static str {
        "powershell"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::PowerShell];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        // 上游 _setup_runtime_dependency 顺序：先 pwsh（硬前置）→ PSES 缓存。
        // bundled_modules_path = 解压根（脚本父目录的父 = install_dir）：Start-EditorServices.ps1
        // 以 $PSScriptRoot 加载 PSES 本体（v4.4.0 源码 :118），-BundledModulesPath 只供
        // 运行时解析 zip 内置的 PSScriptAnalyzer/PSReadLine —— 语义即上游注释
        // "the directory containing PowerShellEditorServices"。上游 py 实传
        // `pses_path.parent`（模块目录自身）使其 PSScriptAnalyzer 探测永 miss、
        // 触发错位 Save-Module —— mirror 抄注释语义不抄该 bug。
        let pwsh = resolve_pwsh()?;
        let script = resolve_start_script()?;
        let bundled = script
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| default_cache_root().join(CACHE_ID).join(CACHE_VERSION));
        let (log, session_details) = pses_session_files();
        Ok(LaunchInfo {
            cmd: launch_command(&pwsh, &script, &bundled, &log, &session_details)
                .into_iter()
                .map(Into::into)
                .collect(),
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, _base: &mut InitializeParams) {
        // 上游 _create_base_initialize_params 声明的能力（didSave/hover markdown/
        // formatting 等）默认 params 已覆盖。刻意不声明 hierarchicalDocumentSymbolSupport
        // —— PSES 将回退平铺 SymbolInformation[]，与 supervisor 符号层兼容。
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // 轻探针捕获启动即崩溃（pwsh 缺参 / PSES 模块损坏时进程秒退）；
        // PSES 无索引等待语义（见模块注释）。
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "workspace/configuration",
                json!({"items": [{"section": "powershell"}]}),
                READY_PROBE_TIMEOUT,
            )
            .await;
        let _ = probe;
        Ok(())
    }

    fn request_hooks(&self) -> RequestHooks {
        RequestHooks::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// launch_command argv 形态：pwsh + -NoLogo/-NoProfile/-Command + 单条
    /// -Command 字符串含 & 调用与全部 PSES 参数 + -Stdio 收尾。
    #[test]
    fn launch_command_shape() {
        let cmd = launch_command(
            Path::new("C:/Program Files/PowerShell/7/pwsh.exe"),
            Path::new(
                "C:/cache/powershell/4.4.0/PowerShellEditorServices/Start-EditorServices.ps1",
            ),
            Path::new("C:/cache/powershell/4.4.0/PowerShellEditorServices"),
            Path::new("C:/Temp/pses.log"),
            Path::new("C:/Temp/session.json"),
        );
        assert_eq!(cmd[0], "C:/Program Files/PowerShell/7/pwsh.exe");
        assert_eq!(cmd[1], "-NoLogo");
        assert_eq!(cmd[2], "-NoProfile");
        assert_eq!(cmd[3], "-Command");
        // & 调用 + 参数全在单条 -Command 字符串内（pwsh -Command 语义）。
        let c = &cmd[4];
        assert!(c.starts_with("& '"), "须以 & 调用启动脚本: {c}");
        assert!(c.contains("-HostName SolidLSP"), "{c}");
        assert!(c.contains("-HostProfileId solidlsp"), "{c}");
        assert!(c.contains("-HostVersion 1.0.0"), "{c}");
        assert!(c.contains("-BundledModulesPath '"), "{c}");
        assert!(c.contains("-LogPath '"), "{c}");
        assert!(c.contains("-LogLevel Information"), "{c}");
        assert!(c.contains("-SessionDetailsPath '"), "{c}");
        assert!(c.ends_with("-Stdio"), "{c}");
    }

    /// 含空格的路径必须被单引号包裹（pwsh -Command tokenize 破坏防护）。
    #[test]
    fn launch_command_quotes_paths_with_spaces() {
        let cmd = launch_command(
            Path::new("/usr/bin/pwsh"),
            Path::new("/c/My Cache/PowerShellEditorServices/Start-EditorServices.ps1"),
            Path::new("/c/My Cache/PowerShellEditorServices"),
            Path::new("/tmp/my logs/pses.log"),
            Path::new("/tmp/my logs/session.json"),
        );
        let c = &cmd[4];
        assert!(c.contains("& '/c/My Cache/"), "{c}");
        assert!(c.contains("-BundledModulesPath '/c/My Cache/"), "{c}");
        assert!(c.contains("-LogPath '/tmp/my logs/"), "{c}");
        assert!(c.contains("-SessionDetailsPath '/tmp/my logs/"), "{c}");
    }

    /// 会话文件唯一性：两次调用不撞名（多 session 并发互踩防护）。
    #[test]
    fn pses_session_files_are_unique() {
        let (log1, sd1) = pses_session_files();
        let (log2, sd2) = pses_session_files();
        assert_ne!(log1, log2, "log 文件名必须唯一");
        assert_ne!(sd1, sd2, "session 文件名必须唯一");
        assert!(log1.starts_with(std::env::temp_dir()), "必须落在临时目录");
    }

    /// 缺失路径：pwsh 与缓存产物都无 → not_installed_error（LS_NOT_INSTALLED 语义），
    /// 无 panic。真机可能有 pwsh/缓存命中干扰，全部注入屏蔽。
    #[tokio::test]
    async fn launch_errors_when_pwsh_and_cache_missing() {
        let cache_dir = tempfile::tempdir().unwrap();
        let empty_dir = tempfile::tempdir().unwrap();
        let path_original = std::env::var_os("PATH").unwrap_or_default();
        let home_key = if cfg!(windows) {
            "LOCALAPPDATA"
        } else {
            "HOME"
        };
        let home_original = std::env::var_os(home_key);
        let programfiles_original = std::env::var_os("PROGRAMFILES");
        let pf_dir = tempfile::tempdir().unwrap();
        // SAFETY: 单线程 tokio test 内注入 + 末尾还原；真机 pwsh 位置表命中被
        // PROGRAMFILES/tempdir 屏蔽。
        unsafe {
            std::env::set_var("PATH", empty_dir.path());
            std::env::set_var(home_key, cache_dir.path());
            std::env::set_var("PROGRAMFILES", pf_dir.path());
        }
        let result = PowerShellAdapter
            .launch_info(&ProjectCtx {
                project_root: std::env::temp_dir(),
            })
            .await;
        unsafe {
            std::env::set_var("PATH", path_original);
            match home_original {
                Some(v) => std::env::set_var(home_key, v),
                None => std::env::remove_var(home_key),
            }
            match programfiles_original {
                Some(v) => std::env::set_var("PROGRAMFILES", v),
                None => std::env::remove_var("PROGRAMFILES"),
            }
        }
        let err = result.expect_err("pwsh 与 PSES 皆缺失时必须报错");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("not found in PATH"),
            "错误必须带 not_installed_error 形态（LS_NOT_INSTALLED 归类依据）: {msg}"
        );
    }

    /// launch 形态：fake pwsh on PATH + fake 缓存脚本 → argv 指向 fake pwsh、
    /// -Command 引用缓存脚本、cwd = project root。
    #[tokio::test]
    async fn launch_uses_fake_pwsh_and_cached_script() {
        let pwsh_name = if cfg!(windows) { "pwsh.exe" } else { "pwsh" };
        let pwsh_dir = tempfile::tempdir().unwrap();
        std::fs::write(pwsh_dir.path().join(pwsh_name), b"fake").unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        // default_cache_root = {LOCALAPPDATA}/serena/ls（Windows）→ fixture 带全两层。
        let script = cache_dir
            .path()
            .join("serena/ls/powershell/4.4.0/PowerShellEditorServices/Start-EditorServices.ps1");
        std::fs::create_dir_all(script.parent().unwrap()).unwrap();
        std::fs::write(&script, b"# fake script").unwrap();
        let project = tempfile::tempdir().unwrap();

        let path_original = std::env::var_os("PATH").unwrap_or_default();
        let home_key = if cfg!(windows) {
            "LOCALAPPDATA"
        } else {
            "HOME"
        };
        let home_original = std::env::var_os(home_key);
        // SAFETY: 同 launch_errors_when_pwsh_and_cache_missing。
        unsafe {
            let mut new_path = pwsh_dir.path().as_os_str().to_os_string();
            if !path_original.is_empty() {
                new_path.push(if cfg!(windows) { ";" } else { ":" });
                new_path.push(path_original.clone());
            }
            std::env::set_var("PATH", &new_path);
            std::env::set_var(home_key, cache_dir.path());
        }
        let result = PowerShellAdapter
            .launch_info(&ProjectCtx {
                project_root: project.path().to_path_buf(),
            })
            .await;
        unsafe {
            std::env::set_var("PATH", path_original);
            match home_original {
                Some(v) => std::env::set_var(home_key, v),
                None => std::env::remove_var(home_key),
            }
        }
        let info = result.expect("fake pwsh + fake 缓存齐全时 launch_info 必须成功");
        let first = info.cmd[0].to_string_lossy();
        assert!(
            first.contains(pwsh_name),
            "cmd[0] 应指向 fake pwsh, cmd={:?}",
            info.cmd
        );
        assert_eq!(info.cwd, project.path());
        let command_arg = info.cmd[4].to_string_lossy();
        assert!(
            command_arg.contains("Start-EditorServices.ps1"),
            "-Command 须引用缓存脚本: {command_arg}"
        );
        assert!(command_arg.contains("-Stdio"), "{command_arg}");
        // bundled_modules_path = install_dir（脚本父目录的父），供 PSES 解析 zip
        // 内置 PSScriptAnalyzer/PSReadLine —— 上游 py bug（模块目录自身）的修正点。
        // join 形态对齐 resolve_start_script（分步 join），保证 display 分隔符一致。
        let install_dir = cache_dir
            .path()
            .join("serena/ls")
            .join("powershell")
            .join("4.4.0");
        assert!(
            command_arg.contains(&format!("-BundledModulesPath '{}", install_dir.display())),
            "{command_arg}"
        );
    }
}
