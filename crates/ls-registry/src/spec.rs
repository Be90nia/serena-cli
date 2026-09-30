//! `servers.toml` schema（auto-install-design.md v0.4 §2）。
//!
//! 内置表 include_str! 编译期内嵌；external-servers.toml（用户目录，运行时解析）
//! 复用同一 schema（external-ls-registration-design.md §2）。
//!
//! 首批（Task 19）：A 类 download（marksman）+ F 类 path_only（crystalline）。
//! npm/uvx/dotnet/gem/特殊形态（§2.3-2.6/2.8）随 Task 20 批量收录扩充。
//!
//! platform key 规范：`"{os}-{arch}"`（windows-x86_64 / linux-x86_64 /
//! macos-x86_64 / macos-aarch64），与 design §2.2 一致。

use std::collections::HashMap;

use serde::Deserialize;

/// `servers.toml` 顶层（内置 include_str! 单表 + external-servers.toml 运行时表共用）。
/// `servers` 带 serde default：ls-use 移除全部条目后只剩头注释的文件按空表解析，
/// 不报 `missing field servers`（用户注册表为空是常态，不是错误）。
#[derive(Debug, Deserialize)]
pub struct ServersToml {
    #[serde(default)]
    pub servers: HashMap<String, ServerSpec>,
}

/// §2.1 通用字段 + 安装方式子表。
#[derive(Debug, Deserialize)]
pub struct ServerSpec {
    pub languages: Vec<String>,
    /// 文件扩展名（信息性；文件探测实际归 ls-registry EXT_TABLE，path_only 可省）。
    #[serde(default)]
    pub extensions: Vec<String>,
    /// download | path_only | npm | uvx | dotnet | gem | source
    pub install: String,
    /// 含 `{bin}` 占位符的启动模板（如 `["{bin}", "lsp"]`）；省略 = 裸启动 `[{bin}]`。
    /// npm/uvx 条目不适用——最终 cmd 由安装器/启动器返回（见 NpmSpec/UvxSpec）。
    #[serde(default)]
    pub exec: Vec<String>,
    pub download: Option<DownloadSpec>,
    pub path_only: Option<PathOnlySpec>,
    pub npm: Option<NpmSpec>,
    pub uvx: Option<UvxSpec>,
    pub dotnet: Option<DotnetSpec>,
    pub gem: Option<GemSpec>,
    pub source: Option<SourceSpec>,
    /// 数据漂移锚（design §8 风险表）：本条目抄译自上游的 commit。
    pub source_commit: Option<String>,
    /// Phase 4 基建 Task 22b：per-LS 工具请求超时（毫秒）。CLI flag `LsOverride` 优先
    /// 于本字段；二者皆 None → supervisor 默认 30s。Index 类（workspace/symbol）
    /// 走 `index_timeout_ms`（默认 120s），仍受本字段覆盖语义相同。
    #[serde(default)]
    pub timeout_ms: Option<u32>,
    /// Index / workspace 类长操作超时（毫秒）；缺省沿用 `timeout_ms * 4` 计算逻辑。
    #[serde(default)]
    pub index_timeout_ms: Option<u32>,
    /// external-servers.toml 合并优先级（external-ls-registration-design §2/§3）：
    /// 同 id/language 与内置表冲突时取大者，并列（含缺省 0）external 胜出。
    /// 内置 servers.toml 不使用本字段（全部缺省 0）；负值 = 显式让位内置。
    #[serde(default)]
    pub priority: i32,
    /// ---- 上游对拍采纳 Wave 1（T0 三通道，upstream-compare-batchA/B/C 横切根因）----
    /// 以下字段全部可省；省略 = 行为与引入前逐字节一致（向后兼容硬约束）。
    /// 合并进 initialize params 的 `initializationOptions`（深层合并：两侧皆对象时
    /// 递归并集、本表值覆盖；基线缺失/类型不匹配则直接设本值）。
    /// ↖ mirror: 上游各 `*_language_server.py` `_create_base_initialize_params`
    ///   的 `initializationOptions`（yaml/dart/elixir 首批消费者，toml 条目注释带行号锚）。
    #[serde(default)]
    pub init_options: Option<serde_json::Value>,
    /// spawn 时注入的环境变量（`[servers.X.env]` 子表）。键为 `PATH`（大小写不敏感）
    /// 时取「原值 + 平台分隔符 + 本值」追加语义（LS 伴随工具目录可达性）；其余键
    /// 直接覆盖设置（进程环境其余变量照常继承）。
    #[serde(default)]
    pub env: Option<std::collections::HashMap<String, String>>,
    /// `initialized` 后自动推送一条 `workspace/didChangeConfiguration`
    /// `{settings: <本值>}`（外层 settings 键由接线层包裹）。
    /// ↖ mirror: julia_server.py@7a296833 `_start_server` 尾部 nudge
    ///   （LanguageServer.jl 只在 didChangeConfiguration handler 里触发配置拉取）。
    #[serde(default)]
    pub did_change_config: Option<serde_json::Value>,
    /// `workspace/configuration` 请求的 per-LS 真值应答表：请求 item 的 `section`
    /// 命中表内条目时回对应 `value`，未命中回 null（与本 crate 引入前的默认
    /// null 数组行为一致）。仅声明了本字段的条目注册 handler，其它 LS 行为不变。
    #[serde(default)]
    pub config_reply: Option<Vec<ConfigReplyEntry>>,
    /// per-LS 否决 pull 诊断（`textDocument/diagnostic`）：capability 推断为 true
    /// 时仍强制走 push 缓存。↖ mirror: julia_server.py@7a296833
    /// `_supports_pull_diagnostics → False`（LanguageServer.jl 对 pull 直接崩溃）。
    #[serde(default)]
    pub pull_diagnostics_denied: bool,
}

