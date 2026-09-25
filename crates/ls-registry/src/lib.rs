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

use ls_adapters::{
    LanguageId, LanguageServerAdapter, bash::BashAdapter, clangd::ClangdAdapter,
    csharp_ls::CsharpLsAdapter, gopls::GoplsAdapter, jdtls::JdtlsAdapter, json::JsonAdapter,
    powershell::PowerShellAdapter, pyright::PyrightAdapter, rust_analyzer::RustAnalyzerAdapter,
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
        // T0 配置驱动语言：无手写 adapter（见 config::ensure_launch）。
        LanguageId::Markdown => return None,
    })
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

    /// Wave 1/2：--lang bash/json/powershell/vue 必须路由到手写 T2 adapter（supervisor
    /// session_for 的 T2 优先分支）。
    #[test]
    fn adapter_for_routes_wave1_languages() {
        for lang in ["bash", "json", "powershell", "vue"] {
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
        // alias：from_str_opt 接受 pwsh（LanguageId 层），adapter_for 同语义。
        assert!(adapter_for("pwsh").is_some());
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
        assert_eq!(resolve(&PathBuf::from("App.vue")), Some(LanguageId::Vue));
    }
}
