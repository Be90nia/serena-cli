//! ls-registry —— 配置驱动层（PLAN Task 9 / ARCHITECTURE §4.2 最小版）。
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

use ls_adapters::{LanguageId, LanguageServerAdapter, clangd::ClangdAdapter};

/// 扩展名 → LanguageId 静态表（小写键）。
///
/// M0 仅 cpp 系；M2 扩展为 servers.toml 驱动的查表结构。
const EXT_TABLE: &[(&str, LanguageId)] = &[
    ("c", LanguageId::Cpp),
    ("cpp", LanguageId::Cpp),
    ("cc", LanguageId::Cpp),
    ("cxx", LanguageId::Cpp),
    ("h", LanguageId::Cpp),
    ("hpp", LanguageId::Cpp),
];

/// ClangdAdapter 单例 —— LazyLock 内置初始化，标准库替代 once_cell / OnceLock（rs-lazylock）。
static CLANGD_SINGLETON: LazyLock<Arc<dyn LanguageServerAdapter>> =
    LazyLock::new(|| Arc::new(ClangdAdapter));

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

/// 语言字符串 → adapter 单例。M0 只认 `"cpp"` → `ClangdAdapter`。
///
/// 返回 `Arc` 让调用方按 trait 对象持有；`Arc::ptr_eq` 在两次调用间成立（LazyLock 单例）。
/// 未知语言 / 空串返回 `None`。
pub fn adapter_for(lang: &str) -> Option<Arc<dyn LanguageServerAdapter>> {
    match lang {
        "cpp" => Some(CLANGD_SINGLETON.clone()),
        _ => None,
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
}
