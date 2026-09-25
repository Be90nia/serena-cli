//! typescript-language-server 适配器（PLAN M3 / Task T2 第 4 个）。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/typescript_language_server.py`
//!
//! typescript-language-server 是 tsserver 的 LSP 包装（`@typescript-language-server/typescript-language-server`）。
//! 同时服务 TS + JS（同名包，区别在内部通过 `filetype` 区分）。
//!
//! 启动 quirk：
//! - 服务 TS 项目：找 `tsconfig.json` / `jsconfig.json`；不在时降级为单文件模式。
//! - `typescript-language-server` 需要 `typescript` + `tsserver` 作为 peer dep —— 装包时一并 npm i。
//!
//! 已知限制：
//! - 不实现 jsx/tsx 自动配置；默认即可。
//! - 不注入 inlayHints / semanticTokens —— M3+ 用户需要再加。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use ls_runtime::process::{LaunchInfo, TransportKind};
use lsp_types::InitializeParams;

use crate::{
    LanguageId, LanguageServerAdapter, ProjectCtx, RequestHooks, not_installed_error, which_no_unc,
};

const READY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// root 未设置 / 无候选文件时的退路：旧版虚拟探针 URI（不触发项目索引，仅保底）。
const PROBE_FALLBACK: &str = "file:///__ts_ls_ready_probe__";

/// 当前会话项目 root。adapter 是零字段单例（`Copy`）存不了实例状态 —— 会话级数据
/// 放静态槽，由 supervisor::session_for 在 `on_server_ready` 前经 `set_project_root` 写入。
static PROBE_ROOT: Mutex<Option<PathBuf>> = Mutex::new(None);

#[derive(Debug, Default, Clone, Copy)]
pub struct TypescriptLanguageServerAdapter;

#[async_trait]
impl LanguageServerAdapter for TypescriptLanguageServerAdapter {
    fn id(&self) -> &'static str {
        "typescript-language-server"
    }

    fn languages(&self) -> &'static [LanguageId] {
        // 同 adapter 服务 TS + JS（spec 通过 file_extension 走 LanguageId 维度后映射）。
        const LANGS: &[LanguageId] = &[LanguageId::TypeScript];
        LANGS
    }

    async fn launch_info(&self, ctx: &ProjectCtx) -> anyhow::Result<LaunchInfo> {
        let exe = which_no_unc("typescript-language-server").ok_or_else(|| {
            not_installed_error(
                "typescript-language-server",
                "install TypeScript LS (`npm i -g typescript typescript-language-server`) and ensure `typescript-language-server` on PATH",
            )
        })?;
        // Windows: 绕开 npm shim (无论 .cmd / .sh)。shim 会 cd 到自己所在目录
        // (npm global dir) 然后 exec node,导致 cli.mjs cwd 丢失 workspace 的
        // node_modules/typescript。直接 node + cli.mjs (cli.mjs 在 exe 同包
        // node_modules 里) —— cwd 由 LaunchInfo 的 cwd 字段交给 runtime。
        // ponytail: 此 workaround 仅 typescript-language-server,其它 .sh LS
        // (pyright / vscode-langservers-extracted) 留待真撞上再加。
        if cfg!(windows)
            && let Some(cli_mjs) = exe
                .parent()
                .map(|p| p.join("node_modules/typescript-language-server/lib/cli.mjs"))
                .filter(|p| p.is_file())
        {
            let node = which_no_unc("node").ok_or_else(|| {
                anyhow::anyhow!("node not on PATH; required to run typescript-language-server")
            })?;
            return Ok(LaunchInfo {
                cmd: vec![
                    node.into_os_string(),
                    cli_mjs.into_os_string(),
                    "--stdio".into(),
                ],
                cwd: ctx.project_root.clone(),
                env: vec![],
                transport: TransportKind::Stdio,
            });
        }
        Ok(LaunchInfo {
            cmd: vec![exe.into_os_string(), "--stdio".into()],
            cwd: ctx.project_root.clone(),
            env: vec![],
            transport: TransportKind::Stdio,
        })
    }

    fn initialize_patches(&self, base: &mut InitializeParams) {
        // ↖ mirror: typescript_language_server.py@28e866b5 `_create_base_initialize_params`
        // 关闭 ATA（Automatic Type Acquisition）：开启时 tsserver 索引期间后台从 npm
        // 拉 @types/*，拖慢启动、引入网络依赖，离线/受限机器可挂死。与上游一致只依赖
        // 项目已装类型。Δ 上游 #1990：flag 必须在 initializationOptions 顶层——
        // typescript-language-server 新版不再读 preferences 包裹层。
        let opts = base
            .initialization_options
            .get_or_insert_with(serde_json::Value::default);
        if !opts.is_object() {
            *opts = serde_json::json!({});
        }
        opts["disableAutomaticTypingAcquisition"] = serde_json::Value::Bool(true);
    }

    fn set_project_root(&self, root: &Path) {
        *PROBE_ROOT.lock().expect("PROBE_ROOT poisoned") = Some(root.to_path_buf());
    }

    async fn on_server_ready(&self, session: &lsp_core::session::Session) -> anyhow::Result<()> {
        // 探针必须用 root 下真实文件：虚拟 URI 不触发 tsserver 的项目 lazy-load，
        // 首个真实工具请求就得独自承担全量扫描（cold-start hang 同根因，
        // 见 local/cold-start-hang-diagnosis.md）。失败也返回 Ok 让 supervisor 放行。
        use serde_json::json;
        let probe = session
            .request::<serde_json::Value>(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": self.probe_uri() } }),
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
        // TypeScript LS 支持 `textDocument/implementation`（interface → class）。
        true
    }
}

