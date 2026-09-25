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
    assert_eq!(
        servers.len(),
        62,
        "存量 38（14 legacy + 24 A 类）+ Phase 3 新收 24"
    );
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

/// Phase 3 B/C/D/E/F 类 24 条（npm 13 + uvx 5 + dotnet 2 + gem 2 + source 2）。
/// 数据锚 = oraios/serena@43ae0211 适配器源码 DEFAULT_* 原值（spot-check 代表性 pin，
/// 防数据漂移）；angular 不入表（tri-server 编排，jdtls 先例）。
#[test]
fn phase3_pkg_entries_parse_with_upstream_pins() {
    let servers = parsed_servers();
    let mut seen: HashSet<&'static str> = HashSet::new();

    // npm 类：必填四件 + 版本 pin spot-check + secondary 展开正确。
    const NPM_IDS: &[&str] = &[
        "bash",
        "ansible",
        "elm",
        "intelephense",
        "json",
        "solidity",
        "scss",
        "svelte",
        "html",
        "typescript_ls",
        "typescript_vts",
        "vue",
        "yaml",
    ];
    for id in NPM_IDS {
        seen.insert("npm");
        let spec = servers
            .get(*id)
            .unwrap_or_else(|| panic!("[servers.{id}] missing"));
        assert_eq!(spec.install, "npm", "{id}");
        let npm = spec
            .npm
            .as_ref()
            .unwrap_or_else(|| panic!("{id}: npm table"));
        assert!(!npm.package.is_empty(), "{id}");
        assert!(!npm.bin_rel.is_empty(), "{id}");
        for sec in &npm.secondary_packages {
            assert!(!sec.package.is_empty(), "{id}: secondary package");
        }
    }
    // 上游 pin spot-check（DEFAULT_* 原值）。
    let bash = servers["bash"].npm.as_ref().unwrap();
    assert_eq!(bash.package, "bash-language-server");
    assert_eq!(bash.version.as_deref(), Some("5.6.0"));
    assert_eq!(bash.npm_args, Some(vec!["start".to_string()]));
    let svelte = servers["svelte"].npm.as_ref().unwrap();
    assert_eq!(
        svelte.bin_rel, "svelteserver",
        "svelte 可执行名 = svelteserver"
    );
    assert_eq!(
        svelte.secondary_packages.len(),
        3,
        "typescript+tsls+svelte-plugin"
    );
    assert_eq!(
        svelte.secondary_packages[0].version.as_deref(),
        Some("6.0.3")
    );
    let ts = servers["typescript_ls"].npm.as_ref().unwrap();
    assert_eq!(ts.version.as_deref(), Some("5.1.3"));
    assert_eq!(ts.secondary_packages[0].package, "typescript");
    assert_eq!(ts.secondary_packages[0].version.as_deref(), Some("5.9.3"));
    let vts = servers["typescript_vts"].npm.as_ref().unwrap();
    assert_eq!(vts.package, "@vtsls/language-server");
    assert_eq!(vts.bin_rel, "vtsls");
    let sol = servers["solidity"].npm.as_ref().unwrap();
    assert_eq!(sol.package, "@nomicfoundation/solidity-language-server");
    assert_eq!(sol.secondary_packages[0].package, "@foundry-rs/forge");

    // uvx 类：package+entrypoint 必填 + pin spot-check。
    const UVX_IDS: &[&str] = &[
        "pyright",
        "basedpyright",
        "python_ty",
        "python_pyrefly",
        "fortls",
    ];
    for id in UVX_IDS {
        seen.insert("uvx");
        let spec = servers
            .get(*id)
            .unwrap_or_else(|| panic!("[servers.{id}] missing"));
        assert_eq!(spec.install, "uvx", "{id}");
        let u = spec
            .uvx
            .as_ref()
            .unwrap_or_else(|| panic!("{id}: uvx table"));
        assert!(!u.package.is_empty() && !u.entrypoint.is_empty(), "{id}");
    }
    assert_eq!(
        servers["pyright"].uvx.as_ref().unwrap().version.as_deref(),
        Some("1.1.403")
    );
    assert_eq!(
        servers["pyright"].uvx.as_ref().unwrap().entrypoint,
        "pyright-langserver"
    );
    assert_eq!(
        servers["basedpyright"]
            .uvx
            .as_ref()
            .unwrap()
            .version
            .as_deref(),
        Some("1.39.9")
    );
    let ty = servers["python_ty"].uvx.as_ref().unwrap();
    assert_eq!(ty.package, "ty");
    assert_eq!(ty.args, Some(vec!["server".to_string()]));
    let pyrefly = servers["python_pyrefly"].uvx.as_ref().unwrap();
    assert_eq!(pyrefly.version.as_deref(), Some("1.1.1"));
    assert_eq!(pyrefly.args, Some(vec!["lsp".to_string()]));
    assert_eq!(
        servers["fortls"].uvx.as_ref().unwrap().version.as_deref(),
        Some("3.2.2")
    );

    // dotnet 类。
    for id in ["fsharp", "csharp_ls"] {
        seen.insert("dotnet");
        let spec = servers
            .get(id)
            .unwrap_or_else(|| panic!("[servers.{id}] missing"));
        assert_eq!(spec.install, "dotnet", "{id}");
        assert!(spec.dotnet.as_ref().unwrap().tool.len() > 1, "{id}");
    }
    let fs = servers["fsharp"].dotnet.as_ref().unwrap();
    assert_eq!(fs.tool, "fsautocomplete");
    assert_eq!(
        fs.version.as_deref(),
        Some("0.83.0"),
        "上游 DEFAULT_FSAUTOCOMPLETE_VERSION"
    );

    // gem 类。
    for id in ["ruby_lsp", "solargraph"] {
        seen.insert("gem");
        let spec = servers
            .get(id)
            .unwrap_or_else(|| panic!("[servers.{id}] missing"));
        assert_eq!(spec.install, "gem", "{id}");
        let g = spec
            .gem
            .as_ref()
            .unwrap_or_else(|| panic!("{id}: gem table"));
        assert!(!g.gem.is_empty() && !g.bin_rel.is_empty(), "{id}");
    }
    assert_eq!(
        servers["ruby_lsp"].gem.as_ref().unwrap().version.as_deref(),
        Some("0.26.8")
    );
    assert_eq!(
        servers["solargraph"]
            .gem
            .as_ref()
            .unwrap()
            .version
            .as_deref(),
        Some("0.51.1")
    );
    assert_eq!(
        servers["solargraph"].gem.as_ref().unwrap().args,
        Some(vec!["stdio".to_string()])
    );

    // source 类：repo HTTPS + build_cmd 非空。
    for id in ["nixd", "crystalline_source"] {
        seen.insert("source");
        let spec = servers
            .get(id)
            .unwrap_or_else(|| panic!("[servers.{id}] missing"));
        assert_eq!(spec.install, "source", "{id}");
        let s = spec
            .source
            .as_ref()
            .unwrap_or_else(|| panic!("{id}: source table"));
        assert!(s.repo.starts_with("https://"), "{id}");
        assert!(!s.build_cmd.is_empty(), "{id}");
        assert!(!s.bin_rel.is_empty(), "{id}");
    }
    let nixd = servers["nixd"].source.as_ref().unwrap();
    assert_eq!(nixd.build_cmd, vec!["nix", "build"]);
    assert_eq!(nixd.bin_rel, "result/bin/nixd");

    // 五类齐活。
    assert_eq!(seen.len(), 5, "B/C/D/E/F 五类都应有条目");
}

/// angular 的 tri-server 编排形态不入表（上游 angular_language_server.py：ngserver +
/// 伴随 typescript-language-server + vscode-html companion 三进程）。
#[test]
fn angular_tri_server_form_is_not_in_table() {
    let servers = parsed_servers();
    assert!(
        !servers.contains_key("angular"),
        "angular 双/三进程编排不适配单进程 Launch（eclipse_jdtls 先例），不入本表"
    );
}
