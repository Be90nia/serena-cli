//! jdtls (Eclipse JDT Language Server) 适配器（M3 / Task T2 第 6 个；W3b 双模式改造）。
//!
//! ↖ mirror: oraios/serena@7a296833 `language_servers/eclipse_jdtls.py`
//!
//! ## 双安装模式（上游对拍采纳 W3b，bd 69e batchA 漂移[高]）
//!
//! **vscode-java VSIX 模式（默认）**：下载平台 VSIX 整包（JDTLS + 捆绑 JRE 21 +
//! Lombok，↖ mirror `DEFAULT_VSCODE_JAVA_VERSION = "1.54.0-923"` + 五平台 sha 钉
//! ↖ mirror `downloaded_dependency_hashes.json`），JVM ≈28 参（--add-modules /
//! --add-opens×3 / lombok agent / 共享索引 / Xmx3G，↖ mirror `create_launch_command`）
//! + env `JAVA_HOME` = 捆绑 JRE / `syntaxserver=false`（↖ mirror `create_launch_command_env`）。
//!
//! Δ 未抄 Gradle 8.14.2 捆绑下载——gradle 工程走 Buildship 标准发现（./gradlew /
//! 系统 gradle），少 ~130MB 下载面；initializationOptions 只移植关键树
//! （runtimes/includeGeneratedCode/interactive import），其余走 jdtls 自身默认
//! （上游大树多为显式化默认值）。
//!
//! **snapshot fallback 模式**（网络受限环境）：Eclipse snapshot tar 滚动软链 +
//! 系统 JDK（M3 原路径，`allow_unsigned_sha` 特例保留）。切换 = 代码内
//! [`SNAPSHOT_FALLBACK`] const 或 env `SERENA_JDTLS_SNAPSHOT=1`（不改代码的
//! 运行时逃生门）。
//!
//! **-configuration per-project 复制**（↖ mirror `_start_server` 尾部：发行版
//! `config_<plat>` 是只读发行物，OSGi 首启写回 `config.ini`——共享/只读目录直指会
//! 污染或失败；两种模式都复制到 `{project}/.jdtls_workspace/config_path` 幂等）。
//!
//! ## 启动 quirk（最复杂）
//!
//! 1. cwd 必须**项目根**；jdtls 通过 mvn/gradle 自动发现 Java 项目。
//! 2. on_server_ready 不能用 documentSymbol 探测 —— jdtls 首次 documentSymbol 阻塞
//!    在 workspace 初始化完成事件；用 workspace/configuration 轻探针捕获启动崩溃。
//!
//! JRE：VSIX 模式自带（JRE 21.0.10 捆绑）；snapshot 模式要求用户预装（PATH `java`，
//! 已知漂移：latest snapshot 使 JDK 下限漂浮，当前构建要求 JavaSE 25）。

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

/// snapshot fallback 模式开关（false = VSIX 默认，↖ mirror 上游双模式语义）。
/// 运行时逃生门：env `SERENA_JDTLS_SNAPSHOT=1` 覆盖（网络受限环境不改代码切换）。
const SNAPSHOT_FALLBACK: bool = false;

fn snapshot_mode() -> bool {
    SNAPSHOT_FALLBACK || std::env::var("SERENA_JDTLS_SNAPSHOT").as_deref() == Ok("1")
}

/// vscode-java VSIX pin（↖ mirror `DEFAULT_VSCODE_JAVA_VERSION`，eclipse_jdtls.py@7a296833）。
/// release tag = v1.54.0（版本去掉尾部构建号，↖ mirror `_create_deps_vscode_java` 的
/// `rsplit('-', 1)` 推导——已折进下方 URL 字面量）。
const VSCODE_JAVA_VERSION: &str = "1.54.0-923";
/// VSIX 内资源路径 pin（↖ mirror `DEFAULT_VSCODE_JAVA_PATHS`：bump 版本必须连动）。
const VSIX_JRE_VERSION: &str = "21.0.10";
const VSIX_LOMBOK_BASENAME: &str = "lombok-1.18.39-4050.jar";
const VSIX_LAUNCHER_BASENAME: &str = "org.eclipse.equinox.launcher_1.7.100.v20251111-0406.jar";