/// `[[servers.X.config_reply]]` 条目：section 精确匹配（大小写敏感，LSP
/// ConfigurationItem.section 原文）→ 真值。
#[derive(Debug, Clone, Deserialize)]
pub struct ConfigReplyEntry {
    pub section: String,
    pub value: serde_json::Value,
}

/// §2.2 A 类 download 子表。
#[derive(Debug, Deserialize)]
pub struct DownloadSpec {
    pub version: String,
    /// zip | tar.gz | tar.xz | gz | raw
    pub archive: String,
    /// per-platform `archive` 覆盖（key 词汇表同 `url_per_platform`）。平台间压缩
    /// 形态互异时使用（首例 kotlin：win=zip / linux=tar.gz）；miss 回退单值
    /// `archive`，解析统一走 [`Self::resolved_archive`]。
    #[serde(default)]
    pub archive_per_platform: HashMap<String, String>,
    #[serde(default)]
    pub strip_components: usize,
    /// 压缩包内可执行相对路径（raw 形态 = 落盘文件名）。
    ///
    /// 默认值（windows 组语义不变）；平台互异路径用 [`Self::bin_path_per_platform`]
    /// 覆盖，解析统一走 [`Self::resolved_bin_path`]。
    pub bin_path: String,
    /// per-platform `bin_path` 覆盖（key 词汇表同 `url_per_platform` = platform_key
    /// 输出全集）。命中该平台优先；miss 回退单值 `bin_path`。省略 = 全平台同单值。
    #[serde(default)]
    pub bin_path_per_platform: HashMap<String, String>,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    pub url_per_platform: HashMap<String, String>,
    /// 空串/缺平台 = sha 未知（§2.9 → UnsignedRefused）。
    pub sha256_per_platform: HashMap<String, String>,
}

impl DownloadSpec {
    /// per-platform `bin_path` 解析统一入口（映射/缓存命中/doctor 探测共用）：
    /// `bin_path_per_platform[platform_key]` 命中优先，miss 回退单值 `bin_path`。
    pub fn resolved_bin_path<'a>(&'a self, platform_key: &str) -> &'a str {
        self.bin_path_per_platform
            .get(platform_key)
            .map(String::as_str)
            .unwrap_or(&self.bin_path)
    }

    /// per-platform `archive` 解析统一入口：命中优先，miss 回退单值 `archive`。
    pub fn resolved_archive<'a>(&'a self, platform_key: &str) -> &'a str {
        self.archive_per_platform
            .get(platform_key)
            .map(String::as_str)
            .unwrap_or(&self.archive)
    }
}

/// §2.7 F 类 path_only 子表。
#[derive(Debug, Deserialize)]
pub struct PathOnlySpec {
    pub binary_name: String,
    pub install_hint: String,
}

