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
use ls_runtime::install_pkg::npm_bin_path;

/// serena npm 缓存目录 pin（= servers.toml [servers.typescript_vts] npm 段，禁随意改；
/// 升版 = 缓存目录键换新，servers.toml version 与本常量同改）。
const CACHE_ID: &str = "typescript_vts";
const CACHE_VERSION: &str = "0.3.0";

#[derive(Debug, Default, Clone, Copy)]
pub struct VtsAdapter;

/// typescript `lib/`（tsdk）：tsserver 程序与内置 lib 的根（与 vtsls 同 node_modules，
/// servers.toml secondary_packages 保证同装）。
fn cached_tsdk() -> PathBuf {
    default_cache_root()
        .join(CACHE_ID)
        .join(CACHE_VERSION)
        .join("node_modules/typescript/lib")
}

/// vtsls 解析：serena npm 缓存命中优先 → PATH（用户全局自装）→ 标准未安装错误。
/// 缓存命中必须走 [`npm_bin_path`]：npm 的裸名 bin 在 Windows 是 sh 脚本，
/// CreateProcess 直接 spawn 报 os error 193 "%1 is not a valid Win32 application"
/// （run 36670130529 实锚）——Windows 只认 `.cmd` shim（json/css/bash 适配器同约束）。
fn resolve_vtsls() -> anyhow::Result<PathBuf> {
    let install = default_cache_root().join(CACHE_ID).join(CACHE_VERSION);
    if let Some(exe) = npm_bin_path(&install, "vtsls") {
        return Ok(exe);
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

    /// R2 spawn 修复接线（run 36670130529 实锚 os error 193）：缓存布局下
    /// resolve 必须落 npm_bin_path 的平台正确 shim——Windows 造了 .cmd 就返回
    /// .cmd（裸名是 sh 脚本不可 spawn），Unix 返回裸名。env 注入 + 还原对齐
    /// powershell.rs 测试先例；which 分支（用户全局自装）不在本测（PATH 依赖）。
    /// 全程持锁串行：env 注入与并行测试的 env 读互踩（gopls/typescript.rs 同款锁）。
    static RESOLVE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn resolve_prefers_platform_shim_from_cache() {
        let _seq = RESOLVE_TEST_LOCK
            .lock()
            .expect("RESOLVE_TEST_LOCK poisoned");
        // bd 83f：跨 adapter 的 env 写互斥——本测的 PATH/LOCALAPPDATA 注入
        // 与 deno/powershell 等并发 set_var 互踩（9ai 假红同根因）。
        let _env = crate::ENV_TEST_LOCK.blocking_lock();
        let cache_dir = tempfile::tempdir().expect("tempdir");
        // default_cache_root = {LOCALAPPDATA}/serena/ls（unix ~/.local/share/serena/ls）
        // ——注入的是 LOCALAPPDATA/HOME 本体，布局要补 serena/ls 段（powershell.rs 同款）。
        let cache_root = if cfg!(windows) {
            cache_dir.path().join("serena/ls")
        } else {
            cache_dir.path().join(".local/share/serena/ls")
        };
        let bin_dir = cache_root
            .join(CACHE_ID)
            .join(CACHE_VERSION)
            .join("node_modules/.bin");
        std::fs::create_dir_all(&bin_dir).expect("mkdir .bin");
        let home_key = if cfg!(windows) {
            "LOCALAPPDATA"
        } else {
            "HOME"
        };
        let home_original = std::env::var_os(home_key);
        let path_original = std::env::var_os("PATH");
        // SAFETY: 单线程测试内注入 + 末尾还原；PATH 指空目录屏蔽 which 分支命中
        // 真机 vtsls 的干扰。
        unsafe {
            std::env::set_var(home_key, cache_dir.path());
            std::env::set_var("PATH", cache_dir.path());
        }
        // 只有裸名：Windows 视为不可 spawn 的 sh 脚本 → 落 which（空 PATH）→ Err。
        std::fs::write(bin_dir.join("vtsls"), "").expect("write bare shim");
        let bare_only = resolve_vtsls();
        // 补 .cmd shim：Windows 必须返回 .cmd；Unix 必须仍返回裸名。
        std::fs::write(bin_dir.join("vtsls.cmd"), "").expect("write cmd shim");
        let with_cmd = resolve_vtsls();
        unsafe {
            match &path_original {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
            match &home_original {
                Some(p) => std::env::set_var(home_key, p),
                None => std::env::remove_var(home_key),
            }
        }
        if cfg!(windows) {
            assert!(
                bare_only.is_err(),
                "裸名 sh 脚本不可 spawn（os error 193 实锚），不得命中: {bare_only:?}"
            );
            assert_eq!(
                with_cmd.expect("cmd shim resolved"),
                bin_dir.join("vtsls.cmd"),
                "Windows 必须解析到 .cmd shim"
            );
        } else {
            assert_eq!(
                bare_only.expect("bare resolved"),
                bin_dir.join("vtsls"),
                "Unix 裸名可直接 spawn"
            );
            assert_eq!(
                with_cmd.expect("bare still resolved"),
                bin_dir.join("vtsls"),
                "Unix 不受 .cmd 影响"
            );
        }
    }
}
