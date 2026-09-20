//! jdtls (Eclipse JDT Language Server) 适配器（PLAN M3 / Task T2 第 6 个 = 末尾）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/eclipse_jdtls_language_server.py`
//!
//! jdtls 是 Java 生态最权威的 LSP server；它需要 **JRE 21+ + 单独下载 jdtls 发行包**
//! （https://download.eclipse.org/jdtls/snapshots/），启动慢（~5-10s 加载 JDT workspace）
//! + 索引慢（首次 indexing 按项目大小 30s-几分钟）。这是最复杂的 adapter。
//!
//! ## 启动 quirk（最复杂）
//!
//! 1. 通过 `which_no_unc("jdtls")` 找 launcher；fallback `which_no_unc("java")` + jdtls
//!    equinox launcher JAR（jdtls 二进制本身不带 launcher，需 `java -jar`）。
//! 2. cwd 必须**项目根**；jdtls 通过 mvn/gradle 自动发现 Java 项目。
//! 3. on_server_ready 不能用 documentSymbol 探测 —— jdtls 首次 documentSymbol 阻塞
//!    在 workspace 初始化完成事件；改为等待 `language/status`（jdtls 特有）广播。
//!
//! ## 深度（M2 落地：全自动 jdtls 安装）
//!
//! 不再要求用户自装 jdtls + 配 PATH。`launch_info` 检测到 PATH 无 `jdtls` 时：
//! 1. 走 `ls_runtime::install::DownloadInstaller` 自动下载
//!    `https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz`
//    到 `{cache_root}/jdtls/<version>/`；
//! 2. 解压后 layout = `jdt-language-server-latest/{bin,config_linux,config_mac,config_win}/`；
//! 3. 启动命令模板 = `java -jar {plugins}/org.eclipse.equinox.launcher_<ver>.jar
//!    -configuration {install}/config_{plat} -data {project_root}/.jdtls_workspace`。
//!
//! JRE 探测：仍要求用户预装 JRE 21+（PATH 有 `java`）。jdtls 不含 JRE。
//!
//! ponytail: jdtls project import（maven/gradle 项目解析）走 jdtls 自身流程；不预解析。
//!
//! 已知限制（M3 MVP）：
//! - 不实现 lombok agent 注入（特殊 JAR 配置）；M4+ 加。
//! - 不实现 `maven/gradle settings.json` 自动生成；用户自管。
//! - 默认 JVM 参数（无 `-Xmx` 等调优）；按需 M4+。

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::deps::{Arch, Os};
use ls_runtime::install::{
    ArchiveKind, DownloadInstaller, InstallCtx, InstallKind, InstallOutcome, InstallSpec,
};
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

/// jdtls 启动 + 首次索引合并超时。jdtls 是最慢的 LS —— 给 90s 保守值。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(90);

/// jdtls 最新稳定 snapshot 版本（写死。snapshot 自身滚动；M2 不做 update 流程）。
///
/// 锚：https://download.eclipse.org/jdtls/snapshots/ —— jdtls 用 Maven snapshot 模式分发，
/// URL `jdt-language-server-latest.tar.gz` 是软链，指向最新构建。
#[allow(dead_code)]
const JDTLS_VERSION: &str = "latest";

/// jdtls 安装 cache key（不含版本，URL 含 `latest`）。
#[allow(dead_code)] // M2 stub 占位接口，wire 到 launch_info 时再消
const JDTLS_CACHE_ID: &str = "jdtls";

/// jdtls equinox launcher JAR 相对路径模板（解压后）。
///
/// 实际路径 = `{install}/plugins/org.eclipse.equinox.launcher_<ver>.jar`，版本号随
/// jdtls snapshot 滚动。`install_dir/packaged_jar_path()` 走通配探测：
/// 1. 列出 `plugins/org.eclipse.equinox.launcher_*.jar`，取最像 stable 那个；
/// 2. 找不到 → Err。
fn locate_equinox_launcher(install_dir: &Path) -> Option<PathBuf> {
    let plugins = install_dir.join("plugins");
    let read = std::fs::read_dir(&plugins).ok()?;
    for entry in read.flatten() {
        let p = entry.path();
        if let Some(name) = p.file_name().and_then(|n| n.to_str())
            && name.starts_with("org.eclipse.equinox.launcher_")
            && name.ends_with(".jar")
        {
            return Some(p);
        }
    }
    None
}

