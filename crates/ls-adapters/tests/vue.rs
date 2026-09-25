//! vue-language-server adapter 测试（Wave 2）。
//!
//! 覆盖：id / languages / launch_info 缓存形态（node + 入口 js 直跑，绕 .cmd shim）/
//! 缓存缺失 → NotInstalled 语义 / 真实 LS e2e（SERENA_SKIP_LS_E2E 门，先例
//! supervisor/tests/e2e_completion.rs）。
//!
//! e2e 全程持 `common::with_path_lock`：launch 测试会注入 LOCALAPPDATA 屏蔽真机缓存，
//! 持锁互斥避免 e2e 的 `default_cache_root()` 读到被篡改的 env（flaky 防火墙）。

mod common;

use std::str::FromStr;
use std::time::Duration;

use ls_adapters::LanguageId;
use ls_adapters::LanguageServerAdapter;
use ls_adapters::vue::VueAdapter;
use lsp_types::InitializeParams;

#[test]
fn metadata() {
    common::assert_basic_metadata(VueAdapter, "vue-language-server", &[LanguageId::Vue]);
    // hybrid 模式下 implementation 由 tsserver 桥承担。
    assert!(VueAdapter.supports_implementation());
}

/// 空缓存根 + 空 PATH → 语义化 NotInstalled 错误（supervisor session_for 按
/// "not found in PATH" 归类 LS_NOT_INSTALLED，install hint 带给 agent）。
#[tokio::test]
async fn launch_errors_without_cache_or_node() {
    let err_holder: std::sync::Arc<
        std::sync::Mutex<Option<anyhow::Result<ls_runtime::process::LaunchInfo>>>,
    > = std::sync::Arc::new(std::sync::Mutex::new(None));
    let err_holder_c = err_holder.clone();
    common::with_path_lock(move || {
        let err_holder_c = err_holder_c.clone();
        async move {
            let dir = tempfile::tempdir().expect("tempdir");
            let cache_dir = tempfile::tempdir().expect("cache tempdir");
            let path_original = std::env::var_os("PATH").unwrap_or_default();
            let home_key = if cfg!(windows) {
                "LOCALAPPDATA"
            } else {
                "HOME"
            };
            let home_original = std::env::var_os(home_key);
            // SAFETY: 持 PATH_LOCK 串行；restore 于 closure 末尾。
            unsafe { std::env::set_var("PATH", dir.path()) };
            unsafe { std::env::set_var(home_key, cache_dir.path()) };
            let result = VueAdapter.launch_info(&common::dummy_ctx()).await;
            *err_holder_c.lock().unwrap() = Some(result);
            // SAFETY: 同上，还原注入。
            unsafe { std::env::set_var("PATH", path_original) };
            match home_original {
                Some(v) => unsafe { std::env::set_var(home_key, v) },
                None => unsafe { std::env::remove_var(home_key) },
            }
        }
    })
    .await;

    let err = err_holder
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .expect_err("PATH 与缓存皆无时必须报错");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("not found in PATH"),
        "错误必须含 supervisor 归类锚 `not found in PATH`, msg={msg}"
    );
    assert!(
        msg.contains("install vue"),
        "install hint 必须指向 `serena-cli install vue`, msg={msg}"
    );
}

