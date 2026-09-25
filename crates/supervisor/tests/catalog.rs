//! Catalog 集成测试（P1 k32）。
//!
//! 判定：
//! 1. `supervisor::catalog::catalog()` 包含 44 个工具（与 supervisor lib.rs execute_tool
//!    match 分支一一对应）。新增/删除工具必须在两边同步；CI 失败即提示人工补 catalog。
//! 2. catalog 中的私有前缀 `_` 字段（`_timeout_ms` / `_compact` / `_delta` / `_max_tokens`
//!    / `_compress` / `_index_timeout_ms`）是 sanitize 流水线已知名单；改动 sanitize 时
//!    必须同步 catalog 注释。
//!
//! ponytail：test 只 parse lib.rs 文件 + 反序列化 catalog JSON；不动业务路径。

use std::collections::BTreeSet;
use std::fs;

use supervisor::catalog;

/// 工具名白名单：执行 entry 工具名以外的分支必须从差集里剔除。
/// - "other"：execute_tool 的 default 兜底分支，不是真实工具。
/// - 私有 `_*` 字段前缀是 args 内部约定（sanitize 不清），不是工具名。
const NON_TOOL_BRANCHES: &[&str] = &["other"];

/// 列出 `lib.rs` 内 execute_tool match 分支的所有字符串字面量（顶层 arm）。
/// ponytail：单文件正则提取即可，不需要 AST；改动 lib.rs match 结构时这个测试会立刻
/// 报错提醒同步 catalog。
///
/// 状态机：找包含 `match tool {` 的行进入扫描；用相对 depth 追踪 arm 嵌套的 `{` `}`，
/// 回到 0 时退出。
///
/// 收集策略：每行先看是不是顶层 arm 起始（depth==1 且行 trim 后形如 `"x" =>` 或
/// `"x" => expr,`）—— depth==1 表示在外层 `match tool {` 直接上下文里，要么是
/// 新 arm 开头，要么是非 block arm 起始。Block arm 的 `{` 计入后 depth→2，下一行
/// 才会做 arm 体处理；非 block arm 始终在 depth==1。
///
/// 内嵌 `match op { ... }`（call/type-hierarchy 内部）的子 arms 在 depth>=3，
/// 不会被误收。
fn execute_tool_branches_from_source(src: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut scanning = false;
    let mut depth: i32 = 0; // 进入 match tool { 时置 1，block-arm `{` 加到 2。
    for line in src.lines() {
        let trimmed = line.trim_start();
        if !scanning {
            if trimmed.contains("match tool {") {
                scanning = true;
                depth = 1;
            }
            continue;
        }
        // arm 起始收集：depth==1 时本行是新 arm（block-arm 头或非 block-arm）。
        // 收集在 depth 变化**之前**做，避免 arm `{` 已被本行计入。
        if depth == 1
            && let Some(rest) = trimmed.strip_prefix('"')
            && let Some(end) = rest.find('"')
        {
            let name = &rest[..end];
            if !name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                names.insert(name.to_string());
            }
        }
        // 推进 depth；遇到 0 → match 结束。
        let mut broke = false;
        for c in trimmed.chars() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        scanning = false;
                        broke = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        if !scanning {
            break;
        }
        let _ = broke;
    }
    names
}

#[test]
fn catalog_matches_execute_tool_branches() {
    // 路径：CARGO_MANIFEST_DIR = crates/supervisor；lib.rs 在 src/。
    let lib_rs = fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/lib.rs"))
        .expect("read lib.rs");
    let lib_src = String::from_utf8(lib_rs).expect("lib.rs utf8");

    let mut branches = execute_tool_branches_from_source(&lib_src);
    for skip in NON_TOOL_BRANCHES {
        branches.remove(*skip);
    }

    let catalog_names: BTreeSet<String> = catalog::catalog_tool_names().into_iter().collect();

    assert_eq!(
        branches,
        catalog_names,
        "execute_tool match branches ({}) != catalog tools ({})\nmissing_in_catalog: {:?}\nextra_in_catalog: {:?}",
        branches.len(),
        catalog_names.len(),
        branches.difference(&catalog_names).collect::<Vec<_>>(),
        catalog_names.difference(&branches).collect::<Vec<_>>(),
    );
}

#[test]
fn catalog_has_no_duplicate_tool_names() {
    let names = catalog::catalog_tool_names();
    let unique: BTreeSet<&String> = names.iter().collect();
    assert_eq!(
        unique.len(),
        names.len(),
        "duplicate tool names in catalog: {names:?}"
    );
}

#[test]
fn catalog_each_tool_has_args_object() {
    let v = catalog::catalog();
    let tools = v
        .get("tools")
        .and_then(|t| t.as_object())
        .expect("tools object");
    for (name, def) in tools {
        assert!(
            def.get("args").and_then(|a| a.as_object()).is_some(),
            "tool `{name}` missing args object"
        );
    }
}

#[test]
fn catalog_size_is_stable() {
    // 上限提示：当前 44 个 tools（execute_tool 顶层 arm 计数）。后续添加工具时此
    // 数字必须同步，否则下游文档/diff 基准会过期。阈值放宽到 40 ~ 200，避免「新增
    // 1 个工具忘了改这个测试」阻断；但 <40 仍报错以防有人误删大段 catalog。
    let names = catalog::catalog_tool_names();
    assert!(
        (40..=200).contains(&names.len()),
        "catalog tool count out of expected range [40, 200]: {} (current = {:?})",
        names.len(),
        names
    );
}

#[test]
fn catalog_private_prefixes_are_consistent_with_sanitize() {
    // 私有字段必须只在 args 出现，绝不出现在 tool name。
    let v = catalog::catalog();
    let tools = v.get("tools").and_then(|t| t.as_object()).unwrap();
    let known_private = [
        "_timeout_ms",
        "_index_timeout_ms",
        "_compact",
        "_delta",
        "_max_tokens",
        "_compress",
    ];
    let mut seen_private: BTreeSet<String> = BTreeSet::new();
    for (tool_name, def) in tools {
        let args = def.get("args").and_then(|a| a.as_object()).unwrap();
        for arg_name in args.keys() {
            if arg_name.starts_with('_') {
                seen_private.insert(arg_name.clone());
                assert!(
                    known_private.contains(&arg_name.as_str()),
                    "tool `{tool_name}` declares private arg `{arg_name}` not in known sanitize list {known_private:?}; \
                     if intentional, add to known_private + sanitize_timeout_args in lib.rs"
                );
            }
        }
    }
    // 不强制要求每个私有前缀都有 catalog 条目（不一定所有工具都接受所有私有前缀），
    // 但出现过的必须是已知清单。
    for seen in &seen_private {
        assert!(
            known_private.contains(&seen.as_str()),
            "private arg `{seen}` seen in catalog but not in known list"
        );
    }
}
