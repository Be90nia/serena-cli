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
    /// `--request-timeout <ms>`：工具请求 timeout override。CLI 优先于 servers.toml
    /// `timeout_ms` 字段。Phase 4 基建 Task 22b。
    pub timeout_ms: Option<u32>,
    /// `--index-timeout <ms>`：index 类（workspace/symbol）长操作 timeout override。
    pub index_timeout_ms: Option<u32>,
}

/// §4 用户全局 config.toml 路径（Windows %APPDATA%\serena；Unix ~/.config/serena）。
pub fn user_config_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("serena/config.toml"))
    } else {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/serena/config.toml"))
    }
}

/// external-servers.toml 路径（Windows %APPDATA%\serena；Unix ~/.config/serena，
/// external-ls-registration-design §2）。无 HOME/APPDATA → None（机制整体停用）。
pub fn external_servers_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("serena/external-servers.toml"))
    } else {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(".config/serena/external-servers.toml"))
    }
}

/// external-servers.toml（运行时解析，不 include_str!；external-ls-registration-design §3）。
/// 路径缺失/文件不存在/不可读 → None（静默，常态分支）；schema 校验失败 → warn +
/// 当空表（永不触网、不 panic，静默容错对齐上游 entry-point discovery）。
static EXTERNAL: LazyLock<Option<spec::ServersToml>> =
    LazyLock::new(|| external_servers_path().and_then(|p| load_external(&p)));

/// `EXTERNAL` 的加载本体（路径参数化以供测试注入）。成功时对覆盖/扩展名冲突逐条 warn。
pub(crate) fn load_external(path: &Path) -> Option<spec::ServersToml> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!(
                "warning: external-servers.toml unreadable ({}): {e}",
                path.display()
            );
            return None;
        }
    };
    let parsed = match spec::parse(&text) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("warning: external-servers.toml invalid, ignoring entire file: {e}");
            return None;
        }
    };
    warn_external_conflicts(&parsed);
    Some(parsed)
}

/// 覆盖/冲突可观测性（PM 拍板：保留显式覆盖 + warn，非拒绝）。内置表无 priority
/// 字段恒 0，故 external priority ≥ 0 的冲突条目必在 `merge_pick` 中胜出，加载时
/// 逐条 warn 一行；负 priority = 显式让位内置，不告警。扩展名撞内置路由时内置优先，
/// external 条目间互撞时命中未定义——均 warn 提醒。
fn warn_external_conflicts(ext: &spec::ServersToml) {
    let mut seen_exts: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (eid, es) in &ext.servers {
        if es.priority >= 0
            && let Some((bid, _)) = SERVERS.servers.iter().find(|(bid, bs)| {
                bid.eq_ignore_ascii_case(eid)
                    || bs
                        .languages
                        .iter()
                        .any(|l| es.languages.iter().any(|el| el.eq_ignore_ascii_case(l)))
            })
        {
            eprintln!(
                "warning: external-servers.toml `[servers.{eid}]` (priority {}) overrides built-in `[servers.{bid}]`",
                es.priority
            );
        }
        for e in &es.extensions {
            let key = e.trim_start_matches('.').to_lowercase();
            if crate::EXT_TABLE.iter().any(|(be, _)| *be == key) {
                eprintln!(
                    "warning: external-servers.toml `[servers.{eid}]` extension `.{key}` shadows built-in routing (built-in wins)"
                );
            }
            if !seen_exts.insert(key) {
                eprintln!(
                    "warning: external-servers.toml `[servers.{eid}]` declares a duplicate extension; routing pick is unspecified"
                );
            }
        }
    }
}

/// 用户全局 config.toml 的 `[ls.<id>]` 覆盖（读取失败/文件不存在 → None，永不触网）。
pub fn user_override(lang: &str) -> Option<LsOverride> {
    let path = user_config_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&text).ok()?;
    let ls = value.get("ls")?.get(lang)?;
    Some(LsOverride {
        path: ls
            .get("ls_path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from),
        base_cmd: str_list(ls.get("ls_base_cmd")),
        args: str_list(ls.get("ls_args")),
        timeout_ms: ls.get("timeout_ms").and_then(toml_to_u32),
        index_timeout_ms: ls.get("index_timeout_ms").and_then(toml_to_u32),
    })
}

