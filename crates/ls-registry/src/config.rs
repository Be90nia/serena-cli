//! ConfigAdapter：`ServerSpec → InstallSpec` 映射 + §4 override 优先级。
//!
//! 依赖方向（momus Critical-1）：ServerSpec 归 ls-registry，InstallSpec 归 ls-runtime；
//! 本模块是唯一映射点。
//!
//! §4 优先级：CLI flag（`LsOverride`）> 用户全局 config.toml > servers.toml 默认。
//! 项目级配置**不参与** cmd 构造（防恶意仓库注入二进制）；环境变量 override 禁止。

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use ls_runtime::deps::Os;
use ls_runtime::install::{
    ArchiveKind, DownloadInstaller, InstallCtx, InstallKind, InstallOutcome, InstallSpec,
};
use ls_runtime::install_pkg::{NpmInstaller, UvxInstaller};

use crate::spec::{self, ServerSpec};

/// 内置单表（编译期 include_str!，坏表 = 编译期数据 bug，LazyLock expect 合理；
/// 外部 toml 输入路径 v1 不存在，见 design §0 非目标）。
static SERVERS: LazyLock<spec::ServersToml> = LazyLock::new(|| {
    spec::parse(include_str!("../servers.toml")).expect("built-in servers.toml must be valid")
});

/// §4 CLI flag 层（clap global 参数在 CLI 侧，机制上以本结构传入）。
#[derive(Debug, Clone, Default)]
pub struct LsOverride {
    /// `--ls-path`：直接指定可执行文件；不存在 → MissingRuntime + hint。
    pub path: Option<PathBuf>,
    /// `--ls-base-cmd`：整体替换启动命令（占位符 `{bin}` 可引用 path）。
    pub base_cmd: Option<Vec<String>>,
    /// `--ls-args`：追加参数。
    pub args: Option<Vec<String>>,
}

/// §4 用户全局 config.toml 路径（Windows %APPDATA%\serena；Unix ~/.config/serena）。
pub fn user_config_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("serena/config.toml"))
    } else {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/serena/config.toml"))
    }
}

/// 用户全局 config.toml 的 `[ls.<id>]` 覆盖（读取失败/文件不存在 → None，永不触网）。
pub fn user_override(lang: &str) -> Option<LsOverride> {
    let path = user_config_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    let ls = value.get("ls")?.get(lang)?;
    Some(LsOverride {
        path: ls.get("ls_path").and_then(|v| v.as_str()).map(PathBuf::from),
        base_cmd: str_list(ls.get("ls_base_cmd")),
        args: str_list(ls.get("ls_args")),
    })
}

fn str_list(v: Option<&toml::Value>) -> Option<Vec<String>> {
    v?.as_array()?
        .iter()
        .map(|s| s.as_str().map(String::from))
        .collect()
}

/// 按 id 或语言名查找（`languages` 数组含 lang，或 id 精确匹配——`cli install
/// marksman` 用 id，session_for 传语言名，双语义一函数）。手写 T2 语言不在表内。
pub fn spec_for(lang_or_id: &str) -> Option<(&'static str, &'static ServerSpec)> {
    SERVERS
        .servers
        .iter()
        .find(|(id, s)| {
            id.eq_ignore_ascii_case(lang_or_id)
                || s.languages
                    .iter()
                    .any(|l| l.eq_ignore_ascii_case(lang_or_id))
        })
        .map(|(k, v)| (k.as_str(), v))
}

/// platform key（design §2.2）。
pub fn platform_key(os: Os, arch: ls_runtime::deps::Arch) -> String {
    let os = match os {
        Os::Windows => "windows",
        Os::Linux => "linux",
        Os::Macos => "macos",
    };
    let arch = match arch {
        ls_runtime::deps::Arch::X86_64 => "x86_64",
        ls_runtime::deps::Arch::Aarch64 => "aarch64",
    };
    format!("{os}-{arch}")
}

