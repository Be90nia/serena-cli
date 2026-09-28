//! ls-registry —— 配置驱动层（PLAN Task 9 / ARCHITECTURE §4.2；Task 19 servers.toml）。
//!
//! M0 硬编码 C++ → clangd 一项；M2 `servers.toml` 接管后，本模块的 `resolve` /
// `adapter_for` 改为读 `ServerSpec` 表（spec.rs 子模块），不再内联枚举。
//!
//! ↖ mirror: oraios/serena@43ae021 `ls_config.py::LanguageServerId.get_source_fn_matcher`
//!
//! ## 设计要点
//!
//! - `LanguageId` 定义在 `ls-adapters`（trait 签名依赖）；本 crate re-export 以稳定上游消费面。
//! - `resolve`：扩展名 → LanguageId。扩展名存小写、匹配时 `to_lowercase`（大小写不敏感）。
//! - `adapter_for`：硬编码 "cpp" → `ClangdAdapter`，单例（`std::sync::LazyLock<Arc<…>>`，
//!   无新依赖；规则：rs-lazylock 偏好 LazyLock over OnceLock/once_cell）。

use std::path::Path;
use std::sync::{Arc, LazyLock};

pub mod config;
pub mod file_detect;
pub mod spec;

pub use ls_adapters::LanguageId;
use ls_adapters::{
    LanguageServerAdapter, astro::AstroAdapter, bash::BashAdapter,
    clangd::ClangdAdapter, csharp_ls::CsharpLsAdapter, css::CssAdapter, gopls::GoplsAdapter,
    html::HtmlAdapter, jdtls::JdtlsAdapter, json::JsonAdapter, powershell::PowerShellAdapter,
    pyright::PyrightAdapter, rust_analyzer::RustAnalyzerAdapter,
    typescript::TypescriptLanguageServerAdapter, vue::VueAdapter,
};

/// 扩展名 → LanguageId 静态表（小写键）。
///
/// M3 覆盖 7 个 LanguageId（M0 仅 cpp 系）。
/// ts/js 同走 TypeScript LS —— 解析时归到 TypeScript；adapter_for 按 lang 维度分。
/// `pub(crate)`：config.rs 加载 external-servers.toml 时检测扩展名撞表并 warn。
pub(crate) const EXT_TABLE: &[(&str, LanguageId)] = &[
    // C / C++
    ("c", LanguageId::Cpp),
    ("cpp", LanguageId::Cpp),
    ("cc", LanguageId::Cpp),
    ("cxx", LanguageId::Cpp),
    ("h", LanguageId::Cpp),
    ("hpp", LanguageId::Cpp),
    // Markdown（T0 配置驱动：servers.toml marksman，Task 21 双路径）
    ("md", LanguageId::Markdown),
    ("markdown", LanguageId::Markdown),
    // YAML（T0 配置驱动：servers.toml yaml 条目，bd 56a 后续批次）
    ("yaml", LanguageId::Yaml),
    ("yml", LanguageId::Yaml),
    // SQL / Dockerfile（T0 配置驱动：servers.toml sql/docker，bd 56a 第一批）。
    // 本批 sql 独占 .sql（pgsql/mysql 第二批用 --lang 显式覆盖，不进本表）。
    ("sql", LanguageId::Sql),
    ("dockerfile", LanguageId::Docker),
    // Rust
    ("rs", LanguageId::Rust),
    // Python
    ("py", LanguageId::Python),
    ("pyi", LanguageId::Python),
    // Go
    ("go", LanguageId::Go),
    // TypeScript / JavaScript（同 adapter 服务）
    ("ts", LanguageId::TypeScript),
    ("tsx", LanguageId::TypeScript),
    ("js", LanguageId::TypeScript),
    ("jsx", LanguageId::TypeScript),
    ("mjs", LanguageId::TypeScript),
    ("cjs", LanguageId::TypeScript),
    // C#
    ("cs", LanguageId::CSharp),
    // Java
    ("java", LanguageId::Java),
    // Bash / shell（bash-language-server 兼容 POSIX sh 语法）
    ("sh", LanguageId::Bash),
    ("bash", LanguageId::Bash),
    // JSON（jsonc = 带注释 JSON，同一 LS 接管）
    ("json", LanguageId::Json),
    ("jsonc", LanguageId::Json),
    // PowerShell
    ("ps1", LanguageId::PowerShell),
    ("psm1", LanguageId::PowerShell),
    ("psd1", LanguageId::PowerShell),
    // Vue 单文件组件
    ("vue", LanguageId::Vue),
    // Kotlin / Dart（bd 56a 后续批，T0 配置驱动：servers.toml kotlin/dart 条目）
    ("kt", LanguageId::Kotlin),
    ("kts", LanguageId::Kotlin),
    ("dart", LanguageId::Dart),
    // Astro 单文件组件（ts/js 仍归 TypeScript —— 上游 superset 优先级 1 的
    // 项目级覆盖我们无对应机制，--lang astro 显式指定即达同效）
    ("astro", LanguageId::Astro),
    // HTML / CSS（bd 56a 后续批）
    ("html", LanguageId::Html),
    ("htm", LanguageId::Html),
    ("css", LanguageId::Css),
];