fn str_list(v: Option<&toml::Value>) -> Option<Vec<String>> {
    v?.as_array()?
        .iter()
        .map(|s| s.as_str().map(String::from))
        .collect()
}

fn toml_to_u32(v: &toml::Value) -> Option<u32> {
    v.as_integer()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| {
            // toml 字符串字面量也接受（与 ls_path 同语义）
            v.as_str().and_then(|s| s.parse::<u32>().ok())
        })
}

/// Task 22b：tool 调用的 effective timeout（毫秒）。
///
/// 优先级：CLI flag `LsOverride.timeout_ms` > servers.toml `ServerSpec.timeout_ms`
/// > supervisor 默认 30000ms（30s）。`None` 表示走 supervisor 默认。
///
/// 配套 `effective_index_timeout_ms` 给 index 类（workspace/symbol / 大项目索引），
/// 默认 120s。
pub fn effective_timeout_ms(lang: &str, cli_override: Option<&LsOverride>) -> Option<u32> {
    if let Some(c) = cli_override
        && let Some(ms) = c.timeout_ms
    {
        return Some(ms);
    }
    spec_for(lang).and_then(|(_, spec)| spec.timeout_ms)
}

pub fn effective_index_timeout_ms(lang: &str, cli_override: Option<&LsOverride>) -> Option<u32> {
    if let Some(c) = cli_override
        && let Some(ms) = c.index_timeout_ms
    {
        return Some(ms);
    }
    spec_for(lang).and_then(|(_, spec)| spec.index_timeout_ms)
}

/// 按 id 或语言名在单表中查找（`languages` 数组含 lang，或 id 精确匹配）。
fn table_hit(
    table: &'static spec::ServersToml,
    lang_or_id: &str,
) -> Option<(&'static str, &'static ServerSpec)> {
    table
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

/// 合并优先级（external-ls-registration-design §3）：builtin 与 external 都命中时
/// 取 priority 大者；并列（含双方缺省 0）external 胜出（PM 拍板：保留显式覆盖能力，
/// 可观测性由加载时 `warn_external_conflicts` 统一输出）。返回 (id, spec, is_external)。
pub fn merge_pick<'a>(
    builtin: Option<(&'a str, &'a ServerSpec)>,
    external: Option<(&'a str, &'a ServerSpec)>,
) -> Option<(&'a str, &'a ServerSpec, bool)> {
    match (builtin, external) {
        (Some((bi, bs)), Some((_, es))) if es.priority < bs.priority => Some((bi, bs, false)),
        (Some(_), Some((ei, es))) => Some((ei, es, true)),
        (Some((bi, bs)), None) => Some((bi, bs, false)),
        (None, Some((ei, es))) => Some((ei, es, true)),
        (None, None) => None,
    }
}

/// 内置 + external 双表合并命中（merged_spec_for / spec_source 的共享主体）。
fn merged_hit(lang_or_id: &str) -> Option<(&'static str, &'static ServerSpec, bool)> {
    let external = EXTERNAL.as_ref().and_then(|t| table_hit(t, lang_or_id));
    merge_pick(table_hit(&SERVERS, lang_or_id), external)
}

/// spec_for 的合并版（§3 优先级链：CLI flag > user config.toml > external-servers.toml
/// > 内置 servers.toml；external 为完整条目替换，user_override 为逐字段覆盖，并存不冲突）。
pub fn merged_spec_for(lang_or_id: &str) -> Option<(&'static str, &'static ServerSpec)> {
    merged_hit(lang_or_id).map(|(id, spec, _)| (id, spec))
}

/// merged 命中条目的来源标注（CLI install/status 展示用）：`"external"` / `"builtin"`。
pub fn spec_source(lang_or_id: &str) -> Option<&'static str> {
    merged_hit(lang_or_id).map(|(_, _, external)| if external { "external" } else { "builtin" })
}

