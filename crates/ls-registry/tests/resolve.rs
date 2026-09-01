//! ls-registry 最小版测试（PLAN Task 9 / ARCHITECTURE §4.2）。
//!
//! 覆盖：
//! 1. `resolve("x.cpp") == Some(Cpp)` 等六种 C++ 扩展名。
//! 2. `resolve("x.py") == None` —— M2 前不认识 python。
//! 3. 大小写不敏感：`resolve("x.CPP") == Some(Cpp)`。
//! 4. `adapter_for("cpp")` 返回 `Arc<dyn LanguageServerAdapter>` 且 id == "clangd"。
//! 5. `adapter_for("python") == None`，`adapter_for("rust") == None`。
//! 6. 无扩展名 / 空路径 / 目录都不爆，返回 `None`。

use std::path::Path;

use ls_adapters::LanguageId;

#[test]
fn resolve_cpp_extensions() {
    for ext in ["c", "cpp", "cc", "cxx", "h", "hpp"] {
        let s = format!("some/dir/file.{ext}");
        let p = Path::new(s.as_str());
        assert_eq!(
            ls_registry::resolve(p),
            Some(LanguageId::Cpp),
            ".{ext} 必须解析为 Cpp"
        );
    }
}

#[test]
fn resolve_unknown_extension_is_none() {
    // M2 之前只认 cpp 系；python 走 servers.toml 的 pylsp 才上。
    assert_eq!(ls_registry::resolve(Path::new("foo.py")), None);
    assert_eq!(ls_registry::resolve(Path::new("foo.rs")), None);
    assert_eq!(ls_registry::resolve(Path::new("foo.go")), None);
}

#[test]
fn resolve_is_case_insensitive() {
    // 大小写不敏感 —— Windows 文件系统天然行为；扩展名表全小写，匹配时 to_lowercase。
    assert_eq!(
        ls_registry::resolve(Path::new("MAIN.CPP")),
        Some(LanguageId::Cpp)
    );
    assert_eq!(
        ls_registry::resolve(Path::new("Foo.Hpp")),
        Some(LanguageId::Cpp)
    );
    assert_eq!(
        ls_registry::resolve(Path::new("mix.Cc")),
        Some(LanguageId::Cpp)
    );
}

#[test]
fn resolve_path_without_extension_is_none() {
    // 无扩展名 → None（不是错误）。
    assert_eq!(ls_registry::resolve(Path::new("Makefile")), None);
    assert_eq!(ls_registry::resolve(Path::new("README")), None);
}

#[test]
fn adapter_for_cpp_returns_clangd() {
    let ad = ls_registry::adapter_for("cpp").expect("cpp 必须能取到 adapter");
    assert_eq!(ad.id(), "clangd");
}

#[test]
fn adapter_for_unknown_language_is_none() {
    // M0 只硬编码 "cpp"；M2 servers.toml 接管后此函数换为表查找。
    assert!(ls_registry::adapter_for("python").is_none());
    assert!(ls_registry::adapter_for("rust").is_none());
    assert!(ls_registry::adapter_for("").is_none());
}

#[test]
fn adapter_for_is_idempotent() {
    // 多次调用返回 Arc 同一份实例 —— Single 实例化（ponyxtail: OnceLock 内单例）。
    let a1 = ls_registry::adapter_for("cpp").unwrap();
    let a2 = ls_registry::adapter_for("cpp").unwrap();
    assert!(std::sync::Arc::ptr_eq(&a1, &a2));
}