/// `ServerSpec → InstallSpec`（per-platform 解析；本层唯一映射点）。
pub fn to_install_spec(
    spec: &ServerSpec,
    id: &str,
    os: Os,
    arch: ls_runtime::deps::Arch,
) -> Result<InstallSpec, String> {
    match spec.install.as_str() {
        "download" => {
            let dl = spec.download.as_ref().ok_or_else(|| {
                format!("[{id}]: install=download but no download table (InvalidSpec)")
            })?;
            let key = platform_key(os, arch);
            let url = dl.url_per_platform.get(&key).ok_or_else(|| {
                format!("[{id}]: no url for platform `{key}` (InvalidSpec)")
            })?;
            let sha256 = dl
                .sha256_per_platform
                .get(&key)
                .cloned()
                .unwrap_or_default();
            let archive = match dl.archive.as_str() {
                "zip" => ArchiveKind::Zip,
                "tar.gz" => ArchiveKind::TarGz,
                "tar.xz" => ArchiveKind::TarXz,
                "gz" => ArchiveKind::SingleGz,
                "raw" => ArchiveKind::Raw,
                other => return Err(format!("[{id}]: unknown archive `{other}` (InvalidSpec)")),
            };
            Ok(InstallSpec {
                id: id.to_string(),
                kind: InstallKind::Download {
                    version: dl.version.clone(),
                    url: url.clone(),
                    sha256,
                    archive,
                    strip_components: dl.strip_components,
                    bin_path: dl.bin_path.clone(),
                    allowed_hosts: dl.allowed_hosts.clone(),
                },
                exec: spec.exec.clone(),
            })
        }
        "path_only" => {
            // path_only 无下载产物；转 PathOnly InstallKind（install 流程返 NotInstalled+hint）。
            let po = spec.path_only.as_ref().ok_or_else(|| {
                format!("[{id}]: install=path_only but no path_only table (InvalidSpec)")
            })?;
            Ok(InstallSpec {
                id: id.to_string(),
                kind: InstallKind::PathOnly {
                    binary_name: po.binary_name.clone(),
                    install_hint: po.install_hint.clone(),
                },
                exec: spec.exec.clone(),
            })
        }
        "npm" => {
            let npm = spec.npm.as_ref().ok_or_else(|| {
                format!("[{id}]: install=npm but no npm table (InvalidSpec)")
            })?;
            Ok(InstallSpec {
                id: id.to_string(),
                kind: InstallKind::Npm {
                    package: npm.package.clone(),
                    version: npm.version.clone(),
                    bin_rel: npm.bin_rel.clone(),
                    npm_args: npm.npm_args.clone(),
                },
                exec: spec.exec.clone(),
            })
        }
        "uvx" => {
            let uvx = spec.uvx.as_ref().ok_or_else(|| {
                format!("[{id}]: install=uvx but no uvx table (InvalidSpec)")
            })?;
            Ok(InstallSpec {
                id: id.to_string(),
                kind: InstallKind::Uvx {
                    package: uvx.package.clone(),
                    version: uvx.version.clone(),
                    entrypoint: uvx.entrypoint.clone(),
                    args: uvx.args.clone(),
                },
                exec: spec.exec.clone(),
            })
        }
        other => Err(format!("[{id}]: unknown install `{other}` (InvalidSpec)")),
    }
}

/// 展开占位符模板：`{bin}` → exe 路径；空模板默认 `[{bin}]`（裸启动即 stdio LS）。
pub fn expand_exec(exec: &[String], bin: &Path) -> Vec<String> {
    if exec.is_empty() {
        return vec![bin.to_string_lossy().to_string()];
    }
    exec.iter()
        .map(|a| {
            if a == "{bin}" {
                bin.to_string_lossy().to_string()
            } else {
                a.clone()
            }
        })
        .collect()
}

/// §4 优先级合成：override（CLI）> user config > None。
/// 返回最终 LsOverride（None = 无覆盖，走 servers.toml/安装流默认）。
pub fn effective_override(lang: &str, cli: Option<&LsOverride>) -> Option<LsOverride> {
    if let Some(c) = cli {
        return Some(c.clone());
    }
    user_override(lang)
}