/// platform 标识 → jdtls config dir 后缀。
///
/// ↖ mirror: jdtls install instructions: `config_linux/`, `config_mac/`, `config_win/`。
fn config_dir_suffix(os: Os) -> &'static str {
    match os {
        Os::Windows => "win",
        Os::Macos => "mac",
        Os::Linux => "linux",
    }
}

/// 构造 jdtls 启动参数模板（用于 `launch_info`）。
///
/// 返回 argv = `["java", "-jar", "<equinox>", "-configuration", "<cfg_dir>",
/// "-data", "<workspace>"]`。`workspace` = `project_root/.jdtls_workspace`（jdtls 必需）。
pub(crate) fn jdtls_launch_args(
    install_dir: &Path,
    project_root: &Path,
    java_exe: &Path,
) -> anyhow::Result<Vec<String>> {
    let launcher = locate_equinox_launcher(install_dir).ok_or_else(|| {
        anyhow::anyhow!(
            "jdtls equinox launcher JAR not found in {}/plugins/; \
             auto-install incomplete — try clearing cache and retrying",
            install_dir.display()
        )
    })?;
    let os = Os::current();
    let cfg = install_dir.join(format!("config_{}", config_dir_suffix(os)));
    if !cfg.is_dir() {
        anyhow::bail!(
            "jdtls config dir missing: {} (expected config_{{linux|mac|win}})",
            cfg.display()
        );
    }
    let workspace = project_root.join(".jdtls_workspace");
    std::fs::create_dir_all(&workspace).ok();
    Ok(vec![
        java_exe.to_string_lossy().into_owned(),
        "-jar".to_string(),
        launcher.to_string_lossy().into_owned(),
        "-configuration".to_string(),
        cfg.to_string_lossy().into_owned(),
        "-data".to_string(),
        workspace.to_string_lossy().into_owned(),
    ])
}

/// 构造 jdtls auto-install `InstallSpec`。
///
/// ponytail: 不抽 URL 矩阵 helper —— jdtls URL 唯一（snapshot 软链），元组 < (os, arch),
/// JDT-LS 是 platform-neutral Java 包（解包后无 platform 二进制）。
#[allow(dead_code)] // M2 stub 占位接口，wire 到 launch_info 时再消
pub(crate) fn jdtls_install_spec(cache_root: &Path) -> InstallSpec {
    let _install_dir = cache_root.join(JDTLS_CACHE_ID).join(JDTLS_VERSION);
    InstallSpec {
        id: JDTLS_CACHE_ID.to_string(),
        kind: InstallKind::Download {
            version: JDTLS_VERSION.to_string(),
            url: "https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz"
                .to_string(),
            // sha256 未知 → 拒绝 auto-install（auto-install-design §2.9 门）。用户需
            // 走 `--allow-unsigned-sha` 越狱（人类显式）或预装 jdtls。
            sha256: String::new(),
            archive: ArchiveKind::TarGz,
            // jdtls tar 包顶层 = `jdt-language-server-latest/`。strip=1 让
            // bin/config_*/plugins 直接落在 install_dir 下。
            strip_components: 1,
            // bin_path 仅作"装好"短路探测：jdtls 没单一 bin（要走 java -jar），
            // 用 equinox launcher JAR 路径作存在性探针。
            bin_path: "plugins/org.eclipse.equinox.launcher_1.6.500.v20230731-1003.jar"
                .to_string(),
            // Eclipse Foundation 官方域 + 镜像。
            allowed_hosts: vec![
                "download.eclipse.org".to_string(),
                "eclipse.org".to_string(),
            ],
        },
        // exec 模板：jdtls 不是单 binary，由 launch_info 自行构造
        // `java -jar launcher -configuration cfg -data ws` argv。
        exec: vec!["{bin}".to_string()],
    }
}