/// 按 id 或语言名查找（`languages` 数组含 lang，或 id 精确匹配——`cli install
/// marksman` 用 id，session_for 传语言名，双语义一函数）。手写 T2 语言不在表内。
/// 走 merged 查找：external-servers.toml 条目按 §3 优先级参与命中。
pub fn spec_for(lang_or_id: &str) -> Option<(&'static str, &'static ServerSpec)> {
    merged_spec_for(lang_or_id)
}

/// external-servers.toml 扩展名匹配（design §2 extensions 字段）：小写命中 → 该条目
/// `languages[0]`（session_for/spec_for 按语言名走配置驱动启动）。多条目同扩展名的
/// 命中顺序未定义（HashMap 迭代序）——加载时已 warn。纯函数，测试可注入任意表。
pub(crate) fn match_external_ext<'a>(table: &'a spec::ServersToml, ext: &str) -> Option<&'a str> {
    table
        .servers
        .values()
        .find(|s| {
            s.extensions
                .iter()
                .any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(ext))
        })
        .and_then(|s| s.languages.first())
        .map(String::as_str)
}

/// external 表只读访问（lib.rs `resolve_lang_name` 兜底层用）。
pub(crate) fn external_table() -> Option<&'static spec::ServersToml> {
    EXTERNAL.as_ref()
}

