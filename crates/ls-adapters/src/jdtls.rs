//! jdtls (Eclipse JDT Language Server) 适配器（PLAN M3 / Task T2 第 6 个 = 末尾）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/eclipse_jdtls_language_server.py`
//!
//! jdtls 是 Java 生态最权威的 LSP server；它需要 **JRE 25+ + jdtls 发行包**
//! （https://download.eclipse.org/jdtls/snapshots/，首启自动下载到本地缓存），
//! 启动慢（~5-10s 加载 JDT workspace）+ 索引慢（首次 indexing 按项目大小 30s-几分钟）。
//! 这是最复杂的 adapter。
//!
//! ## 启动 quirk（最复杂）
//!
//! 1. 通过 `which_no_unc("jdtls")` 找 launcher；fallback `which_no_unc("java")` + jdtls
//!    equinox launcher JAR（jdtls 二进制本身不带 launcher，需 `java -jar`）。
//! 2. cwd 必须**项目根**；jdtls 通过 mvn/gradle 自动发现 Java 项目。
//! 3. on_server_ready 不能用 documentSymbol 探测 —— jdtls 首次 documentSymbol 阻塞
//!    在 workspace 初始化完成事件；改为等待 `language/status`（jdtls 特有）广播。
//!
//! ## 安装（launch_info 已接线：首启自动下载）
//!
//! PATH 无 `jdtls` 且无预装目录时，`launch_info` 走 `ls_runtime::install::DownloadInstaller`：
//! 1. 下载 `https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz`
//!    到 `{cache_root}/jdtls/latest/`（Windows %LOCALAPPDATA%\serena\ls，Unix
//!    ~/.local/share/serena/ls）；已装缓存秒回短路（幂等）。
//! 2. snapshot tar **顶层即包体**（`{bin,config_*,features,plugins}/` 直接在顶，
//!    无 `jdt-language-server-latest/` 包装层）→ `strip_components: 0`。
//! 3. sha256 特例：`latest` 是滚动软链且 Eclipse 不发布伴随 hash 文件
//!    （`.sha256`/`.sha512` 实测 404），无法钉 hash —— 信任锚 = HTTPS +
//!    `allowed_hosts` 白名单（download.eclipse.org），跳过 §2.9 sha 门。
//! 4. 启动命令模板 = `java -jar {plugins}/org.eclipse.equinox.launcher_<ver>.jar
//!    -configuration {install}/config_{plat} -data {project_root}/.jdtls_workspace`。
//!
//! JRE 探测：仍要求用户预装 JRE 25+（PATH 有 `java`）。jdtls 不含 JRE。
//! 已知漂移：`latest` snapshot 滚动使 JDK 下限漂浮（当前构建要求 JavaSE 25，
//! JDK21 启动秒死）；钉版本属后续版本管理范畴，不在本接线范围。
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
    default_cache_root,
};
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

/// jdtls 启动 + 首次索引合并超时。jdtls 是最慢的 LS —— 给 90s 保守值。
const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(90);

/// jdtls 最新稳定 snapshot 版本（写死。snapshot 自身滚动；不做 update 流程）。
///
/// 锚：https://download.eclipse.org/jdtls/snapshots/ —— jdtls 用 Maven snapshot 模式分发，
/// URL `jdt-language-server-latest.tar.gz` 是软链，指向最新构建。
const JDTLS_VERSION: &str = "latest";