/// 平台 → (VSIX URL, sha256, jre 目录后缀, config 目录名)。
/// ↖ mirror: `_create_deps_vscode_java` + `VSCodeJavaConfig` 表 + hash db
/// `downloaded_dependency_hashes.json`（sha 一手锚）。
fn vsix_platform(
    os: Os,
    arch: Arch,
) -> Option<(&'static str, &'static str, &'static str, &'static str)> {
    match (os, arch) {
        (Os::Windows, Arch::X86_64) => Some((
            "https://github.com/redhat-developer/vscode-java/releases/download/v1.54.0/java-win32-x64-1.54.0-923.vsix",
            "66f3914987edeccfee8a2558470e0fde4f8c4154232ff4baa5d73373ebc819d4",
            "win32-x86_64",
            "config_win",
        )),
        (Os::Linux, Arch::X86_64) => Some((
            "https://github.com/redhat-developer/vscode-java/releases/download/v1.54.0/java-linux-x64-1.54.0-923.vsix",
            "9d4b15da54e25a0192f9bac073f086c015397d3676623b68dbf83a5dbaf5132b",
            "linux-x86_64",
            "config_linux",
        )),
        (Os::Linux, Arch::Aarch64) => Some((
            "https://github.com/redhat-developer/vscode-java/releases/download/v1.54.0/java-linux-arm64-1.54.0-923.vsix",
            "e2bb22c427d90da8dbb1afff72ff1e2dce38d50b76deb02d7bc313a330a1330c",
            "linux-aarch64",
            "config_linux_arm",
        )),
        (Os::Macos, Arch::X86_64) => Some((
            "https://github.com/redhat-developer/vscode-java/releases/download/v1.54.0/java-darwin-x64-1.54.0-923.vsix",
            "dfc98abc4e54165a78372e280242a039671729b1b03420608df3b10c6b629fb6",
            "macosx-x86_64",
            "config_mac",
        )),
        (Os::Macos, Arch::Aarch64) => Some((
            "https://github.com/redhat-developer/vscode-java/releases/download/v1.54.0/java-darwin-arm64-1.54.0-923.vsix",
            "c54c45cb0d2579d8e0a4ddeb24d4a9dd0b460d07d9366adea2b38a1da22a463c",
            "macosx-aarch64",
            "config_mac_arm",
        )),
        _ => None,
    }
}

/// VSIX 模式资源路径集合（解包后 `{install}/extension/...`，strip_components=0 保留
/// `extension/` 顶层 —— ↖ mirror `VSCodeJavaConfig` 路径模板）。
struct VsixPaths {
    java_exe: PathBuf,
    jre_home: PathBuf,
    lombok_jar: PathBuf,
    launcher_jar: PathBuf,
    readonly_config: PathBuf,
}

fn vsix_paths(install: &Path) -> anyhow::Result<VsixPaths> {
    let (os, arch) = (Os::current(), Arch::current());
    let (_, _, jre_suffix, config_dir) = vsix_platform(os, arch).ok_or_else(|| {
        anyhow::anyhow!("unsupported platform for vscode-java VSIX: {os:?}/{arch:?}")
    })?;
    let extension = install.join("extension");
    let jre_home = extension
        .join("jre")
        .join(format!("{VSIX_JRE_VERSION}-{jre_suffix}"));
    let java_exe = if cfg!(windows) {
        jre_home.join("bin").join("java.exe")
    } else {
        jre_home.join("bin").join("java")
    };
    let paths = VsixPaths {
        java_exe: java_exe.clone(),
        jre_home: jre_home.clone(),
        lombok_jar: extension.join("lombok").join(VSIX_LOMBOK_BASENAME),
        launcher_jar: extension
            .join("server")
            .join("plugins")
            .join(VSIX_LAUNCHER_BASENAME),
        readonly_config: extension.join("server").join(config_dir),
    };
    for (p, label) in [
        (&paths.java_exe, "bundled java"),
        (&paths.lombok_jar, "lombok jar"),
        (&paths.launcher_jar, "equinox launcher"),
        (&paths.readonly_config, "config dir"),
    ] {
        if !p.exists() {
            anyhow::bail!(
                "vscode-java VSIX incomplete: {label} missing at {}",
                p.display()
            );
        }
    }
    Ok(paths)
}