impl TypescriptLanguageServerAdapter {
    /// `on_server_ready` 将发出的探针 URI：优先 tsconfig/jsconfig 旁的真实 .ts 文件
    /// （触发 tsserver 项目加载最有效）；无则退 root 通用探针；root 未设置退虚拟 URI。
    fn probe_uri(&self) -> String {
        let root = PROBE_ROOT.lock().expect("PROBE_ROOT poisoned").clone();
        match root {
            Some(root) => {
                if let Some(uri) = self.tsconfig_adjacent_probe(&root) {
                    return uri;
                }
                crate::probe_uri_for_root(&root, self.languages(), PROBE_FALLBACK)
            }
            None => PROBE_FALLBACK.to_string(),
        }
    }

    /// ↖ mirror: typescript_language_server.py@43ae021 `_find_representative_source_file`
    /// 浅层 walk root（跳 TS 专属 ignore 目录 + 文件数护栏），找 `tsconfig.json` /
    /// `jsconfig.json` 同目录的首个 `.ts`/`.tsx`（非 `.d.ts`）—— tsconfig 位置即项目
    /// 根标志，其旁文件最能触发 tsserver 的项目 lazy-load。
    fn tsconfig_adjacent_probe(&self, root: &Path) -> Option<String> {
        // 上游 TS 适配器 is_ignored_dirname 增补的三个目录（3.3 通用表之外 TS 特有集合）
        const TS_IGNORE: &[&str] = &["node_modules", "dist", "build", ".git"];
        const MAX_VISITED: usize = 500; // 护栏：异常大目录不全扫

        let mut stack = vec![(root.to_path_buf(), 0u8)];
        let mut visited = 0usize;
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut has_tsconfig = false;
            let mut dir_ts: Option<PathBuf> = None;
            for entry in entries.flatten() {
                visited += 1;
                if visited > MAX_VISITED {
                    return None; // 护栏熔断：不全扫，退通用探针
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    if depth < 6 && !TS_IGNORE.contains(&name.as_ref()) {
                        stack.push((entry.path(), depth + 1));
                    }
                } else if name == "tsconfig.json" || name == "jsconfig.json" {
                    has_tsconfig = true;
                } else if dir_ts.is_none()
                    && ((name.ends_with(".ts") && !name.ends_with(".d.ts"))
                        || name.ends_with(".tsx"))
                {
                    dir_ts = Some(entry.path());
                }
            }
            // 上游语义：只认 tsconfig 同目录的 .ts/.tsx —— 旁文件最有效触发项目加载
            if has_tsconfig {
                return dir_ts.map(|p| lsp_core::docsync::path_to_uri_str(&p));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 探针选 root 下真实文件（触发项目索引）；无候选文件退虚拟 URI（向后兼容）。
    #[test]
    fn probe_uri_real_file_then_fallback() {
        let adapter = TypescriptLanguageServerAdapter;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        adapter.set_project_root(dir.path());
        let uri = adapter.probe_uri();
        assert!(uri.starts_with("file:///"), "必须是 file URI: {uri}");
        assert!(uri.ends_with(".gitignore"), "应指向真实文件: {uri}");

        let empty = tempfile::tempdir().unwrap();
        adapter.set_project_root(empty.path());
        assert_eq!(adapter.probe_uri(), PROBE_FALLBACK);
    }

    /// ↖ mirror: `_find_representative_source_file` —— tsconfig 同目录 .ts 优先于通用探针。
    #[test]
    fn probe_uri_prefers_tsconfig_adjacent_ts() {
        let adapter = TypescriptLanguageServerAdapter;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        std::fs::write(dir.path().join("tsconfig.json"), "{}").unwrap();
        std::fs::write(dir.path().join("main.ts"), "export const x = 1;").unwrap();
        adapter.set_project_root(dir.path());
        let uri = adapter.probe_uri();
        assert!(
            uri.ends_with("main.ts"),
            "应优先 tsconfig 旁的 main.ts: {uri}"
        );
    }

    /// 无 tsconfig：退通用探针 → 语言源文件扫描优先选中 main.ts（对 tsserver
    /// 比工程标记更能触发项目加载），.gitignore 仅在无语言源文件时兜底。
    #[test]
    fn probe_uri_falls_back_without_tsconfig() {
        let adapter = TypescriptLanguageServerAdapter;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        std::fs::write(dir.path().join("main.ts"), "export const x = 1;").unwrap();
        adapter.set_project_root(dir.path());
        let uri = adapter.probe_uri();
        assert!(
            uri.ends_with("main.ts"),
            "无 tsconfig 应回退语言源文件探针: {uri}"
        );
    }

    /// node_modules 里的 tsconfig 不参与（上游 is_ignored_dirname 增补集）。
    #[test]
    fn tsconfig_probe_skips_node_modules() {
        let adapter = TypescriptLanguageServerAdapter;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        let nm = dir.path().join("node_modules/pkg");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("tsconfig.json"), "{}").unwrap();
        std::fs::write(nm.join("dep.ts"), "export const y = 2;").unwrap();
        adapter.set_project_root(dir.path());
        let uri = adapter.probe_uri();
        assert!(uri.ends_with(".gitignore"), "node_modules 应被跳过: {uri}");
    }

    /// .d.ts 与嵌套子目录 tsconfig 的组合：tsconfig-adjacent 不认 .d.ts；但通用
    /// 语言扫描仍可选它 —— .d.ts 是 root 下真实 TS 文件且紧邻 tsconfig，对
    /// tsserver 触发项目加载同样有效。
    #[test]
    fn probe_uri_ignores_d_ts() {
        let adapter = TypescriptLanguageServerAdapter;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        let sub = dir.path().join("types");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("tsconfig.json"), "{}").unwrap();
        std::fs::write(sub.join("legacy.d.ts"), "declare const z: number;").unwrap();
        adapter.set_project_root(dir.path());
        let uri = adapter.probe_uri();
        assert!(
            uri.ends_with("legacy.d.ts"),
            "tsconfig-adjacent 不认 .d.ts 后应由语言扫描兜住: {uri}"
        );
    }

    /// ↖ mirror: `_create_base_initialize_params` —— 注入关闭 ATA。
    /// Δ 上游 #1990（28e866b5）：flag 必须在 initializationOptions 顶层，
    /// TLS 新版不读 preferences 包裹层。
    #[test]
    fn initialize_patches_disables_automatic_type_acquisition() {
        let adapter = TypescriptLanguageServerAdapter;
        let mut params = InitializeParams::default();
        assert!(params.initialization_options.is_none());
        adapter.initialize_patches(&mut params);
        let opts = params
            .initialization_options
            .clone()
            .expect("应注入 initializationOptions");
        assert_eq!(
            opts["disableAutomaticTypingAcquisition"],
            serde_json::Value::Bool(true),
            "ATA 应被关闭: {opts}"
        );
        // 幂等：二次 patch 不炸不翻转
        adapter.initialize_patches(&mut params);
        assert_eq!(
            params.initialization_options.unwrap()["disableAutomaticTypingAcquisition"],
            serde_json::Value::Bool(true)
        );
    }
}