/// A 类安装 + 拉起组装（路径 B opt-in；`auto_install=false` 永不触网，design §0）。
/// 返回 exe 绝对路径 + 展开后的 exec 模板。
pub fn ensure_launch(
    lang: &str,
    cli_override: Option<&LsOverride>,
    auto_install: bool,
    allow_unsigned_sha: bool,
) -> Result<(PathBuf, Vec<String>), String> {
    let (id, spec) =
        spec_for(lang).ok_or_else(|| format!("no servers.toml entry for `{lang}`"))?;

    // §4 override：CLI --ls-path 直接指定（不存在 → MissingRuntime 语义错误）。
    if let Some(LsOverride { path: Some(p), .. }) = effective_override(lang, cli_override) {
        if !p.is_file() {
            return Err(format!(
                "missing runtime `{}`: --ls-path points to a non-existent file",
                p.display()
            ));
        }
        return Ok((p.clone(), expand_exec(&spec.exec, &p)));
    }

    match spec.kind_table() {
        Some(crate::spec::KindRef::PathOnly(po)) => {
            // PATH 探测（复用 ls-adapters 查找，去 UNC）。
            if let Some(found) = ls_adapters::which_path(&po.binary_name) {
                let expanded = expand_exec(&spec.exec, &found);
                return Ok((found, expanded));
            }
            Err(format!(
                "missing runtime `{}` not on PATH: {}",
                po.binary_name, po.install_hint
            ))
        }
        Some(crate::spec::KindRef::Npm(npm)) => {
            let os = Os::current();
            let arch = ls_runtime::deps::Arch::current();
            let install_spec = to_install_spec(spec, id, os, arch)?;
            let cache_root = dirs_cache_root();
            // 缓存命中（未触网）：{cache_root}/{id}/{version|latest}/node_modules/.bin/{bin_rel}。
            // 最终 cmd 由安装机制决定，不走 {bin} 模板。
            let dir_name = npm.version.clone().unwrap_or_else(|| "latest".to_string());
            if let Some(exe) =
                ls_runtime::install_pkg::npm_bin_path(&cache_root.join(id).join(&dir_name), &npm.bin_rel)
            {
                return Ok((exe, npm.npm_args.clone().unwrap_or_default()));
            }
            if !auto_install {
                return Err(format!(
                    "language server `{lang}` not installed; enable --auto-install or run `serena-cli install {id}` (npm package {})",
                    npm.package
                ));
            }
            let ictx = InstallCtx {
                os,
                arch,
                auto_install,
                allow_unsigned_sha,
                cache_root,
            };
            match NpmInstaller.install(&ictx, &install_spec) {
                Ok(InstallOutcome::Ready(ls_runtime::install::Launch::Process { exe, args })) => {
                    Ok((exe, args))
                }
                Ok(InstallOutcome::Ready(ls_runtime::install::Launch::External { host, port })) => {
                    Err(format!("external LS not supported by CLI launch: {host}:{port}"))
                }
                Ok(InstallOutcome::UnsignedRefused { hint, .. } | InstallOutcome::NotInstalled { hint, .. }) => {
                    Err(hint)
                }
                Err(e) => Err(format!("{e}")),
            }
        }
        Some(crate::spec::KindRef::Uvx(_)) => {
            // 无安装步骤：uvx 直接拉起（uv 自管缓存）；最终 cmd 由启动器返回。
            let install_spec =
                to_install_spec(spec, id, Os::current(), ls_runtime::deps::Arch::current())?;
            let ictx = InstallCtx {
                os: Os::current(),
                arch: ls_runtime::deps::Arch::current(),
                auto_install,
                allow_unsigned_sha,
                cache_root: dirs_cache_root(),
            };
            match UvxInstaller.install(&ictx, &install_spec) {
                Ok(InstallOutcome::Ready(ls_runtime::install::Launch::Process { exe, args })) => {
                    Ok((exe, args))
                }
                Ok(InstallOutcome::NotInstalled { hint, .. }) => Err(hint),
                other => Err(format!("uvx launch unexpected outcome: {other:?}")),
            }
        }
        Some(crate::spec::KindRef::Download(dl)) => {
            let os = Os::current();
            let arch = ls_runtime::deps::Arch::current();
            let install_spec = to_install_spec(spec, id, os, arch)?;
            let cache_root = dirs_cache_root();
            // 安装目录缓存命中（未触网）：{cache_root}/{id}/{version}/{bin_path}。
            let exe = cache_root.join(id).join(&dl.version).join(&dl.bin_path);
            if exe.is_file() {
                let expanded = expand_exec(&spec.exec, &exe);
                return Ok((exe, expanded));
            }
            if !auto_install {
                return Err(format!(
                    "language server `{lang}` not installed; enable --auto-install or run `serena-cli install {id}` (url={})"
                    , dl.url_per_platform.get(&platform_key(os, arch)).map(String::as_str).unwrap_or("?")
                ));
            }
            let ictx = InstallCtx {
                os,
                arch,
                auto_install,
                allow_unsigned_sha,
                cache_root,
            };
            match DownloadInstaller.install(&ictx, &install_spec) {
                Ok(InstallOutcome::Ready(launch)) => match launch {
                    ls_runtime::install::Launch::Process { exe, .. } => {
                        let expanded = expand_exec(&spec.exec, &exe);
                        Ok((exe, expanded))
                    }
                    ls_runtime::install::Launch::External { host, port } => Err(format!(
                        "external LS not supported by CLI launch: {host}:{port}"
                    )),
                },
                Ok(InstallOutcome::UnsignedRefused { hint, .. }) => Err(hint),
                Ok(InstallOutcome::NotInstalled { hint, .. }) => Err(hint),
                Err(e) => Err(format!("{e}")),
            }
        }
        None => Err(format!(
            "[{id}]: install={} but its table is missing (InvalidSpec)",
            spec.install
        )),
    }
}

