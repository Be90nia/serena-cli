//! servers.toml A 类批量收录覆盖测试（Task 20 续）。
//!
//! 数据源 local/ls-download-matrix.md：25 条清单实收 24 条 download（eclipse_jdtls
//! 由手写 jdtls T2 适配器接管，不入表）。本测试锁三件事：
//! 1. 24 个新条目解析 Ok + 必填字段齐全 + 形态约束（sha256 64hex / url-sha 配对 /
//!    platform key 合法 / exec 含 {bin} / URL 含版本）。
//! 2. 既有条目（marksman/crystalline/G 类批量）不被批量收录破坏。
//! 3. 语言路由唯一性（spec_for 按 languages 线性扫描，重复语言名 = 路由随 HashMap
//!    迭代序漂移的静默不确定性，此处前置拦截）。

use std::collections::HashSet;

use ls_registry::spec;

const SERVERS_TOML: &str = include_str!("../servers.toml");

/// A 类批量收录的 24 个新 download 条目（矩阵 §1-§25 顺序，eclipse_jdtls 跳过）。
const NEW_DOWNLOAD_IDS: &[&str] = &[
    "ada",
    "al",
    "bsl",
    "csharp",
    "clojure",
    "cue",
    "dart",
    "elixir",
    "haxe",
    "hlsl",
    "kotlin",
    "lua",
    "luau",
    "matlab",
    "nextflow",
    "omnisharp",
    "pascal",
    "phpactor",
    "phpantom",
    "powershell",
    "systemverilog",
    "toml",
    "terraform",
    "latex",
];

/// config::platform_key 输出全集（Os × Arch）。表内出现集合外键 = 运行时永远查不到的死键。
const VALID_PLATFORM_KEYS: &[&str] = &[
    "windows-x86_64",
    "windows-aarch64",
    "linux-x86_64",
    "linux-aarch64",
    "macos-x86_64",
    "macos-aarch64",
];

/// spec::validate 认可的 archive 词表（DownloadSpec.archive 注释同源）。
const VALID_ARCHIVES: &[&str] = &["zip", "tar.gz", "tar.xz", "gz", "raw"];

fn parsed_servers() -> std::collections::HashMap<String, spec::ServerSpec> {
    spec::parse(SERVERS_TOML)
        .expect("built-in servers.toml must parse")
        .servers
}

#[test]
fn new_download_entries_parse_with_complete_fields() {
    let servers = parsed_servers();
    assert_eq!(
        NEW_DOWNLOAD_IDS.len(),
        24,
        "矩阵 25 条清单减 eclipse_jdtls 应为 24"
    );
    for id in NEW_DOWNLOAD_IDS {
        let spec = servers
            .get(*id)
            .unwrap_or_else(|| panic!("[servers.{id}] missing from servers.toml"));
        assert_eq!(spec.install, "download", "{id}: install kind");
        assert!(
            spec.download.is_some(),
            "{id}: install=download requires download table"
        );
        let dl = spec.download.as_ref().unwrap();
        assert!(
            !dl.url_per_platform.is_empty(),
            "{id}: at least 1 platform url"
        );
        assert!(
            !dl.sha256_per_platform.is_empty(),
            "{id}: at least 1 platform sha256"
        );
        assert_eq!(
            dl.url_per_platform.len(),
            dl.sha256_per_platform.len(),
            "{id}: url/sha platform sets must pair 1:1"
        );
        for (plat, url) in &dl.url_per_platform {
            assert!(
                VALID_PLATFORM_KEYS.contains(&plat.as_str()),
                "{id}/{plat}: platform key outside platform_key() vocabulary"
            );
            assert!(url.starts_with("https://"), "{id}/{plat}: HTTPS only");
            assert!(
                url.contains(&dl.version),
                "{id}/{plat}: url must carry the pinned version `{}`",
                dl.version
            );
            let sha = dl
                .sha256_per_platform
                .get(plat)
                .unwrap_or_else(|| panic!("{id}/{plat}: sha256 missing"));
            assert_eq!(sha.len(), 64, "{id}/{plat}: sha256 must be 64 hex");
            assert!(
                sha.chars().all(|c| c.is_ascii_hexdigit()),
                "{id}/{plat}: sha256 must be hex"
            );
        }
        for plat in dl.sha256_per_platform.keys() {
            assert!(
                dl.url_per_platform.contains_key(plat),
                "{id}/{plat}: sha256 without matching url"
            );
        }
        assert!(
            VALID_ARCHIVES.contains(&dl.archive.as_str()),
            "{id}: archive `{}` outside vocabulary",
            dl.archive
        );
        assert!(!dl.bin_path.is_empty(), "{id}: bin_path empty");
        assert!(
            !dl.allowed_hosts.is_empty(),
            "{id}: allowed_hosts must pin download hosts"
        );
        assert!(!spec.exec.is_empty(), "{id}: exec must be explicit");
        assert!(
            spec.exec.iter().any(|arg| arg.contains("{bin}")),
            "{id}: exec must reference {{bin}} placeholder"
        );
        assert_eq!(
            spec.source_commit.as_deref(),
            Some("43ae0211"),
            "{id}: upstream anchor"
        );
    }
}

#[test]
fn legacy_entries_survive_batch_addition() {
    let servers = parsed_servers();
    for id in [
        "marksman",
        "crystalline",
        "ccls",
        "deno",
        "erlang_ls",
        "gleam",
        "haskell_ls",
        "jedi",
        "lean4",
        "ocamllsp",
        "qmlls",
        "regal",
        "sourcekit_lsp",
        "zls",
    ] {
        assert!(servers.contains_key(id), "legacy entry `{id}` was lost");
    }
    assert_eq!(servers.len(), 38, "14 legacy + 24 new download entries");
    let marksman = &servers["marksman"];
    assert_eq!(marksman.install, "download");
    assert_eq!(
        marksman.download.as_ref().unwrap().version,
        "2026-02-08",
        "marksman pinned version drifted"
    );
    assert_eq!(servers["crystalline"].install, "path_only");
}

#[test]
fn eclipse_jdtls_is_not_duplicated_in_table() {
    let servers = parsed_servers();
    assert!(
        !servers.contains_key("eclipse_jdtls"),
        "java 由手写 jdtls T2 适配器接管，VSIX+Gradle 双下载形态不入本表"
    );
}

#[test]
fn language_routes_are_unique_across_table() {
    let servers = parsed_servers();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut duplicated: Vec<String> = Vec::new();
    for (id, spec) in &servers {
        for lang in &spec.languages {
            if !seen.insert(lang.as_str()) {
                duplicated.push(format!("{lang} (in {id})"));
            }
        }
    }
    assert!(
        duplicated.is_empty(),
        "duplicate language route(s) make spec_for() HashMap-order dependent: {duplicated:?}"
    );
}