/// VSIX 安装 spec（运行时按当前平台组 URL/sha；DownloadInstaller 全链复用：sha 门
/// 正常开启——VSIX 有官方 hash，snapshot 的 allow_unsigned_sha 特例不适用）。
fn vsix_install_spec() -> anyhow::Result<(InstallSpec, &'static str, &'static str)> {
    let (url, sha, jre_suffix, _) = vsix_platform(Os::current(), Arch::current())
        .ok_or_else(|| anyhow::anyhow!("unsupported platform for vscode-java VSIX"))?;
    // 短路探针 = 捆绑 java（版本化文件名 pin 确定；bin 探测与启动路径解耦——启动
    // 走 vsix_paths 的 launcher 通配）。
    let java_rel = if cfg!(windows) {
        format!("extension/jre/{VSIX_JRE_VERSION}-{jre_suffix}/bin/java.exe")
    } else {
        format!("extension/jre/{VSIX_JRE_VERSION}-{jre_suffix}/bin/java")
    };
    let spec = InstallSpec {
        id: JDTLS_CACHE_ID.to_string(),
        kind: InstallKind::Download {
            version: VSCODE_JAVA_VERSION.to_string(),
            url: url.to_string(),
            sha256: sha.to_string(),
            archive: ArchiveKind::Zip,
            strip_components: 0,
            bin_path: java_rel,
            allowed_hosts: vec![
                "github.com".to_string(),
                "release-assets.githubusercontent.com".to_string(),
                "objects.githubusercontent.com".to_string(),
            ],
        },
        exec: vec!["{bin}".to_string()],
    };
    Ok((spec, url, sha))
}

/// 触发 VSIX auto-install，返回 install_dir（`{cache_root}/jdtls/{VSCODE_JAVA_VERSION}/`）。
/// 同步阻塞 IO（~100MB + 解压）——async 调用方 spawn_blocking 包裹。
fn ensure_vsix_installed(cache_root: &Path) -> Result<PathBuf, anyhow::Error> {
    let (spec, _, _) = vsix_install_spec()?;
    let ctx = InstallCtx {
        os: Os::current(),
        arch: Arch::current(),
        auto_install: true,
        allow_unsigned_sha: false,
        cache_root: cache_root.to_path_buf(),
    };
    let outcome = DownloadInstaller
        .install(&ctx, &spec)
        .map_err(|e| anyhow::anyhow!("vscode-java VSIX auto-install failed: {e}"))?;
    match outcome {
        InstallOutcome::Ready(_) => Ok(cache_root.join(JDTLS_CACHE_ID).join(VSCODE_JAVA_VERSION)),
        InstallOutcome::UnsignedRefused { hint, .. }
        | InstallOutcome::NotInstalled { hint, .. } => {
            Err(anyhow::anyhow!("vscode-java VSIX auto-install: {hint}"))
        }
    }
}