/// 各 LanguageId 对应的 adapter 单例。
macro_rules! singleton {
    ($name:ident, $ty:ty) => {
        static $name: LazyLock<Arc<dyn LanguageServerAdapter>> =
            LazyLock::new(|| Arc::new(<$ty>::default()));
    };
}
singleton!(CLANGD, ClangdAdapter);
singleton!(RUST_ANALYZER, RustAnalyzerAdapter);
singleton!(PYRIGHT, PyrightAdapter);
singleton!(GOPLS, GoplsAdapter);
singleton!(TYPESCRIPT, TypescriptLanguageServerAdapter);
singleton!(CSHARP_LS, CsharpLsAdapter);
singleton!(JDTLS, JdtlsAdapter);
singleton!(BASH, BashAdapter);
singleton!(JSON, JsonAdapter);
singleton!(POWERSHELL, PowerShellAdapter);
singleton!(VUE, VueAdapter);
singleton!(ASTRO, AstroAdapter);
singleton!(HTML, HtmlAdapter);
singleton!(CSS, CssAdapter);

/// 路径 → 语言。扩展名小写后查表，命中即返回；其余 None。
///
/// 无扩展名 / 无文件 / 目录 / 不认识的后缀统一返回 `None`（**不**抛错 —— 解析失败是常规分支）。
pub fn resolve(path: &Path) -> Option<LanguageId> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    EXT_TABLE
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, lang)| *lang)
}

/// 路径 → 语言名（external-ls-registration-design §2 extension 路由）。
///
/// 先查内置 `EXT_TABLE`（手写语言优先，external 声明同扩展名时内置胜）；未命中查
/// external-servers.toml 声明的 extensions → 该条目 `languages[0]`（session_for /
/// spec_for 按语言名走配置驱动启动）。两者皆未命中 → None。
pub fn resolve_lang_name(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    EXT_TABLE
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, lang)| lang.as_str())
        .or_else(|| config::external_table().and_then(|t| config::match_external_ext(t, &ext)))
}

/// 语言字符串 → adapter 单例。M3 覆盖 7 手写语言。
///
/// T0 配置驱动语言（servers.toml，如 markdown）**不走此处**——它们的启动经
/// `config::ensure_launch`（Task 19），supervisor 接线归 Task 21。
/// 返回 `Arc` 让调用方按 trait 对象持有；`Arc::ptr_eq` 在两次调用间成立（LazyLock 单例）。
/// 未知语言 / 空串返回 `None`。
pub fn adapter_for(lang: &str) -> Option<Arc<dyn LanguageServerAdapter>> {
    let id = LanguageId::from_str_opt(lang)?;
    Some(match id {
        LanguageId::Cpp => CLANGD.clone(),
        LanguageId::Rust => RUST_ANALYZER.clone(),
        LanguageId::Python => PYRIGHT.clone(),
        LanguageId::Go => GOPLS.clone(),
        LanguageId::TypeScript => TYPESCRIPT.clone(),
        LanguageId::CSharp => CSHARP_LS.clone(),
        LanguageId::Java => JDTLS.clone(),
        LanguageId::Bash => BASH.clone(),
        LanguageId::Json => JSON.clone(),
        LanguageId::PowerShell => POWERSHELL.clone(),
        LanguageId::Vue => VUE.clone(),
        LanguageId::Astro => ASTRO.clone(),
        // bd 56a 后续批：html/css 手写 T2（同包 vscode-langservers-extracted 双入口）。
        LanguageId::Html => HTML.clone(),
        LanguageId::Css => CSS.clone(),
        // T0 配置驱动语言（markdown/yaml）：无手写 adapter（见 config::ensure_launch）。
        LanguageId::Markdown | LanguageId::Yaml => return None,
        // T0 配置驱动（bd 56a 第一批）：docker/sql 走 servers.toml docker/sql 条目。
        LanguageId::Docker | LanguageId::Sql => return None,
        // T0 配置驱动（bd 56a 第二批）：pgsql/mysql 走 servers.toml pgls/sqls-mysql 条目。
        LanguageId::Pgsql | LanguageId::Mysql => return None,
        // T0 配置驱动（bd 56a 后续批）：kotlin/dart 走 servers.toml kotlin/dart 条目
        // （download 形态，ensure_launch 接管）。
        LanguageId::Kotlin | LanguageId::Dart => return None,
    })
}