/// 枚举全部条目 id（内置 + external，用于 `cli install --all` 幂等批量安装）。
pub fn all_server_ids() -> impl Iterator<Item = &'static str> {
    let builtin = SERVERS.servers.keys().map(|k| k.as_str());
    let external = EXTERNAL
        .as_ref()
        .into_iter()
        .flat_map(|t| t.servers.keys().map(|k| k.as_str()));
    builtin.chain(external)
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
            let url = dl
                .url_per_platform
                .get(&key)
                .ok_or_else(|| format!("[{id}]: no url for platform `{key}` (InvalidSpec)"))?;
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
            let npm = spec
                .npm
                .as_ref()
                .ok_or_else(|| format!("[{id}]: install=npm but no npm table (InvalidSpec)"))?;
            Ok(InstallSpec {
                id: id.to_string(),
                kind: InstallKind::Npm {
                    package: npm.package.clone(),
                    version: npm.version.clone(),
                    bin_rel: npm.bin_rel.clone(),
                    npm_args: npm.npm_args.clone(),
                    secondary: npm
                        .secondary_packages
                        .iter()
                        .map(|s| {
                            ls_runtime::install_pkg::npm_pkg_ref(&s.package, s.version.as_deref())
                        })
                        .collect(),
                },
                exec: spec.exec.clone(),
            })
        }
        "uvx" => {
            let uvx = spec
                .uvx
                .as_ref()
                .ok_or_else(|| format!("[{id}]: install=uvx but no uvx table (InvalidSpec)"))?;
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
        "dotnet" => {
            let d = spec.dotnet.as_ref().ok_or_else(|| {
                format!("[{id}]: install=dotnet but no dotnet table (InvalidSpec)")
            })?;
            Ok(InstallSpec {
                id: id.to_string(),
                kind: InstallKind::Dotnet {
                    tool: d.tool.clone(),
                    version: d.version.clone(),
                    args: d.args.clone(),
                },
                exec: spec.exec.clone(),
            })
        }
        "gem" => {
            let g = spec
                .gem
                .as_ref()
                .ok_or_else(|| format!("[{id}]: install=gem but no gem table (InvalidSpec)"))?;
            Ok(InstallSpec {
                id: id.to_string(),
                kind: InstallKind::Gem {
                    gem: g.gem.clone(),
                    version: g.version.clone(),
                    bin_rel: g.bin_rel.clone(),
                    args: g.args.clone(),
                },
                exec: spec.exec.clone(),
            })
        }
        "source" => {
            let s = spec.source.as_ref().ok_or_else(|| {
                format!("[{id}]: install=source but no source table (InvalidSpec)")
            })?;
            Ok(InstallSpec {
                id: id.to_string(),
                kind: InstallKind::Source {
                    repo: s.repo.clone(),
                    pin: s.pin.clone(),
                    build_cmd: s.build_cmd.clone(),
                    bin_rel: s.bin_rel.clone(),
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

/// 包管理器类安装 + 拉起组装的共享尾部（uvx/dotnet/gem/source 四分支同构）：
/// spec → InstallSpec → installer → outcome 归一为 (exe, args) 或语义错误串。
fn launch_via_pkg_installer(
    spec: &crate::spec::ServerSpec,
    id: &str,
    auto_install: bool,
    allow_unsigned_sha: bool,
    install: impl FnOnce(
        &InstallCtx,
        &InstallSpec,
    ) -> Result<InstallOutcome, ls_runtime::process::RuntimeError>,
) -> Result<(PathBuf, Vec<String>), String> {
    let install_spec = to_install_spec(spec, id, Os::current(), ls_runtime::deps::Arch::current())?;
    let ictx = InstallCtx {
        os: Os::current(),
        arch: ls_runtime::deps::Arch::current(),
        auto_install,
        allow_unsigned_sha,
        cache_root: dirs_cache_root(),
    };
    match install(&ictx, &install_spec) {
        Ok(InstallOutcome::Ready(ls_runtime::install::Launch::Process { exe, args })) => {
            Ok((exe, args))
        }
        Ok(InstallOutcome::Ready(ls_runtime::install::Launch::External { host, port })) => Err(
            format!("external LS not supported by CLI launch: {host}:{port}"),
        ),
        Ok(InstallOutcome::UnsignedRefused { hint, .. }) => Err(hint),
        Ok(InstallOutcome::NotInstalled { hint, install_cmd }) => match install_cmd {
            Some(cmd) => Err(format!("{hint}; install with: {cmd}")),
            None => Err(hint),
        },
        Err(e) => Err(format!("{e}")),
    }
}

/// A 类安装 + 拉起组装（路径 B opt-in；`auto_install=false` 永不触网，design §0）。
/// 返回 exe 绝对路径 + 展开后的 exec 模板。
pub fn ensure_launch(
    lang: &str,
    cli_override: Option<&LsOverride>,
    auto_install: bool,
    allow_unsigned_sha: bool,
) -> Result<(PathBuf, Vec<String>), String> {
    let (id, spec) = spec_for(lang).ok_or_else(|| format!("no servers.toml entry for `{lang}`"))?;

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
            if let Some(exe) = ls_runtime::install_pkg::npm_bin_path(
                &cache_root.join(id).join(&dir_name),
                &npm.bin_rel,
            ) {
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
                    Err(format!(
                        "external LS not supported by CLI launch: {host}:{port}"
                    ))
                }
                Ok(
                    InstallOutcome::UnsignedRefused { hint, .. }
                    | InstallOutcome::NotInstalled { hint, .. },
                ) => Err(hint),
                Err(e) => Err(format!("{e}")),
            }
        }
        Some(crate::spec::KindRef::Uvx(_)) => {
            // 无安装步骤：uvx 直接拉起（uv 自管缓存）；最终 cmd 由启动器返回。
            launch_via_pkg_installer(spec, id, auto_install, allow_unsigned_sha, |c, s| {
                UvxInstaller.install(c, s)
            })
        }
        Some(crate::spec::KindRef::Dotnet(_)) => {
            launch_via_pkg_installer(spec, id, auto_install, allow_unsigned_sha, |c, s| {
                ls_runtime::install_extra::DotnetInstaller.install(c, s)
            })
        }
        Some(crate::spec::KindRef::Gem(_)) => {
            launch_via_pkg_installer(spec, id, auto_install, allow_unsigned_sha, |c, s| {
                ls_runtime::install_extra::GemInstaller.install(c, s)
            })
        }
        Some(crate::spec::KindRef::Source(_)) => {
            launch_via_pkg_installer(spec, id, auto_install, allow_unsigned_sha, |c, s| {
                ls_runtime::install_extra::SourceInstaller.install(c, s)
            })
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
                    "language server `{lang}` not installed; enable --auto-install or run `serena-cli install {id}` (url={})",
                    dl.url_per_platform
                        .get(&platform_key(os, arch))
                        .map(String::as_str)
                        .unwrap_or("?")
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
/// 实现锚在 `ls_runtime::install::default_cache_root`（adapter 需要同款缓存根，事实源下沉）。
pub fn dirs_cache_root() -> PathBuf {
    ls_runtime::install::default_cache_root()
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

    // ---- Phase 4 Task 22b: timeout resolution ----

    /// 三层合并：CLI > servers.toml > None。CLI 给值 → 优先；不给 → 走 servers.toml；
    /// 都不给 → None（让 supervisor 走默认 30s）。
    #[test]
    fn effective_timeout_prefers_cli_over_server_spec() {
        let cli = LsOverride {
            timeout_ms: Some(1234),
            ..Default::default()
        };
        let ms = effective_timeout_ms("markdown", Some(&cli));
        assert_eq!(ms, Some(1234), "CLI 优先于 servers.toml");
    }

    #[test]
    fn effective_timeout_falls_back_to_server_spec() {
        // 选一个 servers.toml 里 timeout_ms 字段不存在/为 None 的语言；fallback 即 None。
        let cli = LsOverride::default();
        let ms = effective_timeout_ms("markdown", Some(&cli));
        // marksman 当前 toml 未写 timeout_ms → None（supervisor 默认 30s）。
        assert_eq!(ms, None);
    }

    #[test]
    fn effective_timeout_none_when_no_spec_or_override() {
        // 手写 T2（rust）— 无 spec_for 命中 → None
        let ms = effective_timeout_ms("rust", None);
        assert_eq!(ms, None);
    }

    #[test]
    fn effective_timeout_index_uses_dedicated_field() {
        let cli = LsOverride {
            index_timeout_ms: Some(60_000),
            ..Default::default()
        };
        let ms = effective_index_timeout_ms("markdown", Some(&cli));
        assert_eq!(ms, Some(60_000));
    }

    #[test]
    fn servers_toml_timeout_field_parses() {
        // 临时 toml 注入 timeout_ms，确认字段 deser 行为正确
        let toml_str = r#"
            [servers.demo]
            languages = ["demo"]
            install = "path_only"
            timeout_ms = 7777
            index_timeout_ms = 99999
            [servers.demo.path_only]
            binary_name = "demo"
            install_hint = "x"
        "#;
        let parsed = parse(toml_str).expect("toml 解析 ok");
        let s = &parsed.servers["demo"];
        assert_eq!(s.timeout_ms, Some(7777));
        assert_eq!(s.index_timeout_ms, Some(99999));
        // 字段缺省时为 None（向后兼容老 toml）
        let legacy_toml = r#"
            [servers.legacy]
            languages = ["legacy"]
            install = "path_only"
            [servers.legacy.path_only]
            binary_name = "legacy"
            install_hint = "x"
        "#;
        let parsed2 = parse(legacy_toml).expect("legacy toml 解析 ok");
        assert_eq!(parsed2.servers["legacy"].timeout_ms, None);
        assert_eq!(parsed2.servers["legacy"].index_timeout_ms, None);
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
        let out = expand_exec(
            &["{bin}".into(), "lsp".into()],
            Path::new("D:/x/marksman.exe"),
        );
        assert_eq!(out, vec!["D:/x/marksman.exe", "lsp"]);
        // 空模板 = 裸启动默认 [{bin}]（G 类 path_only 批量条目形态）。
        assert_eq!(
            expand_exec(&[], Path::new("D:/x/zls.exe")),
            vec!["D:/x/zls.exe"]
        );
    }

    /// A 类全链路（真下载，门控）：spec → InstallSpec → DownloadInstaller → bin 落地。
    #[test]
    fn marksman_install_e2e_when_gated() {
        if std::env::var_os("SERENA_TEST_DOWNLOAD").is_none() {
            return;
        }
        let (id, spec) = spec_for("markdown").unwrap();
        let install_spec =
            to_install_spec(spec, id, Os::current(), ls_runtime::deps::Arch::current()).unwrap();
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
        let mapped = to_install_spec(
            npm,
            "npm_probe",
            Os::Windows,
            ls_runtime::deps::Arch::X86_64,
        )
        .unwrap();
        match mapped.kind {
            InstallKind::Npm {
                package,
                version,
                bin_rel,
                npm_args,
                secondary: _,
            } => {
                assert_eq!(package, "bash-language-server");
                assert_eq!(version, None, "toml 未写 version → None（latest）");
                assert_eq!(bin_rel, "bash-language-server");
                assert_eq!(npm_args, None);
            }
            other => panic!("expect Npm, got {other:?}"),
        }
        let uvx = &parsed.servers["uvx_probe"];
        let mapped = to_install_spec(
            uvx,
            "uvx_probe",
            Os::Windows,
            ls_runtime::deps::Arch::X86_64,
        )
        .unwrap();
        match mapped.kind {
            InstallKind::Uvx {
                package,
                version,
                entrypoint,
                args,
            } => {
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
        let install_spec = to_install_spec(
            spec,
            "npm_probe",
            Os::current(),
            ls_runtime::deps::Arch::current(),
        )
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

    const EXTRA_KINDS_TOML: &str = r#"
[servers.dn_probe]
languages = ["fsharp-fake"]
install = "dotnet"

[servers.dn_probe.dotnet]
tool = "fsautocomplete"
version = "0.83.0"
args = ["--stdio"]

[servers.gm_probe]
languages = ["ruby-fake"]
install = "gem"

[servers.gm_probe.gem]
gem = "solargraph"
version = "0.51.1"
bin_rel = "solargraph"
args = ["stdio"]

[servers.src_probe]
languages = ["nix-fake"]
install = "source"

[servers.src_probe.source]
repo = "https://github.com/nix-community/nixd"
build_cmd = ["nix", "build"]
bin_rel = "result/bin/nixd"

[servers.npm_sec_probe]
languages = ["ts-fake"]
install = "npm"

[servers.npm_sec_probe.npm]
package = "typescript-language-server"
version = "5.1.3"
bin_rel = "typescript-language-server"

[[servers.npm_sec_probe.npm.secondary_packages]]
package = "typescript"
version = "5.9.3"

[[servers.npm_sec_probe.npm.secondary_packages]]
package = "@vue/language-server"
"#;

    #[test]
    fn to_install_spec_maps_dotnet_gem_source_and_secondary() {
        let parsed = crate::spec::parse(EXTRA_KINDS_TOML).unwrap();
        let dn = &parsed.servers["dn_probe"];
        let mapped =
            to_install_spec(dn, "dn_probe", Os::Windows, ls_runtime::deps::Arch::X86_64).unwrap();
        match mapped.kind {
            InstallKind::Dotnet {
                tool,
                version,
                args,
            } => {
                assert_eq!(tool, "fsautocomplete");
                assert_eq!(version.as_deref(), Some("0.83.0"));
                assert_eq!(args, Some(vec!["--stdio".to_string()]));
            }
            other => panic!("expect Dotnet, got {other:?}"),
        }
        let gm = &parsed.servers["gm_probe"];
        let mapped =
            to_install_spec(gm, "gm_probe", Os::Windows, ls_runtime::deps::Arch::X86_64).unwrap();
        match mapped.kind {
            InstallKind::Gem {
                gem,
                version,
                bin_rel,
                args,
            } => {
                assert_eq!(gem, "solargraph");
                assert_eq!(version.as_deref(), Some("0.51.1"));
                assert_eq!(bin_rel, "solargraph");
                assert_eq!(args, Some(vec!["stdio".to_string()]));
            }
            other => panic!("expect Gem, got {other:?}"),
        }
        let src = &parsed.servers["src_probe"];
        let mapped = to_install_spec(
            src,
            "src_probe",
            Os::Windows,
            ls_runtime::deps::Arch::X86_64,
        )
        .unwrap();
        match mapped.kind {
            InstallKind::Source {
                repo,
                pin,
                build_cmd,
                bin_rel,
            } => {
                assert_eq!(repo, "https://github.com/nix-community/nixd");
                assert!(pin.is_none());
                assert_eq!(build_cmd, vec!["nix", "build"]);
                assert_eq!(bin_rel, "result/bin/nixd");
            }
            other => panic!("expect Source, got {other:?}"),
        }
        // npm secondary → pkg@ver 引用（npm_pkg_ref 同形态；无版本 = 裸包名）。
        let npm = &parsed.servers["npm_sec_probe"];
        let mapped = to_install_spec(
            npm,
            "npm_sec_probe",
            Os::Windows,
            ls_runtime::deps::Arch::X86_64,
        )
        .unwrap();
        match mapped.kind {
            InstallKind::Npm { secondary, .. } => {
                assert_eq!(
                    secondary,
                    vec![
                        "typescript@5.9.3".to_string(),
                        "@vue/language-server".to_string()
                    ]
                );
            }
            other => panic!("expect Npm, got {other:?}"),
        }
    }

    /// ensure_launch 对新 kind 的 NotInstalled 归一路径：真表 gem 条目 + auto_install=false →
    /// 语义错误串含安装命令（不触网）。其余新 kind 共享同一 helper。
    #[test]
    fn ensure_launch_gem_without_auto_install_reports_hint() {
        assert!(spec_for("solargraph").is_some(), "solargraph 条目应已收录");
        let err = ensure_launch("solargraph", None, false, false).unwrap_err();
        assert!(
            err.contains("gem install --user-install") && err.contains("solargraph"),
            "err: {err}"
        );
        // 同 helper 的 uvx 路径：pyright 未装 uv → hint 带 uv 安装指引（本机有 uv 则 Ready）。
        let _ = ensure_launch("pyright", None, false, false);
    }

    // ---- external-servers.toml（external-ls-registration-design §3/§4）----

    /// §3 静默容错：文件不存在 → None（常态分支，不 warn 不 panic）。
    #[test]
    fn load_external_missing_file_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("absent.toml");
        assert!(load_external(&p).is_none(), "文件不存在 → None");
    }

    /// §3 静默容错：schema 校验失败 → warn + 当空表（None），永不触网不 panic。
    #[test]
    fn load_external_bad_toml_is_none_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("external-servers.toml");
        std::fs::write(&p, "[servers.broken\nlanguages = ").expect("write bad toml");
        assert!(load_external(&p).is_none(), "parse 失败 → None（当空表）");
    }

    /// §3 静默容错：路径不可读（目录 / 无权限）→ None。
    #[test]
    fn load_external_unreadable_path_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            load_external(dir.path()).is_none(),
            "目录路径 → 读取失败 → None"
        );
    }

    /// §2/§3 正常路径：合法 external 表加载成功；覆盖内置条目（marksman）时走
    /// `warn_external_conflicts` warn 分支（stderr 观测，不断言）且条目保留。
    #[test]
    fn load_external_valid_table_loads_and_keeps_override_entry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("external-servers.toml");
        let toml_str = concat!(
            "[servers.marksman]\n",
            "languages = [\"markdown\"]\n",
            "install = \"path_only\"\n",
            "priority = 0\n",
            "[servers.marksman.path_only]\n",
            "binary_name = \"fake-marksman\"\n",
            "install_hint = \"x\"\n",
        );
        std::fs::write(&p, toml_str).expect("write valid external toml");
        let t = load_external(&p).expect("valid table loads");
        assert_eq!(t.servers["marksman"].priority, 0);
        assert_eq!(
            t.servers["marksman"]
                .path_only
                .as_ref()
                .unwrap()
                .binary_name,
            "fake-marksman"
        );
    }

    /// §2 extension 路由核心：小写命中条目 `languages[0]`；大小写不敏感；未命中 None。
    #[test]
    fn match_external_ext_routes_to_first_language() {
        let t = spec::parse(
            "[servers.mydsl]\nlanguages = [\"mydsl\", \"mydsl2\"]\nextensions = [\".mydsl\", \"mydsl3\"]\n\
             install = \"path_only\"\n[servers.mydsl.path_only]\nbinary_name = \"x\"\ninstall_hint = \"x\"\n",
        )
        .expect("valid");
        assert_eq!(match_external_ext(&t, "mydsl"), Some("mydsl"));
        assert_eq!(
            match_external_ext(&t, "MYDSL"),
            Some("mydsl"),
            "大小写不敏感"
        );
        assert_eq!(
            match_external_ext(&t, "mydsl3"),
            Some("mydsl"),
            "无点前缀也命中"
        );
        assert_eq!(match_external_ext(&t, "other"), None);
    }
}
