//! `servers.toml` schema（auto-install-design.md v0.4 §2）。
//!
//! 首批（Task 19）：A 类 download（marksman）+ F 类 path_only（crystalline）。
//! npm/uvx/dotnet/gem/特殊形态（§2.3-2.6/2.8）随 Task 20 批量收录扩充。
//!
//! platform key 规范：`"{os}-{arch}"`（windows-x86_64 / linux-x86_64 /
//! macos-x86_64 / macos-aarch64），与 design §2.2 一致。

use std::collections::HashMap;

use serde::Deserialize;

/// `servers.toml` 顶层（include_str! 内置单表，v1 无外部覆盖，design §0 非目标）。
#[derive(Debug, Deserialize)]
pub struct ServersToml {
    pub servers: HashMap<String, ServerSpec>,
}

/// §2.1 通用字段 + 安装方式子表。
#[derive(Debug, Deserialize)]
pub struct ServerSpec {
    pub languages: Vec<String>,
    /// 文件扩展名（信息性；文件探测实际归 ls-registry EXT_TABLE，path_only 可省）。
    #[serde(default)]
    pub extensions: Vec<String>,
    /// download | path_only（npm/uvx/... Task 20 扩）
    pub install: String,
    /// 含 `{bin}` 占位符的启动模板（如 `["{bin}", "lsp"]`）；省略 = 裸启动 `[{bin}]`。
    #[serde(default)]
    pub exec: Vec<String>,
    pub download: Option<DownloadSpec>,
    pub path_only: Option<PathOnlySpec>,
    /// 数据漂移锚（design §8 风险表）：本条目抄译自上游的 commit。
    pub source_commit: Option<String>,
}

/// §2.2 A 类 download 子表。
#[derive(Debug, Deserialize)]
pub struct DownloadSpec {
    pub version: String,
    /// zip | tar.gz | tar.xz | gz | raw
    pub archive: String,
    #[serde(default)]
    pub strip_components: usize,
    /// 压缩包内可执行相对路径（raw 形态 = 落盘文件名）。
    pub bin_path: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    pub url_per_platform: HashMap<String, String>,
    /// 空串/缺平台 = sha 未知（§2.9 → UnsignedRefused）。
    pub sha256_per_platform: HashMap<String, String>,
}

/// §2.7 F 类 path_only 子表。
#[derive(Debug, Deserialize)]
pub struct PathOnlySpec {
    pub binary_name: String,
    pub install_hint: String,
}

/// 安装方式子表引用（ensure/映射分支用）。
pub enum KindRef<'a> {
    Download(&'a DownloadSpec),
    PathOnly(&'a PathOnlySpec),
}

impl ServerSpec {
    pub fn kind_table(&self) -> Option<KindRef<'_>> {
        match self.install.as_str() {
            "download" => self.download.as_ref().map(KindRef::Download),
            "path_only" => self.path_only.as_ref().map(KindRef::PathOnly),
            _ => None,
        }
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
                return Err(format!("[servers.{id}].download.bin_path must not be empty"));
            }
        }
        "path_only" => {
            if spec.path_only.is_none() {
                return Err(format!(
                    "[servers.{id}]: install=path_only requires [servers.{id}.path_only] table"
                ));
            }
        }
        other => {
            return Err(format!(
                "[servers.{id}]: unknown install kind `{other}` (supported: download, path_only)"
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
"#;

    #[test]
    fn parses_valid_spec() {
        let parsed = parse(GOOD).expect("valid toml must parse");
        assert_eq!(parsed.servers.len(), 2);
        let m = &parsed.servers["marksman"];
        assert_eq!(m.install, "download");
        assert_eq!(m.download.as_ref().unwrap().version, "2026-02-08");
        assert_eq!(m.exec, vec!["{bin}", "lsp"]);
        let c = &parsed.servers["crystalline"];
        assert_eq!(c.path_only.as_ref().unwrap().binary_name, "crystalline");
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
        assert!(err.contains("requires [servers.broken.download]"), "err: {err}");
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
}