/// npm 子表：安装期 `npm install --prefix {cache}/{id}/{version} <pkg>[@<ver>] ...`，
/// 启动 = `node_modules/.bin/<bin_rel>` + `npm_args`。
#[derive(Debug, Deserialize)]
pub struct NpmSpec {
    pub package: String,
    /// 省略 = latest（缓存目录名 `latest`，不锁版本）。
    pub version: Option<String>,
    /// `node_modules/.bin/` 下的相对名（如 `bash-language-server`）。
    pub bin_rel: String,
    /// 启动时追加在 bin 之后的参数（如 `["--stdio"]`）。
    pub npm_args: Option<Vec<String>>,
    /// 伴随包（同一次 install 装齐，npm hoist 后与主包同 node_modules；design §2.3）。
    /// 如 typescript-language-server 需要 `typescript`、svelte 需要 ts-plugin。
    #[serde(default)]
    pub secondary_packages: Vec<SecondaryPackage>,
}

/// npm 伴随包（`package` 必填；`version` 省略 = latest）。
#[derive(Debug, Deserialize)]
pub struct SecondaryPackage {
    pub package: String,
    pub version: Option<String>,
}

/// uvx 子表：无安装步骤，launch = `uvx --from <pkg>[==<ver>] <entrypoint> <args>`
/// （uv 运行时自管缓存）。
#[derive(Debug, Deserialize)]
pub struct UvxSpec {
    pub package: String,
    /// 省略 = 不锁版本（`--from <pkg>`）。
    pub version: Option<String>,
    /// uvx 运行的入口命令名。
    pub entrypoint: String,
    /// 启动时追加在 entrypoint 之后的参数。
    pub args: Option<Vec<String>>,
}

/// dotnet tool 子表：安装期 `dotnet tool install --tool-path {cache}/{id}/{version}
/// <tool> [--version <ver>]`（上游 fsharp 适配器形态），启动 = `{dir}/<tool>[.exe]` + `args`。
#[derive(Debug, Deserialize)]
pub struct DotnetSpec {
    pub tool: String,
    /// 省略 = latest（缓存目录名 `latest`）。
    pub version: Option<String>,
    /// 启动时追加在 tool 之后的参数（如 fsautocomplete 的 LSP 开关组）。
    pub args: Option<Vec<String>>,
}

/// gem 子表：安装期 `gem install --user-install --bindir {cache}/{id}/{version}/bin
/// <gem> [-v <ver>]`，启动 = `{bindir}/<bin_rel>[.cmd/.bat]` + `args`。
#[derive(Debug, Deserialize)]
pub struct GemSpec {
    pub gem: String,
    /// 省略 = latest（缓存目录名 `latest`）。
    pub version: Option<String>,
    /// bindir 下的可执行名（如 `ruby-lsp` / `solargraph`）。
    pub bin_rel: String,
    /// 启动时追加在 bin 之后的参数（如 solargraph 的 `["stdio"]`）。
    pub args: Option<Vec<String>>,
}

/// 源码构建子表：`git clone --depth 1 <repo> src`（+ 可选 `--branch <pin>`）后
/// 在 clone 根执行单步 `build_cmd`，启动 = `src/<bin_rel>`。
#[derive(Debug, Deserialize)]
pub struct SourceSpec {
    /// https git URL（如 `https://github.com/nix-community/nixd`）。
    pub repo: String,
    /// tag/branch pin（省略 = 默认分支 HEAD，缓存目录名 `default`）。
    pub pin: Option<String>,
    /// 单步构建命令（cwd = clone 根；首元素 = 程序名）。
    pub build_cmd: Vec<String>,
    /// 构建产物相对 clone 根的路径。
    pub bin_rel: String,
}

