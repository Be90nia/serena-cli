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
    LanguageId, LanguageServerAdapter, clangd::ClangdAdapter, csharp_ls::CsharpLsAdapter,
    gopls::GoplsAdapter, jdtls::JdtlsAdapter, pyright::PyrightAdapter,
    rust_analyzer::RustAnalyzerAdapter, typescript::TypescriptLanguageServerAdapter,
};

/// 扩展名 → LanguageId 静态表（小写键）。
///
/// M3 覆盖 7 个 LanguageId（M0 仅 cpp 系）。
/// ts/js 同走 TypeScript LS —— 解析时归到 TypeScript；adapter_for 按 lang 维度分。
const EXT_TABLE: &[(&str, LanguageId)] = &[
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
}