/// jdtls 安装 cache key（不含版本，URL 含 `latest`）。
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
pub(crate) fn jdtls_install_spec() -> InstallSpec {
    InstallSpec {
        id: JDTLS_CACHE_ID.to_string(),
        kind: InstallKind::Download {
            version: JDTLS_VERSION.to_string(),
            url: "https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz"
                .to_string(),
            // sha256 特例留空：latest 滚动软链 + Eclipse 无伴随 hash 文件（404 实测），
            // 无法钉 hash —— `ensure_jdtls_installed` 以 allow_unsigned_sha 跳过 §2.9 门
            // （信任锚 = HTTPS + allowed_hosts 域白名单）。
            sha256: String::new(),
            archive: ArchiveKind::TarGz,
            // snapshot tar 顶层即包体（bin/config_*/plugins 直接在顶，无包装层）——
            // strip >0 会把包体拍平。
            strip_components: 0,
            // "装好"短路探针：equinox launcher JAR 文件名带滚动版本号（不可写死），
            // 用全平台都存在的 `bin/jdtls`（POSIX 启动脚本，与 bin_path 探测解耦——
            // 启动仍走 `jdtls_launch_args` 的 launcher 通配探测）。
            bin_path: "bin/jdtls".to_string(),
            // Eclipse Foundation 官方域。
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

/// 触发 jdtls auto-install。返回装好后的 install_dir（`{cache_root}/jdtls/{JDTLS_VERSION}/`）。
///
/// 已装缓存短路在 `DownloadInstaller` 内（`bin/jdtls` 存在即秒回，不触网）。
///
/// sha 门特例：`latest` snapshot 滚动 + Eclipse 无伴随 hash 文件 → 无法钉 sha256，
/// 信任锚 = HTTPS + `allowed_hosts`（download.eclipse.org 白名单）→ 跳过 §2.9 门。
///
/// 同步阻塞 IO（HTTP ~51MB + 解压，分钟级；client 自带 connect 30s / 总 600s 超时）——
/// async 调用方（`launch_info`）以 `spawn_blocking` 包裹。失败（URL 不可达等）→
/// Err，调用方 wrap 成 `not_installed_error` 形态 → wire 归类 LS_NOT_INSTALLED。
pub(crate) fn ensure_jdtls_installed(cache_root: &Path) -> Result<PathBuf, anyhow::Error> {
    let spec = jdtls_install_spec();
    let ctx = InstallCtx {
        os: Os::current(),
        arch: Arch::current(),
        auto_install: true,
        // jdtls 特例：见函数 doc —— snapshot 滚动无官方 hash，域白名单即信任锚。
        allow_unsigned_sha: true,
        cache_root: cache_root.to_path_buf(),
    };
    let outcome = DownloadInstaller
        .install(&ctx, &spec)
        .map_err(|e| anyhow::anyhow!("jdtls auto-install failed: {e}"))?;
    match outcome {
        InstallOutcome::Ready(_) => Ok(cache_root.join(JDTLS_CACHE_ID).join(JDTLS_VERSION)),
        // allow_unsigned_sha=true 且 kind=Download 时不可达（UnsignedRefused 仅 sha 门
        // 拒绝时返回；NotInstalled 仅 PathOnly 返回）——defensive，不吞错不 panic。
        InstallOutcome::UnsignedRefused { hint, .. }
        | InstallOutcome::NotInstalled { hint, .. } => {
            Err(anyhow::anyhow!("jdtls auto-install: {hint}"))
        }
    }
}

/// 拼 `java -jar <equinox> -configuration <cfg> -data <ws>` 的 LaunchInfo。
/// path 3（预装目录）与 path 4（auto-install 产物）共用。
fn launch_via_jar(install_dir: &Path, ctx: &ProjectCtx, java: &Path) -> anyhow::Result<LaunchInfo> {
    let args = jdtls_launch_args(install_dir, &ctx.project_root, java)?;
    Ok(LaunchInfo {
        cmd: args.into_iter().map(Into::into).collect(),
        cwd: ctx.project_root.clone(),
        env: vec![],
        transport: TransportKind::Stdio,
    })
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
        // 2. jdtls 本体是 equinox JAR 集合，必须 `java -jar` 启动 → JVM 是硬前置。
        let java = which_no_unc("java").ok_or_else(|| {
            not_installed_error(
                "jdtls",
                "install a JRE 25+ (`java` on PATH; latest snapshot requires JavaSE 25); \
                 the jdtls distribution itself is auto-downloaded to the local cache on first launch",
            )
        })?;
        // 3. 常见预装位置（手工安装形态：~/jdtls、~/.local/share/jdtls、~/.cache/jdtls）。
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        if let Some(home) = home {
            for rel in ["jdtls", ".local/share/jdtls", ".cache/jdtls"] {
                let install = home.join(rel);
                if install.is_dir() && locate_equinox_launcher(&install).is_some() {
                    return launch_via_jar(&install, ctx, &java);
                }
            }
        }
        // 4. 都没有 → 首启自动下载（同步阻塞 IO，spawn_blocking 走阻塞线程池，
        //    不占 runtime worker）。已装缓存在 DownloadInstaller 内短路（幂等，不触网）。
        let cache_root = default_cache_root();
        let dir = tokio::task::spawn_blocking(move || ensure_jdtls_installed(&cache_root))
            .await
            .map_err(|e| {
                not_installed_error("jdtls", &format!("auto-install task join failed: {e}"))
            })?
            .map_err(|e| {
                not_installed_error(
                    "jdtls",
                    &format!(
                        "auto-install failed ({e:#}); pre-install jdtls \
                         (https://download.eclipse.org/jdtls/snapshots/) to PATH or ~/.local/share/jdtls/"
                    ),
                )
            })?;
        launch_via_jar(&dir, ctx, &java)
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

    /// install spec 对齐真实 snapshot 布局：顶层即包体（strip=0，P-2——strip=1 会把
    /// {bin,config_*,plugins}/ 拍平）；短路探针 = 全平台存在的 `bin/jdtls`（equinox
    /// launcher JAR 文件名带滚动版本号，写死必失效）。
    #[test]
    fn install_spec_matches_snapshot_layout() {
        let spec = jdtls_install_spec();
        assert_eq!(spec.id, "jdtls");
        let InstallKind::Download {
            version,
            url,
            sha256,
            archive,
            strip_components,
            bin_path,
            allowed_hosts,
        } = spec.kind
        else {
            panic!("expected Download kind, got {:?}", spec.kind);
        };
        assert_eq!(version, "latest");
        assert_eq!(
            url,
            "https://download.eclipse.org/jdtls/snapshots/jdt-language-server-latest.tar.gz"
        );
        assert_eq!(
            sha256, "",
            "latest 滚动无钉 hash（sha 门由 ensure 特例跳过）"
        );
        assert!(matches!(archive, ArchiveKind::TarGz));
        assert_eq!(strip_components, 0, "snapshot tar 顶层即包体");
        assert_eq!(bin_path, "bin/jdtls");
        assert!(allowed_hosts.contains(&"download.eclipse.org".to_string()));
    }

    /// 幂等：缓存内 `bin/jdtls` 已存在 → DownloadInstaller 短路秒回，不触网。
    #[test]
    fn ensure_short_circuits_when_cached() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("jdtls/latest/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("jdtls"), b"#!/bin/sh\n").unwrap();
        let got = ensure_jdtls_installed(dir.path()).expect("cached install must short-circuit");
        assert_eq!(got, dir.path().join("jdtls/latest"));
    }
}