/// 内部语言名 → LSP didOpen 的 languageId。多数语言与内部名恒等；LSP 官方 languageId
/// 与内部 id 不一致时在此登记（`Session::set_language_id` 前的唯一换算点，session_for
/// 统一调用）。bd 56a：内部 "docker" 的 LSP 官方 languageId 是 "dockerfile"——发错值
/// dockerfile-ls 侧语义未定义（三关可能静默降级），故显式映射而非赌 LS 宽容。
pub fn lsp_language_id(lang: &str) -> String {
    match lang {
        "docker" => "dockerfile".to_string(),
        // bd 56a 第二批：LSP 官方 SQL languageId 是 "sql"（VS Code 口径）；pgls/sqls
        // 实测对任意值宽容，但同 docker 原则显式映射而非赌 LS 行为。
        "pgsql" | "mysql" => "sql".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    //! 单元测试覆盖"表内 vs 表外"语义；集成行为（Arc 单例、adapter trait）放 `tests/resolve.rs`。
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn table_includes_cpp_variants() {
        // 表的自描述性测试：改表必同步改此断言。
        let cpp_exts: Vec<&str> = EXT_TABLE
            .iter()
            .filter(|(_, lang)| *lang == LanguageId::Cpp)
            .map(|(e, _)| *e)
            .collect();
        assert_eq!(cpp_exts, vec!["c", "cpp", "cc", "cxx", "h", "hpp"]);
    }

    #[test]
    fn resolve_handles_no_extension() {
        assert_eq!(resolve(&PathBuf::from("Makefile")), None);
    }

    #[test]
    fn resolve_handles_uppercase_extension() {
        assert_eq!(resolve(&PathBuf::from("Foo.CPP")), Some(LanguageId::Cpp));
    }

    #[test]
    fn resolve_all_m3_languages() {
        // M3 表覆盖 7 语言；改表必同步改此断言。
        assert_eq!(resolve(&PathBuf::from("a.rs")), Some(LanguageId::Rust));
        assert_eq!(resolve(&PathBuf::from("a.py")), Some(LanguageId::Python));
        assert_eq!(resolve(&PathBuf::from("a.pyi")), Some(LanguageId::Python));
        assert_eq!(resolve(&PathBuf::from("a.go")), Some(LanguageId::Go));
        assert_eq!(
            resolve(&PathBuf::from("a.ts")),
            Some(LanguageId::TypeScript)
        );
        assert_eq!(
            resolve(&PathBuf::from("a.tsx")),
            Some(LanguageId::TypeScript)
        );
        assert_eq!(
            resolve(&PathBuf::from("a.js")),
            Some(LanguageId::TypeScript)
        );
        assert_eq!(
            resolve(&PathBuf::from("a.jsx")),
            Some(LanguageId::TypeScript)
        );
        assert_eq!(resolve(&PathBuf::from("a.cs")), Some(LanguageId::CSharp));
        assert_eq!(resolve(&PathBuf::from("a.java")), Some(LanguageId::Java));
        assert_eq!(resolve(&PathBuf::from("a.lua")), None);
    }

    #[test]
    fn adapter_for_all_m3_languages() {
        // 每个 lang 字符串都应返 Some 单例；Arc::ptr_eq 在两次调用间成立。
        for lang in [
            "cpp",
            "rust",
            "python",
            "go",
            "typescript",
            "javascript",
            "csharp",
            "java",
        ] {
            let a = adapter_for(lang).unwrap_or_else(|| panic!("missing adapter for {lang}"));
            let b = adapter_for(lang).unwrap();
            assert!(Arc::ptr_eq(&a, &b), "singleton broken for {lang}");
            let _ = a.id();
        }
        assert!(adapter_for("lua").is_none());
    }

    /// Wave 1/2：--lang bash/json/powershell/vue/astro 必须路由到手写 T2 adapter（supervisor
    /// session_for 的 T2 优先分支）。
    #[test]
    fn adapter_for_routes_wave1_languages() {
        for lang in ["bash", "json", "powershell", "vue", "astro"] {
            let a =
                adapter_for(lang).unwrap_or_else(|| panic!("--lang {lang} 必须路由到 T2 adapter"));
            let b = adapter_for(lang).unwrap();
            assert!(Arc::ptr_eq(&a, &b), "singleton broken for {lang}");
        }
        // T2 侧 languages 声明与 lang 字符串闭环（路由一致性）。
        assert_eq!(
            adapter_for("bash").unwrap().languages(),
            &[LanguageId::Bash]
        );
        assert_eq!(
            adapter_for("json").unwrap().languages(),
            &[LanguageId::Json]
        );
        assert_eq!(
            adapter_for("powershell").unwrap().languages(),
            &[LanguageId::PowerShell]
        );
        assert_eq!(adapter_for("vue").unwrap().languages(), &[LanguageId::Vue]);
        assert_eq!(
            adapter_for("astro").unwrap().languages(),
            &[LanguageId::Astro]
        );
        // alias：from_str_opt 接受 pwsh（LanguageId 层），adapter_for 同语义。
        assert!(adapter_for("pwsh").is_some());
    }

    /// bd 56a 后续批：--lang html/css 必须路由到手写 T2 adapter（同包双入口），
    /// 扩展名 html/htm/css 走 EXT_TABLE，lsp_language_id 恒等。
    #[test]
    fn adapter_for_routes_html_css() {
        for lang in ["html", "css"] {
            let a =
                adapter_for(lang).unwrap_or_else(|| panic!("--lang {lang} 必须路由到 T2 adapter"));
            let b = adapter_for(lang).unwrap();
            assert!(Arc::ptr_eq(&a, &b), "singleton broken for {lang}");
        }
        assert_eq!(adapter_for("html").unwrap().languages(), &[LanguageId::Html]);
        assert_eq!(adapter_for("css").unwrap().languages(), &[LanguageId::Css]);
        // lsp_language_id 恒等（LSP 官方口径同内部名）。
        assert_eq!(lsp_language_id("html"), "html");
        assert_eq!(lsp_language_id("css"), "css");
        // 扩展名解析（EXT_TABLE）：html/htm 归 Html 门，css 归 Css 门。
        assert_eq!(resolve(&PathBuf::from("index.html")), Some(LanguageId::Html));
        assert_eq!(resolve(&PathBuf::from("page.htm")), Some(LanguageId::Html));
        assert_eq!(resolve(&PathBuf::from("style.css")), Some(LanguageId::Css));
    }

    /// Wave 1 扩展名解析：sh/bash/json/jsonc/ps1/psm1/psd1/vue（resolve 锁定）。
    #[test]
    fn resolve_wave1_extensions() {
        assert_eq!(resolve(&PathBuf::from("a.sh")), Some(LanguageId::Bash));
        assert_eq!(resolve(&PathBuf::from("a.bash")), Some(LanguageId::Bash));
        assert_eq!(resolve(&PathBuf::from("pkg.json")), Some(LanguageId::Json));
        assert_eq!(
            resolve(&PathBuf::from("tsconfig.jsonc")),
            Some(LanguageId::Json)
        );
        assert_eq!(
            resolve(&PathBuf::from("x.ps1")),
            Some(LanguageId::PowerShell)
        );
        assert_eq!(
            resolve(&PathBuf::from("x.psm1")),
            Some(LanguageId::PowerShell)
        );
        assert_eq!(
            resolve(&PathBuf::from("x.psd1")),
            Some(LanguageId::PowerShell)
        );
        assert_eq!(resolve(&PathBuf::from("x.astro")), Some(LanguageId::Astro));
        assert_eq!(resolve(&PathBuf::from("App.vue")), Some(LanguageId::Vue));
    }

    /// bd 56a 第一批：docker/sql 走 T0（无手写 adapter），servers.toml 语言路由 +
    /// LSP didOpen languageId 换算（docker → 官方 "dockerfile"）。
    #[test]
    fn batch56a_docker_sql_t0_routing_and_lsp_language_id() {
        // T0：无手写 adapter（session_for 落 config::ensure_launch）。
        assert!(adapter_for("docker").is_none());
        assert!(adapter_for("sql").is_none());
        // servers.toml 语言路由命中（spec_for 按 languages 扫描）。
        assert!(config::spec_for("docker").is_some());
        assert!(config::spec_for("sql").is_some());
        // LSP 官方 languageId 换算：docker→dockerfile；sql 及既有语言恒等。
        assert_eq!(lsp_language_id("docker"), "dockerfile");
        assert_eq!(lsp_language_id("sql"), "sql");
        assert_eq!(lsp_language_id("rust"), "rust");
        // 扩展名解析（EXT_TABLE）。
        assert_eq!(resolve(&PathBuf::from("schema.sql")), Some(LanguageId::Sql));
        assert_eq!(
            resolve(&PathBuf::from("app.dockerfile")),
            Some(LanguageId::Docker)
        );
    }
}
