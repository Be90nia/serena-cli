//! ls-registry 测试（PLAN Task 9 / ARCHITECTURE §4.2）。
//!
//! M3 覆盖 7 个 LanguageId（M0 仅 cpp）：
//! 1. resolve(.cpp 等六种 C++ 扩展名) → Cpp。
//! 2. resolve(.rs/.py/.go/.ts/.cs/.java 等) → 对应 LanguageId。
//! 3. 大小写不敏感：resolve("x.CPP") → Cpp。
//! 4. adapter_for("cpp"/"rust"/"python"/"go"/"typescript"/"csharp"/"java") → 对应 adapter id。
//! 5. adapter_for("lua"(T0，W3 收编)/"") → None。
//! 6. 无扩展名 / 空路径 / 目录 / 真未知后缀都不爆，返回 None。

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
fn resolve_all_m3_languages() {
    assert_eq!(
        ls_registry::resolve(Path::new("foo.rs")),
        Some(LanguageId::Rust)
    );
    assert_eq!(
        ls_registry::resolve(Path::new("foo.py")),
        Some(LanguageId::Python)
    );
    assert_eq!(
        ls_registry::resolve(Path::new("foo.pyi")),
        Some(LanguageId::Python)
    );
    assert_eq!(
        ls_registry::resolve(Path::new("foo.go")),
        Some(LanguageId::Go)
    );
    assert_eq!(
        ls_registry::resolve(Path::new("foo.ts")),
        Some(LanguageId::TypeScript)
    );
    assert_eq!(
        ls_registry::resolve(Path::new("foo.tsx")),
        Some(LanguageId::TypeScript)
    );
    assert_eq!(
        ls_registry::resolve(Path::new("foo.js")),
        Some(LanguageId::TypeScript)
    );
    assert_eq!(
        ls_registry::resolve(Path::new("foo.jsx")),
        Some(LanguageId::TypeScript)
    );
    assert_eq!(
        ls_registry::resolve(Path::new("foo.cs")),
        Some(LanguageId::CSharp)
    );
    assert_eq!(
        ls_registry::resolve(Path::new("foo.java")),
        Some(LanguageId::Java)
    );
}

#[test]
fn resolve_unknown_extension_is_none() {
    // 真正未知后缀（不在 EXT_TABLE 里）。（曾用 foo.lua 当未知样本，W3 批收编
    // lua 后换 foo.rkt——rb/zig 无 LanguageId 值仍走 None，语义不变。W4 批收编
    // zig/zon 后 zig 样本再迁 foo.nim——迁移轨迹即语言收编史。）
    assert_eq!(ls_registry::resolve(Path::new("foo.rkt")), None);
    assert_eq!(ls_registry::resolve(Path::new("foo.rb")), None);
    assert_eq!(ls_registry::resolve(Path::new("foo.nim")), None);
}

#[test]
fn resolve_is_case_insensitive() {
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
    assert_eq!(
        ls_registry::resolve(Path::new("MAIN.RS")),
        Some(LanguageId::Rust)
    );
}
#[test]
fn resolve_path_without_extension_is_none() {
    assert_eq!(ls_registry::resolve(Path::new("Makefile")), None);
    assert_eq!(ls_registry::resolve(Path::new("README")), None);
}

#[test]
fn adapter_for_each_m3_language() {
    let cases: &[(&str, &str)] = &[
        ("cpp", "clangd"),
        ("rust", "rust-analyzer"),
        ("python", "pyright"),
        ("go", "gopls"),
        ("typescript", "typescript-language-server"),
        ("javascript", "typescript-language-server"),
        ("csharp", "csharp-ls"),
        ("java", "jdtls"),
        ("astro", "astro-ls"),
    ];
    for (lang, expected_id) in cases {
        // bd serena-rust-9z0x：external-servers.toml 注册拥有该语言时 T2 让位
        // （返 None → ensure_launch 走注册条目）——本机注册表有该语言条目属预期。
        if ls_registry::config::spec_source(lang) == Some("external") {
            continue;
        }
        let ad =
            ls_registry::adapter_for(lang).unwrap_or_else(|| panic!("missing adapter for {lang}"));
        assert_eq!(ad.id(), *expected_id, "adapter id mismatch for {lang}");
    }
}

#[test]
fn adapter_for_unknown_language_is_none() {
    // 真正未知语言 + T0 配置驱动语言（lua 于 W3 批收编为 T0，走
    // config::ensure_launch，本函数语义 = 无手写 adapter）。
    assert!(ls_registry::adapter_for("lua").is_none());
    assert!(ls_registry::adapter_for("ruby").is_none());
    assert!(ls_registry::adapter_for("").is_none());
}

#[test]
fn adapter_for_is_idempotent() {
    for lang in [
        "cpp",
        "rust",
        "python",
        "go",
        "typescript",
        "csharp",
        "java",
    ] {
        // bd serena-rust-9z0x：external 注册拥有的语言 T2 让位（见
        // adapter_for_each_m3_language 注）；仅对仍走 T2 的语言断言单例。
        if ls_registry::config::spec_source(lang) == Some("external") {
            continue;
        }
        let a1 = ls_registry::adapter_for(lang).unwrap();
        let a2 = ls_registry::adapter_for(lang).unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&a1, &a2),
            "singleton broken for {lang}"
        );
    }
}