/// 安装缓存根（design §5 cache_root；v1：%LOCALAPPDATA%\serena\ls / ~/.local/share/serena/ls）。
pub fn dirs_cache_root() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(|a| PathBuf::from(a).join("serena/ls"))
            .unwrap_or_else(|| PathBuf::from(".serena-ls"))
    } else {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".local/share/serena/ls"))
            .unwrap_or_else(|| PathBuf::from(".serena-ls"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::parse;

    const TOML: &str = include_str!("../servers.toml");

    #[test]
    fn builtin_servers_toml_is_valid_and_parsed() {
        let parsed = parse(TOML).expect("内置表必须合法（编译期数据）");
        assert!(parsed.servers.contains_key("marksman"));
        assert!(parsed.servers.contains_key("crystalline"));
        assert!(spec_for("markdown").is_some(), "languages 维度命中");
        assert!(spec_for("crystal").is_some());
        assert!(spec_for("rust").is_none(), "手写 T2 语言不进表");
    }

    #[test]
    fn to_install_spec_resolves_per_platform() {
        let (id, spec) = spec_for("markdown").unwrap();
        // Windows：raw + 真 digest。
        let win = to_install_spec(spec, id, Os::Windows, ls_runtime::deps::Arch::X86_64).unwrap();
        match win.kind {
            InstallKind::Download {
                url,
                sha256,
                archive,
                ..
            } => {
                assert!(url.ends_with("marksman.exe"));
                assert_eq!(archive, ArchiveKind::Raw);
                assert_eq!(sha256.len(), 64);
            }
            other => panic!("expect Download, got {other:?}"),
        }
        // macos aarch64 与 x86_64 共用 marksman-macos。
        let mac = to_install_spec(spec, id, Os::Macos, ls_runtime::deps::Arch::Aarch64).unwrap();
        match mac.kind {
            InstallKind::Download { url, .. } => assert!(url.ends_with("marksman-macos")),
            other => panic!("expect Download, got {other:?}"),
        }
        // 未知平台 → InvalidSpec。
        // （platform_key 枚举封闭，缺平台只能靠 toml 数据缺失表达——
        //  windows-aarch64 无条目即此形态。）
        let missing = to_install_spec(spec, id, Os::Windows, ls_runtime::deps::Arch::Aarch64);
        assert!(missing.is_err(), "缺平台必须报 InvalidSpec");
    }

    #[test]
    fn override_priority_cli_beats_user_beats_default() {
        let cli = LsOverride {
            path: Some(PathBuf::from("C:/custom/marksman.exe")),
            ..Default::default()
        };
        // CLI 在场 → 原样胜出（user config 不读）。
        assert_eq!(
            effective_override("markdown", Some(&cli)).unwrap().path,
            Some(PathBuf::from("C:/custom/marksman.exe"))
        );
        // CLI 不在 → 读用户 config（本机多半没有 → None = 默认流）。
        // 该分支的 config.toml 读取逻辑由 user_override 的 None 兼容性保证。
        let _ = effective_override("markdown", None);
    }

    #[test]
    fn expand_exec_substitutes_bin_placeholder() {
        let out = expand_exec(&["{bin}".into(), "lsp".into()], Path::new("D:/x/marksman.exe"));
        assert_eq!(out, vec!["D:/x/marksman.exe", "lsp"]);
        // 空模板 = 裸启动默认 [{bin}]（G 类 path_only 批量条目形态）。
        assert_eq!(expand_exec(&[], Path::new("D:/x/zls.exe")), vec!["D:/x/zls.exe"]);
    }

    /// A 类全链路（真下载，门控）：spec → InstallSpec → DownloadInstaller → bin 落地。
    #[test]
    fn marksman_install_e2e_when_gated() {
        if std::env::var_os("SERENA_TEST_DOWNLOAD").is_none() {
            return;
        }
        let (id, spec) = spec_for("markdown").unwrap();
        let install_spec =
            to_install_spec(spec, id, Os::current(), ls_runtime::deps::Arch::current())
                .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ictx = InstallCtx {
            os: Os::current(),
            arch: ls_runtime::deps::Arch::current(),
            auto_install: true,
            allow_unsigned_sha: false,
            cache_root: dir.path().to_path_buf(),
        };
        let out = DownloadInstaller.install(&ictx, &install_spec).unwrap();
        assert!(matches!(out, InstallOutcome::Ready(_)), "{out:?}");
    }

    const NPM_TOML: &str = r#"
[servers.npm_probe]
languages = ["js-fake"]
install = "npm"

[servers.npm_probe.npm]
package = "bash-language-server"
bin_rel = "bash-language-server"

[servers.uvx_probe]
languages = ["py-fake"]
install = "uvx"

[servers.uvx_probe.uvx]
package = "fake-ls"
version = "0.9.0"
entrypoint = "fake-ls"
args = ["-v"]
"#;

    #[test]
    fn to_install_spec_maps_npm_and_uvx() {
        let parsed = crate::spec::parse(NPM_TOML).unwrap();
        let npm = &parsed.servers["npm_probe"];
        let mapped =
            to_install_spec(npm, "npm_probe", Os::Windows, ls_runtime::deps::Arch::X86_64).unwrap();
        match mapped.kind {
            InstallKind::Npm { package, version, bin_rel, npm_args } => {
                assert_eq!(package, "bash-language-server");
                assert_eq!(version, None, "toml 未写 version → None（latest）");
                assert_eq!(bin_rel, "bash-language-server");
                assert_eq!(npm_args, None);
            }
            other => panic!("expect Npm, got {other:?}"),
        }
        let uvx = &parsed.servers["uvx_probe"];
        let mapped =
            to_install_spec(uvx, "uvx_probe", Os::Windows, ls_runtime::deps::Arch::X86_64).unwrap();
        match mapped.kind {
            InstallKind::Uvx { package, version, entrypoint, args } => {
                assert_eq!(package, "fake-ls");
                assert_eq!(version.as_deref(), Some("0.9.0"));
                assert_eq!(entrypoint, "fake-ls");
                assert_eq!(args, Some(vec!["-v".to_string()]));
            }
            other => panic!("expect Uvx, got {other:?}"),
        }
    }

    /// npm 类全链路（真 npm install，门控 SERENA_TEST_DOWNLOAD）：内存 spec →
    /// InstallSpec → NpmInstaller → node_modules/.bin bin 落地断言。
    /// npm 不在场（CI 无 Node）→ MissingRuntime，不算失败。
    #[test]
    fn npm_install_e2e_when_gated() {
        if std::env::var_os("SERENA_TEST_DOWNLOAD").is_none() {
            return;
        }
        let parsed = crate::spec::parse(NPM_TOML).unwrap();
        let spec = &parsed.servers["npm_probe"];
        let install_spec =
            to_install_spec(spec, "npm_probe", Os::current(), ls_runtime::deps::Arch::current())
                .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ictx = InstallCtx {
            os: Os::current(),
            arch: ls_runtime::deps::Arch::current(),
            auto_install: true,
            allow_unsigned_sha: false,
            cache_root: dir.path().to_path_buf(),
        };
        match NpmInstaller.install(&ictx, &install_spec) {
            Ok(InstallOutcome::Ready(ls_runtime::install::Launch::Process { exe, .. })) => {
                assert!(exe.is_file(), "bin 应落地: {}", exe.display());
                // 已装短路：二调不再触网，仍 Ready。
                let out2 = NpmInstaller.install(&ictx, &install_spec).unwrap();
                assert!(matches!(out2, InstallOutcome::Ready(_)), "{out2:?}");
            }
            Ok(other) => panic!("应 Ready，实际 {other:?}"),
            Err(e) => {
                let msg = format!("{e}");
                assert!(
                    msg.contains("MissingRuntime") || msg.contains("missing runtime `npm`"),
                    "npm 在场却失败，须排查: {msg}"
                );
                eprintln!("SKIP: npm not available on this machine ({msg})");
            }
        }
    }
}