/// 安装方式子表引用（ensure/映射分支用）。
pub enum KindRef<'a> {
    Download(&'a DownloadSpec),
    PathOnly(&'a PathOnlySpec),
    Npm(&'a NpmSpec),
    Uvx(&'a UvxSpec),
    Dotnet(&'a DotnetSpec),
    Gem(&'a GemSpec),
    Source(&'a SourceSpec),
}

impl ServerSpec {
    pub fn kind_table(&self) -> Option<KindRef<'_>> {
        match self.install.as_str() {
            "download" => self.download.as_ref().map(KindRef::Download),
            "path_only" => self.path_only.as_ref().map(KindRef::PathOnly),
            "npm" => self.npm.as_ref().map(KindRef::Npm),
            "uvx" => self.uvx.as_ref().map(KindRef::Uvx),
            "dotnet" => self.dotnet.as_ref().map(KindRef::Dotnet),
            "gem" => self.gem.as_ref().map(KindRef::Gem),
            "source" => self.source.as_ref().map(KindRef::Source),
            _ => None,
        }
    }

    /// spawn env 注入表 → `LaunchInfo.env`。未声明 `env` → 空 Vec（现行为）。
    ///
    /// `PATH` 键（大小写不敏感，Windows 环境变量语义）按平台分隔符追加在原值后
    /// —— `Command::envs` 是覆盖语义，此处需显式带原值拼接；其余键直接设置。
    pub fn spawn_env(&self) -> Vec<(String, String)> {
        let Some(env) = &self.env else {
            return Vec::new();
        };
        env.iter()
            .map(|(k, v)| {
                if k.eq_ignore_ascii_case("PATH") {
                    let sep = if cfg!(windows) { ";" } else { ":" };
                    let orig = std::env::var("PATH").unwrap_or_default();
                    let merged = if orig.is_empty() {
                        v.clone()
                    } else {
                        format!("{orig}{sep}{v}")
                    };
                    (k.clone(), merged)
                } else {
                    (k.clone(), v.clone())
                }
            })
            .collect()
    }
}

/// 解析 + 逐条校验。坏 toml / 必填缺失 → `InvalidSpec` 语义错误串。
pub fn parse(toml_str: &str) -> Result<ServersToml, String> {
    let parsed: ServersToml =
        toml::from_str(toml_str).map_err(|e| format!("servers.toml parse: {e}"))?;
    for (id, spec) in &parsed.servers {
        validate(id, spec)?;
    }
    Ok(parsed)
}

/// 必填字段交叉校验（design §2：install 类别决定子表存在性）。
fn validate(id: &str, spec: &ServerSpec) -> Result<(), String> {
    match spec.install.as_str() {
        "download" => {
            let dl = spec.download.as_ref().ok_or_else(|| {
                format!("[servers.{id}]: install=download requires [servers.{id}.download] table")
            })?;
            for (plat, url) in &dl.url_per_platform {
                if !url.starts_with("https://") {
                    return Err(format!(
                        "[servers.{id}].download.url_per_platform.{plat}: HTTPS only"
                    ));
                }
            }
            if dl.bin_path.is_empty() {
                return Err(format!(
                    "[servers.{id}].download.bin_path must not be empty"
                ));
            }
            for (plat, bin) in &dl.bin_path_per_platform {
                if bin.is_empty() {
                    return Err(format!(
                        "[servers.{id}].download.bin_path_per_platform.{plat} must not be empty"
                    ));
                }
            }
            for (plat, archive) in &dl.archive_per_platform {
                if archive.is_empty() {
                    return Err(format!(
                        "[servers.{id}].download.archive_per_platform.{plat} must not be empty"
                    ));
                }
            }
        }
        "path_only" => {
            if spec.path_only.is_none() {
                return Err(format!(
                    "[servers.{id}]: install=path_only requires [servers.{id}.path_only] table"
                ));
            }
        }
        "npm" => {
            let npm = spec.npm.as_ref().ok_or_else(|| {
                format!("[servers.{id}]: install=npm requires [servers.{id}.npm] table")
            })?;
            if npm.package.is_empty() {
                return Err(format!("[servers.{id}].npm.package must not be empty"));
            }
            if npm.bin_rel.is_empty() {
                return Err(format!("[servers.{id}].npm.bin_rel must not be empty"));
            }
            for sec in &npm.secondary_packages {
                if sec.package.is_empty() {
                    return Err(format!(
                        "[servers.{id}].npm.secondary_packages: package must not be empty"
                    ));
                }
            }
        }
        "uvx" => {
            let uvx = spec.uvx.as_ref().ok_or_else(|| {
                format!("[servers.{id}]: install=uvx requires [servers.{id}.uvx] table")
            })?;
            if uvx.package.is_empty() {
                return Err(format!("[servers.{id}].uvx.package must not be empty"));
            }
            if uvx.entrypoint.is_empty() {
                return Err(format!("[servers.{id}].uvx.entrypoint must not be empty"));
            }
        }
        "dotnet" => {
            let d = spec.dotnet.as_ref().ok_or_else(|| {
                format!("[servers.{id}]: install=dotnet requires [servers.{id}.dotnet] table")
            })?;
            if d.tool.is_empty() {
                return Err(format!("[servers.{id}].dotnet.tool must not be empty"));
            }
        }
        "gem" => {
            let g = spec.gem.as_ref().ok_or_else(|| {
                format!("[servers.{id}]: install=gem requires [servers.{id}.gem] table")
            })?;
            if g.gem.is_empty() {
                return Err(format!("[servers.{id}].gem.gem must not be empty"));
            }
            if g.bin_rel.is_empty() {
                return Err(format!("[servers.{id}].gem.bin_rel must not be empty"));
            }
        }
        "source" => {
            let s = spec.source.as_ref().ok_or_else(|| {
                format!("[servers.{id}]: install=source requires [servers.{id}.source] table")
            })?;
            if !s.repo.starts_with("https://") {
                return Err(format!("[servers.{id}].source.repo: HTTPS only"));
            }
            if s.build_cmd.is_empty() || s.build_cmd[0].is_empty() {
                return Err(format!(
                    "[servers.{id}].source.build_cmd must list at least the program name"
                ));
            }
            if s.bin_rel.is_empty() {
                return Err(format!("[servers.{id}].source.bin_rel must not be empty"));
            }
        }
        other => {
            return Err(format!(
                "[servers.{id}]: unknown install kind `{other}` (supported: download, path_only, npm, uvx, dotnet, gem, source)"
            ));
        }
    }
    if spec.languages.is_empty() {
        return Err(format!("[servers.{id}]: languages must not be empty"));
    }
    // exec 可空 = 裸启动默认（expand_exec 空模板 → [{bin}]）。
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
[servers.marksman]
languages = ["markdown"]
extensions = [".md"]
install = "download"
exec = ["{bin}", "lsp"]
source_commit = "43ae0211"

[servers.marksman.download]
version = "2026-02-08"
archive = "raw"
bin_path = "marksman.exe"
allowed_hosts = ["github.com", "release-assets.githubusercontent.com"]

[servers.marksman.download.url_per_platform]
"windows-x86_64" = "https://github.com/artempyanykh/marksman/releases/download/2026-02-08/marksman.exe"

[servers.marksman.download.sha256_per_platform]
"windows-x86_64" = "a6d05beb08ebe41b0a9f09c98a438540421436fa5531424c22e0bb1d22529705"

[servers.crystalline]
languages = ["crystal"]
extensions = [".cr"]
install = "path_only"
exec = ["{bin}"]

[servers.crystalline.path_only]
binary_name = "crystalline"
install_hint = "crystalshards: https://github.com/elbywan/crystalline"

[servers.npm_ls]
languages = ["js-fake"]
install = "npm"
source_commit = "43ae0211"

[servers.npm_ls.npm]
package = "@fake/some-ls"
version = "1.2.3"
bin_rel = "some-ls"
npm_args = ["--stdio"]

[servers.uvx_ls]
languages = ["py-fake"]
install = "uvx"
source_commit = "43ae0211"

[servers.uvx_ls.uvx]
package = "fake-ls"
version = "0.9.0"
entrypoint = "fake-ls"
args = ["-v"]
"#;

    #[test]
    fn parses_valid_spec() {
        let parsed = parse(GOOD).expect("valid toml must parse");
        assert_eq!(parsed.servers.len(), 4);
        let m = &parsed.servers["marksman"];
        assert_eq!(m.install, "download");
        assert_eq!(m.download.as_ref().unwrap().version, "2026-02-08");
        assert_eq!(m.exec, vec!["{bin}", "lsp"]);
        let c = &parsed.servers["crystalline"];
        assert_eq!(c.path_only.as_ref().unwrap().binary_name, "crystalline");
    }

    #[test]
    fn parses_npm_and_uvx_subtables() {
        let parsed = parse(GOOD).expect("valid toml must parse");
        let n = parsed.servers["npm_ls"].npm.as_ref().unwrap();
        assert_eq!(n.package, "@fake/some-ls");
        assert_eq!(n.version.as_deref(), Some("1.2.3"));
        assert_eq!(n.bin_rel, "some-ls");
        assert_eq!(n.npm_args, Some(vec!["--stdio".to_string()]));
        let u = parsed.servers["uvx_ls"].uvx.as_ref().unwrap();
        assert_eq!(u.package, "fake-ls");
        assert_eq!(u.version.as_deref(), Some("0.9.0"));
        assert_eq!(u.entrypoint, "fake-ls");
        assert_eq!(u.args, Some(vec!["-v".to_string()]));
        // kind_table 路由到新变体。
        assert!(matches!(
            parsed.servers["npm_ls"].kind_table(),
            Some(crate::spec::KindRef::Npm(_))
        ));
        assert!(matches!(
            parsed.servers["uvx_ls"].kind_table(),
            Some(crate::spec::KindRef::Uvx(_))
        ));
    }

    #[test]
    fn npm_missing_table_or_fields_is_invalid() {
        let missing = r#"
[servers.broken]
languages = ["x"]
install = "npm"
"#;
        let err = parse(missing).unwrap_err();
        assert!(err.contains("requires [servers.broken.npm]"), "err: {err}");
        let empty_bin_rel = r#"
[servers.broken]
languages = ["x"]
install = "npm"
[servers.broken.npm]
package = "some-ls"
bin_rel = ""
"#;
        let err = parse(empty_bin_rel).unwrap_err();
        assert!(err.contains("bin_rel must not be empty"), "err: {err}");
    }

    #[test]
    fn uvx_missing_table_or_fields_is_invalid() {
        let missing = r#"
[servers.broken]
languages = ["x"]
install = "uvx"
"#;
        let err = parse(missing).unwrap_err();
        assert!(err.contains("requires [servers.broken.uvx]"), "err: {err}");
        let empty_entrypoint = r#"
[servers.broken]
languages = ["x"]
install = "uvx"
[servers.broken.uvx]
package = "some-ls"
entrypoint = ""
"#;
        let err = parse(empty_entrypoint).unwrap_err();
        assert!(err.contains("entrypoint must not be empty"), "err: {err}");
    }

    #[test]
    fn missing_download_table_is_invalid() {
        let bad = r#"
[servers.broken]
languages = ["x"]
extensions = [".x"]
install = "download"
exec = ["{bin}"]
"#;
        let err = parse(bad).unwrap_err();
        assert!(
            err.contains("requires [servers.broken.download]"),
            "err: {err}"
        );
    }

    #[test]
    fn unknown_install_kind_is_invalid() {
        let bad = r#"
[servers.weird]
languages = ["x"]
extensions = [".x"]
install = "cargo"
exec = ["{bin}"]
"#;
        let err = parse(bad).unwrap_err();
        assert!(err.contains("unknown install kind"), "err: {err}");
    }

    #[test]
    fn non_https_url_is_rejected() {
        let bad = r#"
[servers.insecure]
languages = ["x"]
extensions = [".x"]
install = "download"
exec = ["{bin}"]

[servers.insecure.download]
version = "1"
archive = "raw"
bin_path = "b"

[servers.insecure.download.url_per_platform]
"windows-x86_64" = "http://github.com/x"

[servers.insecure.download.sha256_per_platform]
"windows-x86_64" = "a6d05beb08ebe41b0a9f09c98a438540421436fa5531424c22e0bb1d22529705"
"#;
        let err = parse(bad).unwrap_err();
        assert!(err.contains("HTTPS only"), "err: {err}");
    }

    #[test]
    fn malformed_toml_is_parse_error() {
        assert!(parse("not [ valid toml").is_err());
    }

    const EXTRA_KINDS: &str = r#"
[servers.dn]
languages = ["fsharp-fake"]
install = "dotnet"
source_commit = "43ae0211"

[servers.dn.dotnet]
tool = "fsautocomplete"
version = "0.83.0"
args = ["--stdio"]

[servers.gm]
languages = ["ruby-fake"]
install = "gem"
source_commit = "43ae0211"

[servers.gm.gem]
gem = "ruby-lsp"
version = "0.26.8"
bin_rel = "ruby-lsp"

[servers.src]
languages = ["nix-fake"]
install = "source"
source_commit = "43ae0211"

[servers.src.source]
repo = "https://github.com/nix-community/nixd"
build_cmd = ["nix", "build"]
bin_rel = "result/bin/nixd"

[servers.npm_sec]
languages = ["ts-fake"]
install = "npm"

[servers.npm_sec.npm]
package = "typescript-language-server"
version = "5.1.3"
bin_rel = "typescript-language-server"
npm_args = ["--stdio"]

[[servers.npm_sec.npm.secondary_packages]]
package = "typescript"
version = "5.9.3"
"#;

    #[test]
    fn parses_dotnet_gem_source_subtables() {
        let parsed = parse(EXTRA_KINDS).expect("valid toml must parse");
        assert_eq!(parsed.servers.len(), 4);
        let d = parsed.servers["dn"].dotnet.as_ref().unwrap();
        assert_eq!(d.tool, "fsautocomplete");
        assert_eq!(d.version.as_deref(), Some("0.83.0"));
        assert_eq!(d.args, Some(vec!["--stdio".to_string()]));
        let g = parsed.servers["gm"].gem.as_ref().unwrap();
        assert_eq!(g.gem, "ruby-lsp");
        assert_eq!(g.version.as_deref(), Some("0.26.8"));
        assert_eq!(g.bin_rel, "ruby-lsp");
        assert!(g.args.is_none(), "gem args 可省 = 裸启动");
        let s = parsed.servers["src"].source.as_ref().unwrap();
        assert_eq!(s.repo, "https://github.com/nix-community/nixd");
        assert!(s.pin.is_none(), "pin 可省 = 默认分支");
        assert_eq!(s.build_cmd, vec!["nix", "build"]);
        assert_eq!(s.bin_rel, "result/bin/nixd");
        // kind_table 路由到新变体。
        assert!(matches!(
            parsed.servers["dn"].kind_table(),
            Some(KindRef::Dotnet(_))
        ));
        assert!(matches!(
            parsed.servers["gm"].kind_table(),
            Some(KindRef::Gem(_))
        ));
        assert!(matches!(
            parsed.servers["src"].kind_table(),
            Some(KindRef::Source(_))
        ));
    }

    #[test]
    fn parses_npm_secondary_packages() {
        let parsed = parse(EXTRA_KINDS).expect("valid toml must parse");
        let n = parsed.servers["npm_sec"].npm.as_ref().unwrap();
        assert_eq!(n.secondary_packages.len(), 1);
        assert_eq!(n.secondary_packages[0].package, "typescript");
        assert_eq!(n.secondary_packages[0].version.as_deref(), Some("5.9.3"));
    }

    #[test]
    fn new_kind_missing_table_or_fields_is_invalid() {
        for (toml, needle) in [
            (
                r#"
[servers.broken]
languages = ["x"]
install = "dotnet"
"#,
                "requires [servers.broken.dotnet]",
            ),
            (
                r#"
[servers.broken]
languages = ["x"]
install = "gem"
[servers.broken.gem]
gem = "g"
bin_rel = ""
"#,
                "bin_rel must not be empty",
            ),
            (
                r#"
[servers.broken]
languages = ["x"]
install = "source"
[servers.broken.source]
repo = "http://github.com/x/y"
build_cmd = ["make"]
bin_rel = "b"
"#,
                "HTTPS only",
            ),
            (
                r#"
[servers.broken]
languages = ["x"]
install = "source"
[servers.broken.source]
repo = "https://github.com/x/y"
build_cmd = []
bin_rel = "b"
"#,
                "build_cmd must list at least the program name",
            ),
        ] {
            let err = parse(toml).unwrap_err();
            assert!(err.contains(needle), "want `{needle}` in err: {err}");
        }
    }

    // ---- 上游对拍采纳 Wave 1：T0 三通道字段 ----

    #[test]
    fn parses_wave1_tuning_channels() {
        let toml = r#"
[servers.tuned]
languages = ["x"]
install = "path_only"
init_options = { yaml = { schemaStore = { enable = true } }, top = 1 }
did_change_config = {}
pull_diagnostics_denied = true

[servers.tuned.env]
JAVA_TOOL_OPTIONS = "-Xmx2G"

[[servers.tuned.config_reply]]
section = "perl"
value = { perlInc = ["lib"] }

[[servers.tuned.config_reply]]
section = "other"
value = "v"

[servers.tuned.path_only]
binary_name = "b"
install_hint = "h"
"#;
        let parsed = parse(toml).expect("valid toml must parse");
        let spec = &parsed.servers["tuned"];
        // init_options：JSON 片（inline table）逐键。
        let init = spec.init_options.as_ref().unwrap();
        assert_eq!(
            init.get("yaml")
                .unwrap()
                .get("schemaStore")
                .unwrap()
                .get("enable"),
            Some(&serde_json::Value::Bool(true))
        );
        assert_eq!(init.get("top"), Some(&serde_json::json!(1)));
        // env 子表。
        let env = spec.env.as_ref().unwrap();
        assert_eq!(
            env.get("JAVA_TOOL_OPTIONS").map(String::as_str),
            Some("-Xmx2G")
        );
        // did_change_config = {} 是 Some(空对象) 而非 None（julia nudge 语义）。
        assert_eq!(
            spec.did_change_config,
            Some(serde_json::Value::Object(Default::default()))
        );
        assert!(spec.pull_diagnostics_denied, "显式 true 生效");
        // config_reply 数组：section 精确 + 任意 JSON 值。
        let replies = spec.config_reply.as_ref().unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].section, "perl");
        assert_eq!(
            replies[0].value.get("perlInc").unwrap().get(0),
            Some(&serde_json::json!("lib"))
        );
        assert_eq!(replies[1].section, "other");
        assert_eq!(replies[1].value, serde_json::json!("v"));
    }

    /// 向后兼容硬约束：无新字段的条目，五个通道全为默认值（解析结果与引入前一致）。
    #[test]
    fn wave1_fields_default_when_absent() {
        let toml = r#"
[servers.plain]
languages = ["x"]
install = "path_only"
[servers.plain.path_only]
binary_name = "b"
install_hint = "h"
"#;
        let parsed = parse(toml).expect("valid toml must parse");
        let spec = &parsed.servers["plain"];
        assert!(spec.init_options.is_none());
        assert!(spec.env.is_none());
        assert!(spec.did_change_config.is_none());
        assert!(spec.config_reply.is_none());
        assert!(!spec.pull_diagnostics_denied);
        assert!(spec.spawn_env().is_empty(), "未声明 env = 空 Vec（现行为）");
    }

    #[test]
    fn spawn_env_sets_plain_keys_and_appends_path() {
        let toml = r#"
[servers.e]
languages = ["x"]
install = "path_only"
[servers.e.env]
JAVA_TOOL_OPTIONS = "-Xmx2G"
PATH = "/managed/bin"
path = "/lower/bin"
[servers.e.path_only]
binary_name = "b"
install_hint = "h"
"#;
        let parsed = parse(toml).expect("valid toml must parse");
        let env: std::collections::HashMap<String, String> =
            parsed.servers["e"].spawn_env().into_iter().collect();
        // 非 PATH 键：直接设置。
        assert_eq!(
            env.get("JAVA_TOOL_OPTIONS").map(String::as_str),
            Some("-Xmx2G")
        );
        // PATH 键（大小写不敏感归一）：原值在前、注入值在后（平台分隔符）。
        let sep = if cfg!(windows) { ";" } else { ":" };
        let upper = env.get("PATH").expect("PATH 键存在");
        let orig = std::env::var("PATH").unwrap_or_default();
        assert!(
            upper.ends_with(&format!("{sep}/managed/bin")),
            "PATH 尾部为注入值: {upper}"
        );
        if !orig.is_empty() {
            assert!(
                upper.starts_with(&orig),
                "原 PATH 在前（不被截断/覆盖丢失）"
            );
        }
        // 两个 PATH 变体键（PATH + path）各自独立追注入值——覆盖语义按 toml 键
        // 逐条生效（spawn 端 envs 覆盖由 OS 大小写不敏感合并，此处只验追加形态）。
        assert!(
            env.get("path")
                .unwrap()
                .ends_with(&format!("{sep}/lower/bin"))
        );
    }
}