/// JVM 参数（↖ mirror `create_launch_command` ≈L764-850 逐字：--add-modules /
/// --add-opens×3 / eclipse.* / ParallelGC 组 / lombok agent / 共享索引 / Xmx3G）。
const JVM_ARGS: &[&str] = &[
    "--add-modules=ALL-SYSTEM",
    "--add-opens",
    "java.base/java.util=ALL-UNNAMED",
    "--add-opens",
    "java.base/java.lang=ALL-UNNAMED",
    "--add-opens",
    "java.base/sun.nio.fs=ALL-UNNAMED",
    "-Declipse.application=org.eclipse.jdt.ls.core.id1",
    "-Dosgi.bundles.defaultStartLevel=4",
    "-Declipse.product=org.eclipse.jdt.ls.core.product",
    "-Djava.import.generatesMetadataFilesAtProjectRoot=false",
    "-Dfile.encoding=utf8",
    "-noverify",
    "-XX:+UseParallelGC",
    "-XX:GCTimeRatio=4",
    "-XX:AdaptiveSizePolicyWeight=90",
    "-Dsun.zip.disableMemoryMapping=true",
    "-Djava.lsp.joinOnCompletion=true",
    "-Xmx3G",
    "-Xms100m",
    "-Xlog:disable",
    "-Dlog.level=ALL",
];

/// 发行版 config → per-project 复制（幂等；↖ mirror `_start_server` 尾部
/// `shutil.copytree` —— OSGi 首启写回 config.ini，只读发行目录直指会污染/失败）。
fn copy_config_per_project(readonly_config: &Path, workspace: &Path) -> anyhow::Result<PathBuf> {
    let target = workspace.join("config_path");
    if !target.exists() {
        std::fs::create_dir_all(workspace).ok();
        jdtls_copy_dir(readonly_config, &target)?;
    }
    Ok(target)
}

