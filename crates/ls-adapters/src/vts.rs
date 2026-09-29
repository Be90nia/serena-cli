//! @vtsls/language-server 适配器（smoke R4 修红）。
//!
//! ↖ mirror: oraios/serena@7a296833 `solidlsp/language_servers/vts_language_server.py`
//! （上游 `_setup_runtime_dependencies` 返回 `"{path} --stdio"`——裸启动 = CLI 帮助即退
//! → stdout pump EOF，CI 首跑实锤；显式 `--stdio` 必需）。
//!
//! ## 路由
//! 变体门（pgsql/mysql 先例）：`--lang typescript_vts` 经 spec_for 命中
//! [servers.typescript_vts] 条目；adapter_for 字符串直路由到本 adapter——typescript_vts
//! 是 LS 变体不是语言，不进 LanguageId 枚举，[`LanguageServerAdapter::languages`]
//! 归 TypeScript（探针按 .ts 找文件与 vtsls 消费的文档同族）。
//!
//! ## tsdk 注入（本 adapter 存在的唯一理由）
//! vtsls 对裸 workspace（无 node_modules/typescript）tsserver 桥整体不挂：
//! documentSymbol 报 rpc -32603 "Cannot find provider"、hover 静默
//! （run 36561318948 / 36567771452 实锤；0.3.0 secondary 装了 typescript 仍不自动发现）。
//! `initializationOptions.typescript.tsdk` 注入 serena npm 缓存同目录的
//! typescript/lib（vue/astro/svelte 同款 tsdk 先例）。缓存未命中（用户全局自装
//! vtsls）不注入——tsdk 语义交回用户 workspace（上游零注入同语义）。

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use ls_runtime::install::default_cache_root;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;
use serde_json::json;

use crate::{LanguageId, LanguageServerAdapter, ProjectCtx, not_installed_error, which_no_unc};

/// serena npm 缓存目录 pin（= servers.toml [servers.typescript_vts] npm 段，禁随意改；
/// 升版 = 缓存目录键换新，servers.toml version 与本常量同改）。
const CACHE_ID: &str = "typescript_vts";
const CACHE_VERSION: &str = "0.3.0";

#[derive(Debug, Default, Clone, Copy)]
pub struct VtsAdapter;

/// 缓存内 vtsls 入口：`{cache}/{id}/{version}/node_modules/.bin/vtsls`（ensure_launch
/// npm 缓存命中同款布局；unix = node shebang 脚本，T0 同路径已在 CI 验证可 spawn）。
fn cached_vtsls() -> PathBuf {
    default_cache_root()
        .join(CACHE_ID)
        .join(CACHE_VERSION)
        .join("node_modules/.bin/vtsls")
}

/// typescript `lib/`（tsdk）：tsserver 程序与内置 lib 的根（与 vtsls 同 node_modules，
/// servers.toml secondary_packages 保证同装）。
fn cached_tsdk() -> PathBuf {
    default_cache_root()
        .join(CACHE_ID)
        .join(CACHE_VERSION)
        .join("node_modules/typescript/lib")
}

/// vtsls 解析：serena npm 缓存命中优先 → PATH（用户全局自装）→ 标准未安装错误。
fn resolve_vtsls() -> anyhow::Result<PathBuf> {
    let cached = cached_vtsls();
    if cached.is_file() {
        return Ok(cached);
    }
    which_no_unc("vtsls").ok_or_else(|| {
        not_installed_error(
            "vtsls",
            "run `serena-cli install typescript_vts` or \
             `npm i -g @vtsls/language-server typescript` and ensure `vtsls` on PATH",
        )
    })
}

/// tsdk 注入纯函数（缓存查找之外的可测内核）：tsdk 目录在 cache 布局下存在才注入。
fn patch_tsdk(base: &mut InitializeParams, tsdk: &Path) {
    if !tsdk.is_dir() {
        return;
    }
    let opts = base.initialization_options.get_or_insert_with(|| json!({}));
    if !opts.is_object() {
        *opts = json!({});
    }
    opts["typescript"] = json!({ "tsdk": tsdk });
}

#[async_trait]
impl LanguageServerAdapter for VtsAdapter {
    fn id(&self) -> &'static str {
        "vtsls"
    }

    fn languages(&self) -> &'static [LanguageId] {
        // TS 家族变体：LanguageId 维度归 TypeScript（探针候选找 .ts 与文档同族）。
        const LANGS: &[LanguageId] = &[LanguageId::TypeScript];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = resolve_vtsls()?;
        Ok(LaunchInfo {
            cmd: vec![exe.into_os_string(), "--stdio".into()],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        patch_tsdk(base, &cached_tsdk());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn patch_writes_tsdk_for_cached_layout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tsdk = dir.path().join("node_modules/typescript/lib");
        std::fs::create_dir_all(&tsdk).expect("mkdir tsdk");
        let mut p = InitializeParams::default();
        patch_tsdk(&mut p, &tsdk);
        let opts = p.initialization_options.expect("options written");
        assert_eq!(opts["typescript"]["tsdk"], json!(tsdk));
    }

    #[test]
    fn patch_noops_without_tsdk_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("node_modules/typescript/lib");
        let mut p = InitializeParams::default();
        patch_tsdk(&mut p, &missing);
        // 裸路径不注入——vtsls 走自身默认发现（用户自装语义），不写死路径误导。
        assert!(p.initialization_options.is_none());
    }

    #[test]
    fn patch_overlays_into_existing_options() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tsdk = dir.path().join("tsdk");
        std::fs::create_dir_all(&tsdk).expect("mkdir");
        let mut p = InitializeParams {
            initialization_options: Some(json!({ "vtsls": { "autoUseWorkspaceTsdk": true } })),
            ..Default::default()
        };
        patch_tsdk(&mut p, &tsdk);
        let opts: Value = p.initialization_options.expect("options");
        assert_eq!(opts["vtsls"]["autoUseWorkspaceTsdk"], json!(true));
        assert_eq!(opts["typescript"]["tsdk"], json!(tsdk));
    }

    #[test]
    fn vts_declares_type_script_family() {
        assert_eq!(VtsAdapter.id(), "vtsls");
        assert_eq!(VtsAdapter.languages(), &[LanguageId::TypeScript]);
    }
}