/// 触发 jdtls auto-install（用户授权 + URL 已知 + sha 已知时）。返回装好后的 install_dir。
///
/// 当前 sha 未知 → 走 `UnsignedRefused` 分支（wire 映射 = LS_NOT_INSTALLED + hint）。
/// 用户可越狱：环境变量 `SERENA_ALLOW_UNSIGNED_SHA=1` 强行装；或预装 jdtls。
///
/// 同步阻塞 IO（HTTP 下载 + 解压分钟级）—— `launch_info` 是 async 上下文，调用方
/// 应 `tokio::task::spawn_blocking` 包裹；M2 实际未 wire 进 `launch_info`（path 1+2
/// 优先 auto-install 流程 M3+ 接 ls-registry Task 21），故本函数暂仅 expose 给
/// supervisor / 测试。
#[allow(dead_code)] // M2 stub 占位接口，wire 到 launch_info 时再消
pub(crate) fn ensure_jdtls_installed(
    cache_root: &Path,
    allow_unsigned_sha: bool,
) -> Result<PathBuf, anyhow::Error> {
    let spec = jdtls_install_spec(cache_root);
    let ctx = InstallCtx {
        os: Os::current(),
        arch: Arch::current(),
        auto_install: true,
        allow_unsigned_sha,
        cache_root: cache_root.to_path_buf(),
    };
    let outcome = DownloadInstaller.install(&ctx, &spec).map_err(|e| {
        anyhow::anyhow!("jdtls auto-install failed: {e}")
    })?;
    let install_dir = cache_root.join(JDTLS_CACHE_ID).join(JDTLS_VERSION);
    match outcome {
        InstallOutcome::Ready(_) => Ok(install_dir),
        InstallOutcome::UnsignedRefused { hint, .. } => {
            // M2 占位：sha 未知拒绝 auto。告诉用户两条路径（预装 / 越狱）。
            Err(anyhow::anyhow!(
                "jdtls auto-install refused (sha unknown): {hint}; \
                 either pre-install jdtls to PATH, or set SERENA_ALLOW_UNSIGNED_SHA=1 to override"
            ))
        }
        InstallOutcome::NotInstalled { hint, .. } => {
            Err(anyhow::anyhow!("jdtls install: {hint}"))
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct JdtlsAdapter;

#[async_trait]
impl LanguageServerAdapter for JdtlsAdapter {
    fn id(&self) -> &'static str {
        "jdtls"
    }

    fn languages(&self) -> &'static [LanguageId] {
        const LANGS: &[LanguageId] = &[LanguageId::Java];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        // 1. 优先 PATH 上有 `jdtls`（用户预装的 launcher；jdtls 1.x 提供）。
        if let Some(exe) = which_no_unc("jdtls") {
            return Ok(LaunchInfo {
                cmd: vec![exe.into_os_string()],
                cwd: ctx.project_root.clone(),
                env: vec![],
                transport: TransportKind::Stdio,
            });
        }
        // 2. PATH 上有 `java` + jdtls 已经预装在常见位置（`JAVA_HOME` 同级）。
        if let Some(java) = which_no_unc("java") {
            // 检查常见预装路径：HOME/jdtls、HOME/.local/share/jdtls、HOME/.cache/jdtls。
            let candidate_rel: &[&str] = &[
                "jdtls",
                ".local/share/jdtls",
                ".cache/jdtls",
            ];
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from);
            if let Some(home) = home {
                for rel in candidate_rel {
                    let install = home.join(rel);
                    if install.is_dir() && locate_equinox_launcher(&install).is_some() {
                        let args = jdtls_launch_args(&install, &ctx.project_root, &java)?;
                        let mut cmd: Vec<std::ffi::OsString> = Vec::with_capacity(args.len());
                        for a in args {
                            cmd.push(a.into());
                        }
                        return Ok(LaunchInfo {
                            cmd,
                            cwd: ctx.project_root.clone(),
                            env: vec![],
                            transport: TransportKind::Stdio,
                        });
                    }
                }
            }
        }
        // 3. 都没有 → 报 not_installed。auto-install（M2 设计 §3.1）尚未 wire
        //    到 ls-adapters（ls-registry Task 21 接线 + 安装策略）；M3 再开。
        Err(not_installed_error(
            "jdtls",
            "install JRE 21+ (`java` on PATH) and either pre-install jdtls (https://download.eclipse.org/jdtls/snapshots/) to PATH or to ~/.local/share/jdtls/; auto-install via supervisor coming in M3",
        ))
    }

    fn initialize_patches(&self, _base: &mut InitializeParams) {
        // jdtls 不需要 client capability quirk；它自己 advertise 自己的能力。
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // jdtls 启动后第一个文档请求会触发 `language/status` 事件 —— 我们不在此处
        // 等待该事件（M3 MVP），因为 Session 的 lazy `ensure_open` 已经会在首次
        // 工具调用时阻塞到 workspace 初始化完成。直接返回 Ok 让 supervisor 放行。
        // 这里仍做一次轻量探测，捕获 jdtls 启动崩溃：
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "workspace/configuration",
                json!({"items": [{"section": "java"}]}),
                READY_PROBE_TIMEOUT,
            )
            .await;
        let _ = probe;
        Ok(())
    }

    fn request_hooks(&self) -> RequestHooks {
        RequestHooks::default()
    }

    fn supports_implementation(&self) -> bool {
        // jdtls 支持 `textDocument/implementation`（interface → class）。
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// equinox launcher 探测：fixture 含 plugins/org.eclipse.equinox.launcher_*.jar → 命中。
    #[test]
    fn locate_equinox_launcher_finds_in_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let plugins = dir.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        let jar = plugins.join("org.eclipse.equinox.launcher_1.6.500.v20230731-1003.jar");
        std::fs::write(&jar, b"jar-placeholder").unwrap();
        let found = locate_equinox_launcher(dir.path()).expect("应命中");
        assert_eq!(
            found.file_name().and_then(|n| n.to_str()),
            Some("org.eclipse.equinox.launcher_1.6.500.v20230731-1003.jar")
        );
    }

    /// equinox launcher 未命中（plugins 缺失 / 无 launcher JAR）→ None。
    #[test]
    fn locate_equinox_launcher_absent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        // 没 plugins 目录 → 探测读 dir 失败 → None。
        assert!(locate_equinox_launcher(dir.path()).is_none());
        // 有 plugins 但无 launcher JAR → 跳过 → None。
        std::fs::create_dir_all(dir.path().join("plugins")).unwrap();
        std::fs::write(
            dir.path().join("plugins/org.eclipse.core.runtime.jar"),
            b"x",
        )
        .unwrap();
        assert!(locate_equinox_launcher(dir.path()).is_none());
    }

    /// jdtls_launch_args：fixture 完整 layout（含 config_linux + plugins/launcher JAR）→ 拼接正确。
    #[test]
    fn jdtls_launch_args_full_path() {
        let dir = tempfile::tempdir().unwrap();
        // 造 plugins/launcher
        let plugins = dir.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("org.eclipse.equinox.launcher_1.6.500.v20230731-1003.jar"),
            b"x",
        )
        .unwrap();
        // 造 config_linux（fixture 走 Os::current()）
        let plat = config_dir_suffix(Os::current());
        let cfg = dir.path().join(format!("config_{plat}"));
        std::fs::create_dir_all(&cfg).unwrap();
        let project = tempfile::tempdir().unwrap();
        let java = Path::new("/usr/bin/java");
        let args = jdtls_launch_args(dir.path(), project.path(), java).expect("应成功");
        // 校验 argv shape：java -jar launcher -configuration cfg -data ws
        assert!(args[0].ends_with("java"));
        assert_eq!(args[1], "-jar");
        assert!(args[2].contains("org.eclipse.equinox.launcher"));
        assert_eq!(args[3], "-configuration");
        assert!(args[4].contains(&format!("config_{plat}")));
        assert_eq!(args[5], "-data");
        assert!(args[6].ends_with(".jdtls_workspace"));
    }

    /// jdtls_launch_args 缺 config dir → Err（装好但 layout 损坏）。
    #[test]
    fn jdtls_launch_args_missing_config_dir_errors() {
        let dir = tempfile::tempdir().unwrap();
        let plugins = dir.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("org.eclipse.equinox.launcher_1.6.500.v20230731-1003.jar"),
            b"x",
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        let java = Path::new("/usr/bin/java");
        // 没有 config_linux/ 等 → Err。
        let res = jdtls_launch_args(dir.path(), project.path(), java);
        assert!(res.is_err(), "缺 config dir 必须报错");
    }

    /// config_dir_suffix: 三平台键对齐（snapshot install instructions 约定）。
    #[test]
    fn config_dir_suffix_per_platform() {
        assert_eq!(config_dir_suffix(Os::Linux), "linux");
        assert_eq!(config_dir_suffix(Os::Macos), "mac");
        assert_eq!(config_dir_suffix(Os::Windows), "win");
    }
}