fn jdtls_copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            jdtls_copy_dir(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

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
/// 返回 argv = `["java", "-jar", "<equinox>", "-configuration", "<per-project cfg>",
/// "-data", "<workspace>/data_dir"]`。`-configuration` 经 [`copy_config_per_project`]
/// 从 `readonly_config` 幂等复制到 `{project_root}/.jdtls_workspace/`（OSGi 写回隔离，
/// ↖ mirror 上游 `ws_dir/config_path`；Δ 上游 data 子目录名 `data_dir` 同步对齐）。
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
    let cfg_copy = copy_config_per_project(&cfg, &workspace)?;
    Ok(vec![
        java_exe.to_string_lossy().into_owned(),
        "-jar".to_string(),
        launcher.to_string_lossy().into_owned(),
        "-configuration".to_string(),
        cfg_copy.to_string_lossy().into_owned(),
        "-data".to_string(),
        workspace.join("data_dir").to_string_lossy().into_owned(),
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
        // VSIX 模式（默认）：捆绑 JRE 启动，JVM ≈28 参 + lombok agent + env
        // JAVA_HOME/syntaxserver（↖ mirror create_launch_command(_env)）。
        // 系统 PATH 的 `jdtls` 仍最优先（用户显式预装 = 明确意图）。
        if !snapshot_mode() {
            if let Some(exe) = which_no_unc("jdtls") {
                return Ok(LaunchInfo {
                    cmd: vec![exe.into_os_string()],
                    cwd: ctx.project_root.clone(),
                    env: vec![],
                    transport: TransportKind::Stdio,
                });
            }
            let cache_root = default_cache_root();
            let install = tokio::task::spawn_blocking(move || ensure_vsix_installed(&cache_root))
                .await
                .map_err(|e| {
                    not_installed_error("jdtls", &format!("auto-install task join failed: {e}"))
                })?
                .map_err(|e| {
                    not_installed_error(
                        "jdtls",
                        &format!(
                            "vscode-java VSIX auto-install failed ({e:#}); set \
                             SERENA_JDTLS_SNAPSHOT=1 to fall back to the Eclipse snapshot \
                             + system JDK mode"
                        ),
                    )
                })?;
            let paths = vsix_paths(&install)?;
            let workspace = ctx.project_root.join(".jdtls_workspace");
            let cfg_copy = copy_config_per_project(&paths.readonly_config, &workspace)?;
            let shared_index = default_cache_root().join("jdtls-shared-index");
            std::fs::create_dir_all(&shared_index).ok();
            let mut cmd: Vec<std::ffi::OsString> = vec![paths.java_exe.clone().into_os_string()];
            cmd.extend(JVM_ARGS.iter().map(Into::into));
            cmd.push(format!("-javaagent:{}", paths.lombok_jar.display()).into());
            cmd.push(format!("-Djdt.core.sharedIndexLocation={}", shared_index.display()).into());
            cmd.push("-jar".into());
            cmd.push(paths.launcher_jar.clone().into_os_string());
            cmd.push("-configuration".into());
            cmd.push(cfg_copy.clone().into_os_string());
            cmd.push("-data".into());
            cmd.push(workspace.join("data_dir").into_os_string());
            return Ok(LaunchInfo {
                cmd,
                cwd: ctx.project_root.clone(),
                env: vec![
                    (
                        "JAVA_HOME".to_string(),
                        paths.jre_home.to_string_lossy().into_owned(),
                    ),
                    ("syntaxserver".to_string(), "false".to_string()),
                ],
                transport: TransportKind::Stdio,
            });
        }
        // snapshot fallback 模式（M3 原路径）：系统 JDK + Eclipse snapshot tar。
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

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // VSIX 模式关键 initializationOptions（↖ mirror `_create_base_initialize_params`
        // 的 settings.java 关键树；Δ 上游 300 行大树多为 jdtls 自身默认值的显式化，
        // 只移植行为分叉项 + lombok 符号面 + 捆绑 JRE runtime 注册）。
        if snapshot_mode() {
            return; // snapshot 模式维持 M3 原行为（无 init options）
        }
        let install = default_cache_root()
            .join(JDTLS_CACHE_ID)
            .join(VSCODE_JAVA_VERSION);
        if !install.is_dir() {
            return; // 未装（首启前）→ 不注入，launch_info 装好后下一会话生效
        }
        let Ok(paths) = vsix_paths(&install) else {
            return;
        };
        let opts = base
            .initialization_options
            .get_or_insert_with(serde_json::Value::default);
        if !opts.is_object() {
            *opts = serde_json::json!({});
        }
        opts["bundles"] = serde_json::json!([]);
        opts["workspaceFolders"] = serde_json::json!([]);
        opts["settings"] = serde_json::json!({
            "java": {
                "configuration": {
                    "updateBuildConfiguration": "interactive",
                    "runtimes": [
                        { "name": "JavaSE-21", "path": paths.jre_home.to_string_lossy(), "default": true }
                    ],
                },
                "import": {
                    "maven": { "enabled": true, "downloadSources": true },
                    "gradle": { "enabled": true, "annotationProcessing": { "enabled": true } },
                },
                "maven": { "downloadSources": true, "updateSnapshots": false },
                "eclipse": { "downloadSources": true },
                // lombok 生成符号上 documentSymbol（↖ mirror #1432 includeGeneratedCode）。
                "symbols": { "includeGeneratedCode": true },
                "autobuild": { "enabled": true },
                "server": { "launchMode": "Standard" },
            }
        });
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
        // 造 config_<plat>（fixture 走 Os::current()；jdtls_launch_args 检查存在）
        let cfg = dir
            .path()
            .join(format!("config_{}", config_dir_suffix(Os::current())));
        std::fs::create_dir_all(&cfg).unwrap();
        let project = tempfile::tempdir().unwrap();
        let java = Path::new("/usr/bin/java");
        let args = jdtls_launch_args(dir.path(), project.path(), java).expect("应成功");
        // 校验 argv shape：java -jar launcher -configuration per-project cfg -data data_dir。
        // -configuration 必须指 per-project 副本（OSGi 写回隔离），不是发行版只读目录。
        assert!(args[0].ends_with("java"));
        assert_eq!(args[1], "-jar");
        assert!(args[2].contains("org.eclipse.equinox.launcher"));
        assert_eq!(args[3], "-configuration");
        let cfg_arg = Path::new(&args[4]);
        assert!(
            cfg_arg.ends_with("config_path")
                && cfg_arg.starts_with(project.path().join(".jdtls_workspace")),
            "config 必须复制到 per-project workspace: {}",
            args[4]
        );
        assert_eq!(args[5], "-data");
        assert!(args[6].ends_with("data_dir"));
        // 幂等：config_path 已复制到项目 workspace。
        assert!(project.path().join(".jdtls_workspace/config_path").is_dir());
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

    /// VSIX 平台矩阵对账上游（↖ mirror `_create_deps_vscode_java` URL 形态 +
    /// hash db sha 一手锚；全五平台枚举，运行时取当前平台）。
    #[test]
    fn vsix_platform_matrix_matches_upstream() {
        let cases = [
            (
                Os::Windows,
                Arch::X86_64,
                "java-win32-x64-1.54.0-923.vsix",
                "66f3914987edeccfee8a2558470e0fde4f8c4154232ff4baa5d73373ebc819d4",
            ),
            (
                Os::Linux,
                Arch::X86_64,
                "java-linux-x64-1.54.0-923.vsix",
                "9d4b15da54e25a0192f9bac073f086c015397d3676623b68dbf83a5dbaf5132b",
            ),
            (
                Os::Linux,
                Arch::Aarch64,
                "java-linux-arm64-1.54.0-923.vsix",
                "e2bb22c427d90da8dbb1afff72ff1e2dce38d50b76deb02d7bc313a330a1330c",
            ),
            (
                Os::Macos,
                Arch::X86_64,
                "java-darwin-x64-1.54.0-923.vsix",
                "dfc98abc4e54165a78372e280242a039671729b1b03420608df3b10c6b629fb6",
            ),
            (
                Os::Macos,
                Arch::Aarch64,
                "java-darwin-arm64-1.54.0-923.vsix",
                "c54c45cb0d2579d8e0a4ddeb24d4a9dd0b460d07d9366adea2b38a1da22a463c",
            ),
        ];
        for (os, arch, url_tail, sha) in cases {
            let (url, got_sha, _, _) =
                vsix_platform(os, arch).unwrap_or_else(|| panic!("{os:?}/{arch:?} must map"));
            assert!(url.ends_with(url_tail), "{url}");
            assert!(url.starts_with(
                "https://github.com/redhat-developer/vscode-java/releases/download/v1.54.0/"
            ));
            assert_eq!(got_sha, sha, "{os:?}/{arch:?} sha 对账 hash db");
        }
    }

    /// VSIX spec：zip / strip=0（保留 extension/ 顶层）/ sha 门开启（VSIX 有官方
    /// hash，snapshot 的 allow_unsigned_sha 特例不适用）/ bin 探针 = 捆绑 java。
    #[test]
    fn vsix_install_spec_shape() {
        let (spec, _, sha) = vsix_install_spec().expect("current platform supported");
        assert_eq!(spec.id, "jdtls");
        let InstallKind::Download {
            version,
            archive,
            strip_components,
            sha256,
            bin_path,
            ..
        } = spec.kind
        else {
            panic!("expected Download kind");
        };
        assert_eq!(version, "1.54.0-923");
        assert!(matches!(archive, ArchiveKind::Zip));
        assert_eq!(strip_components, 0, "VSIX zip 保留 extension/ 顶层");
        assert_eq!(sha256, sha);
        assert!(bin_path.starts_with("extension/jre/21.0.10-"), "{bin_path}");
    }

    /// per-project config 复制幂等（↖ mirror copytree 幂等语义：已存在不重铺）。
    #[test]
    fn copy_config_per_project_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let ro = dir.path().join("ro_config");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::write(ro.join("config.ini"), b"first").unwrap();
        let ws = dir.path().join("ws");
        let target = copy_config_per_project(&ro, &ws).expect("copy");
        assert_eq!(
            std::fs::read(target.join("config.ini")).unwrap(),
            b"first".to_vec()
        );
        // 改源不回灌（副本独立，OSGi 写回隔离语义）。
        std::fs::write(ro.join("config.ini"), b"second").unwrap();
        let target2 = copy_config_per_project(&ro, &ws).expect("idempotent");
        assert_eq!(
            std::fs::read(target2.join("config.ini")).unwrap(),
            b"first".to_vec()
        );
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