/// 真实 LS e2e：hybrid 编排 + overview(documentSymbol) + hover 语义两关。
/// 前置：`serena-cli install vue`（npm 三包）+ node 在 PATH。CI 用
/// SERENA_SKIP_LS_E2E 显式跳过（第三方 LS 版本漂移；真机/nightly 覆盖）。
#[tokio::test]
async fn e2e_overview_and_hover_on_vue_fixture() {
    if std::env::var_os("SERENA_SKIP_LS_E2E").is_some() {
        println!("skipped: SERENA_SKIP_LS_E2E set");
        return;
    }
    common::with_path_lock(|| async {
        let install = ls_runtime::install::default_cache_root()
            .join("vue")
            .join("3.1.5");
        if !install
            .join("node_modules/@vue/language-server/bin/vue-language-server.js")
            .is_file()
        {
            println!("skipped: vue LS cache missing (run `serena-cli install vue`)");
            return;
        }

        let dir = tempfile::tempdir().expect("fixture tempdir");
        let root = dir.path();
        std::fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"strict":true,"module":"esnext","moduleResolution":"bundler","target":"es2022","lib":["es2022"]},"include":["*.vue"]}"#,
        )
        .expect("tsconfig");
        // Volar hybrid 初始化要在 <root>/node_modules/ 下写全局类型文件，并检查
        // "vue installed as a direct dependency"（诊断 code 404，缺它时 TS 语义层
        // 静默不产出）。手工放 package.json 假包满足存在性检查——script 内类型
        // 语义由伴生 tsserver（tsdk typescript）计算，不依赖真 vue 运行时。
        let vue_pkg = root.join("node_modules/vue");
        std::fs::create_dir_all(&vue_pkg).expect("node_modules/vue");
        std::fs::write(vue_pkg.join("package.json"), r#"{"name":"vue","version":"3.5.13"}"#)
            .expect("stub vue package.json");
        // hover 探针位：L6（0-based）`greet` 标识符（character 22）。
        std::fs::write(
            root.join("App.vue"),
            r#"<script setup lang="ts">
interface Greeting { name: string }
function greet(g: Greeting): string {
  return `hello ${g.name}`;
}
const who: Greeting = { name: "vue" };
const msg: string = greet(who);
</script>
<template>
  <span>{{ msg }}</span>
</template>
"#,
        )
        .expect("App.vue");

        let adapter = VueAdapter;
        adapter.set_project_root(root);
        let ctx = ls_adapters::ProjectCtx {
            project_root: root.to_path_buf(),
        };

        let info = adapter
            .launch_info(&ctx)
            .await
            .expect("cache e2e: launch_info");
        let handle = ls_runtime::process::Child::spawn(info).expect("spawn vue LS");
        let mut params = InitializeParams::default();
        adapter.initialize_patches(&mut params);
        {
            use lsp_core::docsync::path_to_uri_str;
            let uri = lsp_types::Uri::from_str(&path_to_uri_str(root)).expect("root uri");
            #[allow(deprecated)]
            {
                params.root_uri = Some(uri);
            }
        }
        let session = lsp_core::session::Session::start(Some(handle), params)
            .await
            .expect("vue LS handshake");
        tokio::time::timeout(Duration::from_secs(60), adapter.on_session_ready(&session))
            .await
            .expect("hybrid onboarding within 60s")
            .expect("on_session_ready ok (companion up)");

        let file = root.join("App.vue");
        let uri = lsp_core::docsync::path_to_uri(&file).expect("file uri");
        // Session 默认 languageId "cpp" —— .vue 必须以 "vue" didOpen（supervisor
        // session_for 的 set_language_id 等价步骤）。
        session.set_language_id("vue");
        session.ensure_open(&file).await.expect("didOpen App.vue");

        // 关 1：overview —— documentSymbol 非空（轮询等项目加载）。
        let mut symbols = None;
        let mut last = String::new();
        for _ in 0..60 {
            let r = session
                .request::<serde_json::Value>(
                    "textDocument/documentSymbol",
                    serde_json::json!({ "textDocument": { "uri": uri.as_str() } }),
                    Duration::from_secs(10),
                )
                .await;
            match r {
                Ok(v) if v.as_array().is_some_and(|a| !a.is_empty()) => {
                    symbols = Some(v);
                    break;
                }
                Ok(v) => last = format!("empty/null response: {v}"),
                Err(e) => last = format!("error: {e}"),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let symbols = symbols.unwrap_or_else(|| {
            panic!("documentSymbol 非空（overview 关）, last={last}")
        });
        assert!(
            serde_json::to_string(&symbols).unwrap().contains("greet"),
            "符号表应含 script 内的 greet，got {symbols}"
        );

        // 关 2：hover 语义 —— script 内类型语义由伴生 TS LS 承载（tsserver +
        // @vue/typescript-plugin；主 Vue LS 只管结构/模板），走 semantic_session 路由
        //（supervisor tool_hover 同款）。tsserver 就绪窗口内轮询。
        let companion = adapter
            .semantic_session(root)
            .expect("hybrid onboarding 后伴生语义会话必须可用");
        companion.ensure_open(&file).await.expect("didOpen App.vue (companion)");
        let companion_uri = lsp_core::docsync::path_to_uri(&file).expect("file uri");
        let mut hover = None;
        for _ in 0..90 {
            let r = companion
                .request::<serde_json::Value>(
                    "textDocument/hover",
                    serde_json::json!({
                        "textDocument": { "uri": companion_uri.as_str() },
                        "position": { "line": 6, "character": 22 }
                    }),
                    Duration::from_secs(10),
                )
                .await;
            if let Ok(v) = r
                && !v.is_null()
                && v.get("contents").is_some_and(|c| !c.is_null())
            {
                hover = Some(v);
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let hover = hover.expect("hover 非空（语义关，tsserver 桥生效）");
        let hover_str = serde_json::to_string(&hover).unwrap();
        assert!(
            hover_str.contains("greet"),
            "hover 应指向 greet 签名（tsserver 语义），got {hover}"
        );

        session.shutdown().await;
    })
    .await;
}
