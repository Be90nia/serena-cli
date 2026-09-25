//! external-servers.toml 注册机制集成测试（external-ls-registration-design §3/§4）。
//!
//! 用户 `EXTERNAL` static 走真实用户目录（CI / 无该文件 → None），故本文件只测
//! pub API 的确定性路径；文件加载 / 注入型扩展名匹配的单测在 `src/config.rs`。

use std::path::Path;

use ls_registry::config::{merge_pick, merged_spec_for, spec_for, spec_source};
use ls_registry::spec;

/// 最简合法条目（path_only 形态）。
fn entry(id: &str, priority: i32) -> String {
    format!(
        "[servers.{id}]\nlanguages = [\"{id}\"]\ninstall = \"path_only\"\npriority = {priority}\n\
         [servers.{id}.path_only]\nbinary_name = \"{id}-ls\"\ninstall_hint = \"x\"\n"
    )
}

fn table(toml_str: &str) -> spec::ServersToml {
    spec::parse(toml_str).expect("test toml must be valid")
}

/// §2 schema：与内置 ServerSpec 100% 兼容；priority 缺省 = 0（serde default）。
#[test]
fn external_schema_parses_with_priority_default_zero() {
    let t = table(&entry("mydsl", 0));
    assert_eq!(t.servers["mydsl"].priority, 0, "显式 0 合法");

    let t = table(&entry("mydsl", 10));
    assert_eq!(t.servers["mydsl"].priority, 10);

    // priority 字段整个省略（与内置表写法一致）→ 缺省 0。
    let t = table(
        "[servers.plain]\nlanguages = [\"plain\"]\ninstall = \"path_only\"\n\
         [servers.plain.path_only]\nbinary_name = \"plain-ls\"\ninstall_hint = \"x\"\n",
    );
    assert_eq!(t.servers["plain"].priority, 0, "字段缺省 = 0");
}

/// §3 合并优先级：external 高者胜 / 低者让位 / 并列（含双方缺省 0）external 胜出
/// （PM 拍板：保留显式覆盖能力，非拒绝）。
#[test]
fn merge_pick_priority_ordering() {
    let b_toml = table(&entry("solidity", 0));
    let e_high = table(&entry("solidity_fork", 10));
    let e_low = table(&entry("solidity_fork", -1));
    let e_tie = table(&entry("solidity_fork", 0));
    let b = ("solidity", &b_toml.servers["solidity"]);
    let e_high = ("solidity_fork", &e_high.servers["solidity_fork"]);
    let e_low = ("solidity_fork", &e_low.servers["solidity_fork"]);
    let e_tie = ("solidity_fork", &e_tie.servers["solidity_fork"]);

    // external priority 更高 → external 替换 builtin（完整条目替换）。
    let (id, _, external) = merge_pick(Some(b), Some(e_high)).unwrap();
    assert_eq!((id, external), ("solidity_fork", true));

    // external priority 更低（负值显式让位）→ builtin 保留。
    let (id, _, external) = merge_pick(Some(b), Some(e_low)).unwrap();
    assert_eq!((id, external), ("solidity", false));

    // 并列 0 == 0 → external 胜出。
    let (id, _, external) = merge_pick(Some(b), Some(e_tie)).unwrap();
    assert_eq!((id, external), ("solidity_fork", true));

    // 单边命中直通；双边未命中 None。
    assert!(matches!(merge_pick(Some(b), None), Some((_, _, false))));
    assert!(matches!(merge_pick(None, Some(e_high)), Some((_, _, true))));
    assert!(merge_pick(None, None).is_none());
}

/// 无 external-servers.toml 环境（CI）下 merged 查找与内置单表行为一致。
#[test]
fn merged_lookup_matches_builtin_without_external_table() {
    assert_eq!(spec_source("markdown"), Some("builtin"));
    let (id, spec) = spec_for("markdown").unwrap();
    assert_eq!(id, "marksman", "languages 维度命中（id ≠ 查询 key）");
    assert_eq!(spec.install, "download");
    assert!(merged_spec_for("crystal").is_some());
    // 手写 T2 语言不进表；来源查询同步为 None。
    assert!(spec_for("rust").is_none());
    assert_eq!(spec_source("rust"), None);
    assert_eq!(
        spec_for("markdown").map(|(id, _)| id),
        merged_spec_for("markdown").map(|(id, _)| id)
    );
}

/// §2 extension 路由：内置 EXT_TABLE 优先，未知扩展在无 external 表时回落 None；
/// 大小写不敏感；无扩展名 / 非常规输入不 panic。
#[test]
fn resolve_lang_name_builtin_first_and_graceful_none() {
    assert_eq!(
        ls_registry::resolve_lang_name(Path::new("MAIN.RS")),
        Some("rust"),
        "内置扩展名大小写不敏感"
    );
    assert_eq!(
        ls_registry::resolve_lang_name(Path::new("a.Md")),
        Some("markdown")
    );
    // CI 无用户 external-servers.toml：external 兜底层空转 → None。
    assert_eq!(ls_registry::resolve_lang_name(Path::new("foo.mydsl")), None);
    assert_eq!(ls_registry::resolve_lang_name(Path::new("Makefile")), None);
    assert_eq!(ls_registry::resolve_lang_name(Path::new("")), None);
}
