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

/// A 类批量收录的 23 个新 download 条目（矩阵 §1-§25 顺序，eclipse_jdtls 跳过；
/// al 于 langs20 R2 转 path_only——marketplace vspackage 变 gzip 包 zip，download
/// schema 无两级解包，2026-09-30）。
const NEW_DOWNLOAD_IDS: &[&str] = &[
    "ada",
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
        23,
        "矩阵 25 条清单减 eclipse_jdtls 减 al(path_only) 应为 23"
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
        for (plat, archive) in &dl.archive_per_platform {
            assert!(
                VALID_PLATFORM_KEYS.contains(&plat.as_str()),
                "{id}/{plat}: archive_per_platform key outside platform_key() vocabulary"
            );
            assert!(
                VALID_ARCHIVES.contains(&archive.as_str()),
                "{id}/{plat}: archive_per_platform `{archive}` outside vocabulary"
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
        // 升版锚例外：kotlin 263.4702.0（上游 DEFAULT_KOTLIN_LSP_VERSION + sha 锚）取自
        // 7a296833 kotlin_language_server.py 与同 commit 的 downloaded_dependency_hashes.json；
        // cue 同批升锚（7a296833 才有 cue_language_server.py，W6 批对拍修正）；其余条目
        // 数据源锚 43ae0211。
        let anchor = match *id {
            "kotlin" | "cue" => "7a296833",
            _ => "43ae0211",
        };
        assert_eq!(
            spec.source_commit.as_deref(),
            Some(anchor),
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
        73,
        "存量 38（14 legacy + 24 A 类）+ Phase 3 新收 24 + astro（7a296833）+ bd 56a 第一批 +2 docker/sql + 第二批 +2 pgls/sqls-mysql + 后续批 +1 css + W3 +1 scala + W5 +2 julia/wolfram（html/yaml/marksman/kotlin/dart/ansible 等为存量条目，多批零新增）——集成收口 PM 实测 73"
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
        Some("1.1.414")
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
    // W6 批升版：上游 PYREFLY_VERSION = 1.2.0（pyrefly_server.py@7a296833）。
    assert_eq!(pyrefly.version.as_deref(), Some("1.2.0"));
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

/// astro 条目（上游 7a296833 新增）：四包 pin 锚 astro_language_server.py DependencyProvider
/// 原值，防数据漂移（vue/svelte 同款 spot-check 形态）。
#[test]
fn astro_entry_pins_match_upstream_7a296833() {
    let servers = parsed_servers();
    let spec = servers
        .get("astro")
        .unwrap_or_else(|| panic!("[servers.astro] missing"));
    assert_eq!(spec.install, "npm");
    assert_eq!(spec.languages, vec!["astro"]);
    let npm = spec.npm.as_ref().expect("astro: npm table");
    assert_eq!(npm.package, "@astrojs/language-server");
    assert_eq!(npm.version.as_deref(), Some("2.17.0"));
    assert_eq!(npm.bin_rel, "astro-ls");
    let sec: Vec<(&str, Option<&str>)> = npm
        .secondary_packages
        .iter()
        .map(|s| (s.package.as_str(), s.version.as_deref()))
        .collect();
    assert_eq!(
        sec,
        vec![
            ("@astrojs/ts-plugin", Some("1.10.10")),
            ("typescript", Some("5.9.3")),
            ("typescript-language-server", Some("5.1.3")),
        ],
        "astro 伴生三包 pin 漂移"
    );
}

/// bd 56a 第一批（Δ 自有设计，上游无对应物，source_commit 省略 = Δ 标注）：
/// docker npm 条目 + sql download 条目 pin 锚，防数据漂移（astro 同款 spot-check 形态）。
#[test]
fn batch56a_docker_sql_entries_pins_match_research() {
    let servers = parsed_servers();

    // docker：npm 一手数据（registry.npmjs.org/…/latest，2026-09-28）；bin 名实为
    // docker-langserver（包 bin 字段），非包名。
    let docker = servers
        .get("docker")
        .unwrap_or_else(|| panic!("[servers.docker] missing"));
    assert_eq!(docker.install, "npm");
    assert_eq!(docker.languages, vec!["docker"]);
    assert_eq!(docker.extensions, vec![".dockerfile"]);
    let npm = docker.npm.as_ref().expect("docker: npm table");
    assert_eq!(npm.package, "dockerfile-language-server-nodejs");
    assert_eq!(npm.version.as_deref(), Some("0.15.0"));
    assert_eq!(npm.bin_rel, "docker-langserver");
    assert_eq!(
        npm.npm_args.as_deref(),
        Some(["--stdio".to_string()].as_slice())
    );

    // sql：download；sha256 双锚 = GitHub API assets[].digest + 本机下载实测
    // （v0.2.48 windows 资产，2026-09-28）。
    let sql = servers
        .get("sql")
        .unwrap_or_else(|| panic!("[servers.sql] missing"));
    assert_eq!(sql.install, "download");
    assert_eq!(sql.languages, vec!["sql"]);
    assert_eq!(sql.extensions, vec![".sql"]);
    let dl = sql.download.as_ref().expect("sql: download table");
    assert_eq!(dl.version, "0.2.48");
    assert_eq!(dl.archive, "zip");
    assert_eq!(dl.bin_path, "sqls.exe");
    assert_eq!(
        dl.url_per_platform
            .get("windows-x86_64")
            .map(String::as_str),
        Some(
            "https://github.com/sqls-server/sqls/releases/download/v0.2.48/sqls-windows-0.2.48.zip"
        )
    );
    assert_eq!(
        dl.sha256_per_platform
            .get("windows-x86_64")
            .map(String::as_str),
        Some("df6453b2ddcb4e748547d0288b826251a24af099749dc7a9ddea587aac3d4365")
    );
    assert!(sql.source_commit.is_none(), "Δ 自有设计条目无上游锚");
}

/// bd 56a 后续批：html（上游 43ae0211 既有数据条目，本批 T2 接线）+ css（Δ 自有设计，
/// 同包 vscode-langservers-extracted 双入口）pin 锚，防数据漂移（astro 同款 spot-check
/// 形态）。包/版本 = npm registry 一手 + 上游 vscode_html_language_server.py
/// DEFAULT_PACKAGE（4.10.0，2026-09-28 查证；bin 名≠包名，registry bin 字段）。
#[test]
fn html_css_entries_pins_match_research() {
    let servers = parsed_servers();

    let html = servers
        .get("html")
        .unwrap_or_else(|| panic!("[servers.html] missing"));
    assert_eq!(html.install, "npm");
    assert_eq!(html.languages, vec!["html"]);
    assert_eq!(html.source_commit.as_deref(), Some("43ae0211"));
    let html_npm = html.npm.as_ref().expect("html: npm table");
    assert_eq!(html_npm.package, "vscode-langservers-extracted");
    assert_eq!(html_npm.version.as_deref(), Some("4.10.0"));
    assert_eq!(html_npm.bin_rel, "vscode-html-language-server");
    assert_eq!(
        html_npm.npm_args.as_deref(),
        Some(["--stdio".to_string()].as_slice())
    );

    // css：同包 css 双入口（Δ 上游无对应注册，source_commit 省略 = Δ 标注）；
    // 缓存按 id 分落（html/css 各一份，sqls-mysql 先例）。
    let css = servers
        .get("css")
        .unwrap_or_else(|| panic!("[servers.css] missing"));
    assert_eq!(css.install, "npm");
    assert_eq!(css.languages, vec!["css"]);
    let css_npm = css.npm.as_ref().expect("css: npm table");
    assert_eq!(css_npm.package, "vscode-langservers-extracted");
    assert_eq!(css_npm.version.as_deref(), Some("4.10.0"));
    assert_eq!(css_npm.bin_rel, "vscode-css-language-server");
    assert_eq!(
        css_npm.npm_args.as_deref(),
        Some(["--stdio".to_string()].as_slice())
    );
    assert!(css.source_commit.is_none(), "Δ 自有设计条目无上游锚");
}

/// bd 56a 后续批：kotlin download 条目（JetBrains managed LSP 升版 263.4702.0）+ dart
/// download 条目（整 SDK，windows 组）pin 锚，防数据漂移（astro 同款 spot-check 形态）。
/// sha256 锚 = oraios/serena@7a296833 downloaded_dependency_hashes.json（kotlin）/
/// dart_language_server.py DEFAULT_DART_SDK_SHA256_BY_PLATFORM（dart）。
#[test]
fn kotlin_dart_entries_pins_match_upstream_7a296833() {
    let servers = parsed_servers();

    let kotlin = servers
        .get("kotlin")
        .unwrap_or_else(|| panic!("[servers.kotlin] missing"));
    assert_eq!(kotlin.install, "download");
    assert_eq!(kotlin.languages, vec!["kotlin"]);
    assert_eq!(kotlin.extensions, vec![".kt", ".kts"]);
    assert_eq!(kotlin.source_commit.as_deref(), Some("7a296833"));
    assert_eq!(
        kotlin.exec,
        vec![
            "{bin}".to_string(),
            "--stdio".to_string(),
            "--system-path".to_string(),
            "{bin_dir}/system".to_string(),
        ]
    );
    let kdl = kotlin.download.as_ref().expect("kotlin: download table");
    assert_eq!(kdl.version, "263.4702.0");
    assert_eq!(kdl.archive, "zip");
    assert_eq!(kdl.bin_path, "bin/intellij-server.exe");
    assert_eq!(
        kdl.allowed_hosts,
        vec!["download-cdn.jetbrains.com".to_string()]
    );
    assert_eq!(
        kdl.url_per_platform
            .get("windows-x86_64")
            .map(String::as_str),
        Some(
            "https://download-cdn.jetbrains.com/language-server/kotlin-server/263.4702.0/kotlin-server-263.4702.0.win.zip"
        )
    );
    assert_eq!(
        kdl.sha256_per_platform
            .get("windows-x86_64")
            .map(String::as_str),
        Some("a9b471b16025b1bfb3b0a097862580abb40e3c35406c44242c18b1d70f5d0e44")
    );

    let dart = servers
        .get("dart")
        .unwrap_or_else(|| panic!("[servers.dart] missing"));
    assert_eq!(dart.install, "download");
    assert_eq!(dart.languages, vec!["dart"]);
    assert_eq!(dart.extensions, vec![".dart"]);
    let ddl = dart.download.as_ref().expect("dart: download table");
    assert_eq!(ddl.version, "3.7.1");
    assert_eq!(ddl.archive, "zip");
    assert_eq!(ddl.bin_path, "dart-sdk/bin/dart.exe");
    assert_eq!(
        ddl.url_per_platform
            .get("windows-x86_64")
            .map(String::as_str),
        Some(
            "https://storage.googleapis.com/dart-archive/channels/stable/release/3.7.1/sdk/dartsdk-windows-x64-release.zip"
        )
    );
    assert_eq!(
        ddl.sha256_per_platform
            .get("windows-x86_64")
            .map(String::as_str),
        Some("f56c03122e17abe5be1429eee0a975fb8ed511b6731ec90c6475992d3dee4ea5")
    );
}

/// bd 56a 第二批（Δ 自有设计）：pgls download 条目 + sqls-mysql download 条目 pin 锚
/// （astro 同款 spot-check 形态）。sha256 双锚 = release 页 expanded_assets 官方标注
/// + 本机下载实测（0.25.7 exe / v0.2.48 zip，2026-09-28）。
#[test]
fn batch56a2_pgls_sqls_mysql_entries_pins_match_research() {
    let servers = parsed_servers();

    let pgls = servers
        .get("pgls")
        .unwrap_or_else(|| panic!("[servers.pgls] missing"));
    assert_eq!(pgls.install, "download");
    assert_eq!(pgls.languages, vec!["pgsql"]);
    // pgls 的 LSP 入口是 lsp-proxy 子命令（裸跑 = CLI 帮助即退）。
    assert_eq!(
        pgls.exec,
        vec!["{bin}".to_string(), "lsp-proxy".to_string()]
    );
    let dl = pgls.download.as_ref().expect("pgls: download table");
    assert_eq!(dl.version, "0.25.7");
    assert_eq!(dl.archive, "raw");
    assert_eq!(dl.bin_path, "postgres-language-server.exe");
    assert_eq!(
        dl.url_per_platform
            .get("windows-x86_64")
            .map(String::as_str),
        Some(
            "https://github.com/supabase-community/postgres-language-server/releases/download/0.25.7/postgres-language-server_x86_64-pc-windows-msvc.exe"
        )
    );
    assert_eq!(
        dl.sha256_per_platform
            .get("windows-x86_64")
            .map(String::as_str),
        Some("9b67d59032275810e76e3557ad06c700c7b6cd71a8e3fac7656b926cc5603248")
    );
    assert!(pgls.source_commit.is_none(), "Δ 自有设计条目无上游锚");

    // sqls-mysql：与 sql 门同一 sqls 二进制（同 zip 同 sha256），仅语言名不同；
    // exec 省略 = 裸启动 [{bin}]（sqls 裸跑即 stdio LS，真机实测）。
    let mysql = servers
        .get("sqls-mysql")
        .unwrap_or_else(|| panic!("[servers.sqls-mysql] missing"));
    assert_eq!(mysql.install, "download");
    assert_eq!(mysql.languages, vec!["mysql"]);
    assert!(mysql.exec.is_empty(), "sqls 裸启动即 stdio，exec 省略");
    let mdl = mysql.download.as_ref().expect("sqls-mysql: download table");
    assert_eq!(mdl.version, "0.2.48");
    assert_eq!(mdl.bin_path, "sqls.exe");
    assert_eq!(
        mdl.sha256_per_platform
            .get("windows-x86_64")
            .map(String::as_str),
        Some("df6453b2ddcb4e748547d0288b826251a24af099749dc7a9ddea587aac3d4365")
    );
    assert!(mysql.source_commit.is_none(), "Δ 自有设计条目无上游锚");
}

/// --lang 显式路由闭环（bd 56a 第二批验收单测）：`--lang pgsql|mysql` 经
/// supervisor `resolve_lang_for_file`（override 直接小写透传）→ `spec_for` 必须命中
/// 本批条目，且 T0 分流成立（adapter_for 返 None → session_for 走 config::ensure_launch）。
/// 默认归属承诺：.sql 扩展名归 sql 门，pgsql/mysql 不经扩展名抢占（--lang 才可达）。
#[test]
fn lang_override_routes_pgsql_mysql_to_batch2_entries() {
    use ls_registry::LanguageId;
    use ls_registry::adapter_for;
    use ls_registry::config::spec_for;

    for (lang, want_id) in [("pgsql", "pgls"), ("mysql", "sqls-mysql")] {
        // --lang 透传值 → spec_for 命中本批条目（install 链 / doctor hint 同源）。
        let (id, spec) =
            spec_for(lang).unwrap_or_else(|| panic!("--lang {lang} must route to a spec"));
        assert_eq!(id, want_id, "spec_for(\"{lang}\") routes to [servers.{id}]");
        assert!(spec.download.is_some(), "{lang}: T0 download 条目形态");
        // T0 分流：无手写 adapter，session_for 走 ensure_launch。
        assert!(
            adapter_for(lang).is_none(),
            "{lang}: T0 配置驱动语言不得有 T2 adapter"
        );
    }

    // LanguageId 反查（doctor / install 按语言名工作所需的入口面）。
    assert_eq!(LanguageId::from_str_opt("pgsql"), Some(LanguageId::Pgsql));
    assert_eq!(
        LanguageId::from_str_opt("postgres"),
        Some(LanguageId::Pgsql),
        "postgres 别名同归 Pgsql"
    );
    assert_eq!(LanguageId::from_str_opt("mysql"), Some(LanguageId::Mysql));
    // 优先级语义：.sql 默认归 sql 门（兄弟批 EXT_TABLE 落地），pgsql/mysql 门
    // 不经扩展名路由——from_extension 任何情况下不得返回本批语言。
    for ext in ["sql", "SQL", "ddl"] {
        assert_ne!(
            LanguageId::from_extension(ext),
            Some(LanguageId::Pgsql),
            "{ext}: pgsql 无扩展名路由"
        );
        assert_ne!(
            LanguageId::from_extension(ext),
            Some(LanguageId::Mysql),
            "{ext}: mysql 无扩展名路由"
        );
    }
}

/// bd 56a 后续批次（yaml/markdown）验收单测：`--lang yaml|markdown` 显式路由闭环 ——
/// yaml/markdown 经 spec_for 命中 yaml/marksman 条目，T0 分流成立（adapter_for 返 None
/// → session_for 走 config::ensure_launch）；LanguageId 反查 + 扩展名路由闭环
/// （.md/.markdown/.yaml/.yml）；yaml npm 条目 pin 锚（上游 DEFAULT_YAML_LANGUAGE_SERVER_VERSION）。
#[test]
fn lang_yaml_markdown_routes_and_ext_roundtrip() {
    use ls_registry::LanguageId;
    use ls_registry::adapter_for;
    use ls_registry::config::spec_for;

    for (lang, want_id) in [("yaml", "yaml"), ("markdown", "marksman")] {
        // --lang 透传值 → spec_for 命中条目（install 链 / doctor hint 同源）。
        let (id, _) =
            spec_for(lang).unwrap_or_else(|| panic!("--lang {lang} must route to a spec"));
        assert_eq!(id, want_id, "spec_for(\"{lang}\") routes to [servers.{id}]");
        // T0 分流：无手写 adapter，session_for 走 ensure_launch。
        assert!(
            adapter_for(lang).is_none(),
            "{lang}: T0 配置驱动语言不得有 T2 adapter"
        );
        // LSP didOpen languageId 恒等（marksman / yaml-language-server 官方口径同内部名）。
        assert_eq!(ls_registry::lsp_language_id(lang), lang);
    }

    // LanguageId 反查 + 扩展名闭环（doctor / warm / file_detect 入口面）。
    assert_eq!(LanguageId::from_str_opt("yaml"), Some(LanguageId::Yaml));
    assert_eq!(LanguageId::from_extension("yaml"), Some(LanguageId::Yaml));
    assert_eq!(LanguageId::from_extension("yml"), Some(LanguageId::Yaml));
    assert_eq!(LanguageId::from_extension("md"), Some(LanguageId::Markdown));
    assert_eq!(
        LanguageId::from_extension("markdown"),
        Some(LanguageId::Markdown)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("a.yaml")),
        Some(LanguageId::Yaml)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("b.yml")),
        Some(LanguageId::Yaml)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("c.md")),
        Some(LanguageId::Markdown)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("d.markdown")),
        Some(LanguageId::Markdown)
    );
}

/// yaml npm 条目 pin 锚（防数据漂移；docker/sql 同款 spot-check 形态）。
#[test]
fn yaml_entry_pins_match_upstream() {
    let servers = parsed_servers();
    let spec = servers
        .get("yaml")
        .unwrap_or_else(|| panic!("[servers.yaml] missing"));
    assert_eq!(spec.install, "npm");
    assert_eq!(spec.languages, vec!["yaml"]);
    assert_eq!(spec.source_commit.as_deref(), Some("43ae0211"));
    let npm = spec.npm.as_ref().expect("yaml: npm table");
    assert_eq!(npm.package, "yaml-language-server");
    assert_eq!(
        npm.version.as_deref(),
        Some("1.19.2"),
        "上游 DEFAULT_YAML_LANGUAGE_SERVER_VERSION 原值"
    );
    assert_eq!(npm.bin_rel, "yaml-language-server");
    assert_eq!(
        npm.npm_args.as_deref(),
        Some(["--stdio".to_string()].as_slice())
    );
}

/// W1b 批（ansible/rego/nextflow）验收单测：`--lang ansible|rego|nextflow` 显式路由
/// 闭环 —— spec_for 命中 ansible/regal/nextflow 条目，T0 分流成立（adapter_for 返
/// None → session_for 走 config::ensure_launch）；lsp_language_id 三门恒等（上游三
/// adapter 均以内部名作 LSP languageId）；扩展名闭环 rego=.rego、nextflow=.nf；
/// ansible 无扩展名路由——.yml/.yaml 归 Yaml 门（momus 裁决的冲突归属），此处锁
/// from_extension('yml') 回归（ansible 条目不得影响 yaml 路由）。
#[test]
fn w1b_ansible_rego_nextflow_routes_and_ext_roundtrip() {
    use ls_registry::LanguageId;
    use ls_registry::adapter_for;
    use ls_registry::config::spec_for;

    for (lang, want_id) in [("ansible", "ansible"), ("rego", "regal")] {
        // --lang 透传值 → spec_for 命中条目（install 链 / doctor hint 同源）。
        let (id, _) =
            spec_for(lang).unwrap_or_else(|| panic!("--lang {lang} must route to a spec"));
        assert_eq!(id, want_id, "spec_for(\"{lang}\") routes to [servers.{id}]");
        // T0 分流：无手写 adapter，session_for 走 ensure_launch。
        assert!(
            adapter_for(lang).is_none(),
            "{lang}: T0 配置驱动语言不得有 T2 adapter"
        );
        // LSP didOpen languageId 恒等（上游 adapter 的 language_id 参数 = 内部名）。
        assert_eq!(ls_registry::lsp_language_id(lang), lang);
    }

    // nextflow：条目仍在（install/doctor + did_change_config 通道），但会话已升
    // T2（bd 69e W3 采纳——references flush/符号名前缀剥离需适配器挂点）。
    let (nf_id, _) =
        spec_for("nextflow").unwrap_or_else(|| panic!("--lang nextflow must route to a spec"));
    assert_eq!(nf_id, "nextflow");
    assert!(
        adapter_for("nextflow").is_some(),
        "nextflow 已 T2 接管（bd 69e W3 采纳）"
    );
    assert_eq!(ls_registry::lsp_language_id("nextflow"), "nextflow");

    // LanguageId 反查 + 扩展名闭环（doctor / warm / file_detect 入口面）。
    assert_eq!(
        LanguageId::from_str_opt("ansible"),
        Some(LanguageId::Ansible)
    );
    assert_eq!(LanguageId::from_str_opt("rego"), Some(LanguageId::Rego));
    assert_eq!(
        LanguageId::from_str_opt("nextflow"),
        Some(LanguageId::Nextflow)
    );
    assert_eq!(LanguageId::from_extension("rego"), Some(LanguageId::Rego));
    assert_eq!(LanguageId::from_extension("nf"), Some(LanguageId::Nextflow));
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("main.rego")),
        Some(LanguageId::Rego)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("main.nf")),
        Some(LanguageId::Nextflow)
    );

    // Yaml 门回归锁：ansible 不抢 .yml/.yaml（momus 裁决），扩展名解析不受 W1b 影响。
    for ext in ["yml", "yaml"] {
        assert_ne!(
            LanguageId::from_extension(ext),
            Some(LanguageId::Ansible),
            "{ext}: ansible 无扩展名路由"
        );
    }
    assert_eq!(LanguageId::from_extension("yml"), Some(LanguageId::Yaml));
    assert_eq!(LanguageId::from_extension("yaml"), Some(LanguageId::Yaml));
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("play.yaml")),
        Some(LanguageId::Yaml)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("play.yml")),
        Some(LanguageId::Yaml)
    );
}

/// regal 条目升级锚（W1b：存量 path_only → GitHub release 单文件 download）。
/// v0.42.0 五平台 sha256 = GitHub API releases/tags/v0.42.0 assets[].digest 实锚
/// （open-policy-agent/regal，原 styrainc org 迁移后重定向）；exec 镜像上游
/// regal_server.py@7a296833 的 `regal language-server`（旧 `workspace lsp` 为误配，
/// regal 仓库 cmd/languageserver.go cobra Use 同证）。regal 非 A 类矩阵成员，
/// 不入 NEW_DOWNLOAD_IDS 清单，独立锚定（字段检查同款）。
#[test]
fn regal_entry_download_upgrade_pins() {
    let servers = parsed_servers();
    let spec = servers
        .get("regal")
        .unwrap_or_else(|| panic!("[servers.regal] missing"));
    assert_eq!(spec.install, "download");
    assert_eq!(spec.languages, vec!["rego"]);
    assert_eq!(spec.source_commit.as_deref(), Some("43ae0211"));
    assert_eq!(
        spec.exec,
        vec!["{bin}", "language-server"],
        "上游 regal_server.py@7a296833 同款启动（旧 workspace lsp 误配已修）"
    );
    let dl = spec.download.as_ref().expect("regal: download table");
    assert_eq!(dl.version, "0.42.0");
    assert_eq!(dl.archive, "raw");
    assert_eq!(dl.bin_path, "regal");
    assert_eq!(
        dl.bin_path_per_platform
            .get("windows-x86_64")
            .map(String::as_str),
        Some("regal.exe"),
        "windows 资产带 .exe 后缀，per-OS bin_path 覆盖"
    );
    assert_eq!(dl.url_per_platform.len(), 5);
    assert_eq!(
        dl.sha256_per_platform.len(),
        5,
        "五平台 1:1（windows-aarch64 上游无资产，平台缺席非遗漏）"
    );
    for (plat, url) in &dl.url_per_platform {
        assert!(
            url.starts_with(
                "https://github.com/open-policy-agent/regal/releases/download/v0.42.0/"
            ),
            "{plat}: url org/tag"
        );
        let sha = dl
            .sha256_per_platform
            .get(plat)
            .unwrap_or_else(|| panic!("{plat}: sha256 missing"));
        assert_eq!(sha.len(), 64, "{plat}: sha256 64hex");
    }
    // 真值 spot-check（转写防错：linux-x86_64 全值 + windows 资产名）。
    assert_eq!(
        dl.url_per_platform.get("linux-x86_64").map(String::as_str),
        Some(
            "https://github.com/open-policy-agent/regal/releases/download/v0.42.0/regal_Linux_x86_64"
        )
    );
    assert_eq!(
        dl.sha256_per_platform
            .get("linux-x86_64")
            .map(String::as_str),
        Some("2ebd3c93e4b325b735dbc4851a033e4d7830e88a79c47c8d087ea75c061cec67")
    );
    assert!(
        dl.url_per_platform
            .get("windows-x86_64")
            .map(String::as_str)
            .is_some_and(|u| u.ends_with("regal_Windows_x86_64.exe")),
        "windows 资产名"
    );
    assert!(
        !dl.allowed_hosts.is_empty(),
        "regal: allowed_hosts must pin download hosts"
    );
}

/// ansible / nextflow 条目锚 spot-check（W1b；两门条目为存量，本批仅接线 Rust 侧）。
/// ansible npm pin = 上游 DEFAULT_ANSIBLE_LANGUAGE_SERVER_VERSION（1.2.3，
/// ansible_language_server.py@7a296833）；nextflow pin = 上游
/// DEFAULT_NEXTFLOW_LS_VERSION（26.04.3），sha = GitHub API assets[].digest 实锚
/// （any 平台单一 fat JAR，五平台同值）。
#[test]
fn ansible_nextflow_entry_pins_match_upstream() {
    let servers = parsed_servers();
    let ansible = servers
        .get("ansible")
        .unwrap_or_else(|| panic!("[servers.ansible] missing"));
    assert_eq!(ansible.install, "npm");
    assert_eq!(ansible.languages, vec!["ansible"]);
    assert_eq!(ansible.source_commit.as_deref(), Some("43ae0211"));
    let npm = ansible.npm.as_ref().expect("ansible: npm table");
    assert_eq!(npm.package, "@ansible/ansible-language-server");
    assert_eq!(
        npm.version.as_deref(),
        Some("1.2.3"),
        "上游 DEFAULT_ANSIBLE_LANGUAGE_SERVER_VERSION 原值"
    );
    assert_eq!(npm.bin_rel, "ansible-language-server");
    assert_eq!(
        npm.npm_args.as_deref(),
        Some(["--stdio".to_string()].as_slice())
    );

    let nf = servers
        .get("nextflow")
        .unwrap_or_else(|| panic!("[servers.nextflow] missing"));
    assert_eq!(nf.install, "download");
    assert_eq!(nf.languages, vec!["nextflow"]);
    assert_eq!(
        nf.exec,
        vec!["java", "-jar", "{bin}"],
        "上游 nextflow_language_server.py 同款（java -jar fat JAR；npm 无此包，registry 404）"
    );
    let dl = nf.download.as_ref().expect("nextflow: download table");
    assert_eq!(dl.version, "26.04.3");
    assert_eq!(dl.archive, "raw");
    assert_eq!(dl.bin_path, "language-server-all.jar");
    assert_eq!(dl.sha256_per_platform.len(), 5);
    assert!(
        dl.url_per_platform.values().all(|u| u.starts_with(
            "https://github.com/nextflow-io/language-server/releases/download/v26.04.3/"
        )),
        "nextflow: url org/tag"
    );
    assert_eq!(
        dl.sha256_per_platform
            .get("windows-x86_64")
            .map(String::as_str),
        Some("20cfa34f6e202d6b8babd8d786202ce00e0d39b70ccec3290e2ab3fbd02bc016"),
        "GitHub API assets[].digest 实锚"
    );
}

/// W1a 四门（toml/terraform/cue/nixd）pin 锚，防数据漂移（astro 同款 spot-check 形态）。
/// sha256 锚 = oraios/serena@43ae0211 适配器源码内嵌 checksums 原值（taplo_server.py
/// DEFAULT_TAPLO_SHA256_CHECKSUMS / terraform_ls.py DEFAULT_TERRAFORM_LS_SHA256_BY_PLATFORM，
/// 2026-09-29 对 7a296833 复核同值）；cue/nixd 锚 = cue-lang/cue v0.16.1 release 资产 /
/// 表内既录值。terraform exec 锁 `serve` 子命令（上游 ProcessLaunchInfo 原值；裸二进制
/// 无默认子命令，缺 serve = 打印帮助即退出）。
#[test]
fn w1a_config_entries_pins_match_upstream() {
    let servers = parsed_servers();

    let toml = servers
        .get("toml")
        .unwrap_or_else(|| panic!("[servers.toml] missing"));
    assert_eq!(toml.install, "download");
    assert_eq!(toml.languages, vec!["toml"]);
    assert_eq!(toml.extensions, vec![".toml"]);
    assert_eq!(toml.source_commit.as_deref(), Some("43ae0211"));
    assert_eq!(
        toml.exec,
        vec!["{bin}".to_string(), "lsp".to_string(), "stdio".to_string()]
    );
    let tdl = toml.download.as_ref().expect("toml: download table");
    assert_eq!(tdl.version, "0.10.0");
    assert_eq!(tdl.archive, "gz", "taplo unix 资产 = single-gz 非 tar");
    assert_eq!(tdl.bin_path, "download", "SingleGz 落盘名语义");
    assert_eq!(
        tdl.url_per_platform.get("linux-x86_64").map(String::as_str),
        Some("https://github.com/tamasfe/taplo/releases/download/0.10.0/taplo-linux-x86_64.gz")
    );
    assert_eq!(
        tdl.sha256_per_platform
            .get("linux-x86_64")
            .map(String::as_str),
        Some("8fe196b894ccf9072f98d4e1013a180306e17d244830b03986ee5e8eabeb6156"),
        "taplo-linux-x86_64.gz sha = 上游内嵌 checksums 原值"
    );

    let terraform = servers
        .get("terraform")
        .unwrap_or_else(|| panic!("[servers.terraform] missing"));
    assert_eq!(terraform.install, "download");
    assert_eq!(terraform.languages, vec!["terraform"]);
    assert_eq!(terraform.extensions, vec![".tf", ".tfvars"]);
    assert_eq!(
        terraform.exec,
        vec!["{bin}".to_string(), "serve".to_string()],
        "上游 terraform_ls.py launch = `{{path}} serve`；裸二进制只打印帮助即退出"
    );
    let fdl = terraform
        .download
        .as_ref()
        .expect("terraform: download table");
    assert_eq!(fdl.version, "0.36.5");
    assert_eq!(fdl.archive, "zip");
    assert_eq!(fdl.bin_path, "terraform-ls");
    assert_eq!(
        fdl.allowed_hosts,
        vec!["releases.hashicorp.com".to_string()]
    );
    assert_eq!(
        fdl.url_per_platform.get("linux-x86_64").map(String::as_str),
        Some(
            "https://releases.hashicorp.com/terraform-ls/0.36.5/terraform-ls_0.36.5_linux_amd64.zip"
        )
    );
    assert_eq!(
        fdl.sha256_per_platform
            .get("linux-x86_64")
            .map(String::as_str),
        Some("37e645cc54fd03e863157e2a3e773e7a5ff1d6cb3d045e4c20860cac1f550a44"),
        "terraform-ls linux_amd64 sha = 上游内嵌 checksums 原值"
    );

    let cue = servers
        .get("cue")
        .unwrap_or_else(|| panic!("[servers.cue] missing"));
    assert_eq!(cue.install, "download");
    assert_eq!(cue.languages, vec!["cue"]);
    assert_eq!(cue.extensions, vec![".cue"]);
    assert_eq!(cue.exec, vec!["{bin}".to_string(), "lsp".to_string()]);
    let cdl = cue.download.as_ref().expect("cue: download table");
    assert_eq!(cdl.version, "v0.16.1");
    assert_eq!(cdl.bin_path, "cue");
    assert_eq!(
        cdl.sha256_per_platform
            .get("linux-x86_64")
            .map(String::as_str),
        Some("5d644c1305a2b86504c8dcd2ec829cf5b4999efc2cf51ee375624e0455f774ae")
    );

    // nixd：source 构建形态（release 无预编译资产，GitHub releases assets=[] 实锚
    // 2026-09-29）；安装/启动要求 Nix 工具链——CI 冒烟门 HOST skip 的依据。
    let nixd = servers
        .get("nixd")
        .unwrap_or_else(|| panic!("[servers.nixd] missing"));
    assert_eq!(nixd.install, "source");
    assert_eq!(nixd.languages, vec!["nix"]);
    assert_eq!(nixd.extensions, vec![".nix"]);
    let nsrc = nixd.source.as_ref().expect("nixd: source table");
    assert_eq!(nsrc.repo, "https://github.com/nix-community/nixd");
    assert_eq!(nsrc.build_cmd, vec!["nix".to_string(), "build".to_string()]);
    assert_eq!(nsrc.bin_rel, "result/bin/nixd");
}

/// W1a 四门路由闭环验收单测（yaml/markdown 同款）：`--lang toml|terraform|cue|nix`
/// 经 spec_for 命中本批条目，T0 分流成立（adapter_for 返 None → session_for 走
/// config::ensure_launch）；LanguageId 反查 + 扩展名路由闭环（.toml/.tf/.tfvars/.cue/.nix，
/// 大小写不敏感）；LSP didOpen languageId 恒等内部名（taplo "toml" / terraform-ls
/// "terraform" / cue "cue" / nixd "nix"，上游适配器第四参原值）。
#[test]
fn lang_w1a_routes_and_ext_roundtrip() {
    use ls_registry::LanguageId;
    use ls_registry::adapter_for;
    use ls_registry::config::spec_for;

    for (lang, want_id) in [
        ("toml", "toml"),
        ("terraform", "terraform"),
        ("cue", "cue"),
        ("nix", "nixd"),
    ] {
        let (id, _) =
            spec_for(lang).unwrap_or_else(|| panic!("--lang {lang} must route to a spec"));
        assert_eq!(id, want_id, "spec_for(\"{lang}\") routes to [servers.{id}]");
        assert!(
            adapter_for(lang).is_none(),
            "{lang}: T0 配置驱动语言不得有 T2 adapter"
        );
        assert_eq!(ls_registry::lsp_language_id(lang), lang);
    }

    assert_eq!(LanguageId::from_str_opt("toml"), Some(LanguageId::Toml));
    assert_eq!(
        LanguageId::from_str_opt("terraform"),
        Some(LanguageId::Terraform)
    );
    assert_eq!(LanguageId::from_str_opt("cue"), Some(LanguageId::Cue));
    assert_eq!(LanguageId::from_str_opt("nix"), Some(LanguageId::Nix));

    assert_eq!(LanguageId::from_extension("toml"), Some(LanguageId::Toml));
    assert_eq!(
        LanguageId::from_extension("tf"),
        Some(LanguageId::Terraform)
    );
    assert_eq!(
        LanguageId::from_extension("tfvars"),
        Some(LanguageId::Terraform)
    );
    assert_eq!(LanguageId::from_extension("cue"), Some(LanguageId::Cue));
    assert_eq!(LanguageId::from_extension("nix"), Some(LanguageId::Nix));

    // resolve（EXT_TABLE 路由，手写/内置双入口共享）+ 大小写不敏感。
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("a.toml")),
        Some(LanguageId::Toml)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("Main.TF")),
        Some(LanguageId::Terraform)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("x.tfvars")),
        Some(LanguageId::Terraform)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("y.cue")),
        Some(LanguageId::Cue)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("flake.nix")),
        Some(LanguageId::Nix)
    );
}

/// W2 批（svelte/deno/sass）验收单测：三门均为手写 T2（adapter_for 必须 Some，与
/// W1a/W1b 的 T0 断言相反）；`--lang svelte|deno|sass` 经 spec_for 命中条目（sass
/// 路由名映射到 scss 条目）；lsp_language_id 官方口径（deno→"typescript"、sass→
/// "scss"、svelte 恒等）；deno 显式路由门（TS 家族扩展名不得解析到 Deno）；deno
/// 条目 download 形态与六平台 pin 锚；svelte 四包 pin（typescript-svelte-plugin
/// 镜像锚，hybrid 语义核心）。
#[test]
fn w2_svelte_deno_sass_doors_wired() {
    use ls_registry::LanguageId;
    use ls_registry::adapter_for;
    use ls_registry::config::spec_for;

    let servers = parsed_servers();

    // --lang 透传值 → spec_for 命中条目；三门 T2 分流（session_for 走手写 adapter）。
    for (lang, want_id) in [("svelte", "svelte"), ("deno", "deno"), ("sass", "scss")] {
        let (id, _) =
            spec_for(lang).unwrap_or_else(|| panic!("--lang {lang} must route to a spec"));
        assert_eq!(id, want_id, "spec_for(\"{lang}\") routes to [servers.{id}]");
        adapter_for(lang)
            .unwrap_or_else(|| panic!("{lang}: W2 三门均为手写 T2，adapter_for 不得为 None"));
    }

    // LSP didOpen 官方口径换算（deno lsp / some-sass 的 language_id 原值）。
    assert_eq!(ls_registry::lsp_language_id("deno"), "typescript");
    assert_eq!(ls_registry::lsp_language_id("sass"), "scss");
    assert_eq!(ls_registry::lsp_language_id("svelte"), "svelte");

    // deno 显式路由门：TS 家族扩展名一律归 TypeScript 门。
    for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"] {
        assert_ne!(
            LanguageId::from_extension(ext),
            Some(LanguageId::Deno),
            "{ext}: deno 不抢 TS 家族扩展名"
        );
    }
    assert_eq!(
        LanguageId::from_extension("svelte"),
        Some(LanguageId::Svelte)
    );
    assert_eq!(LanguageId::from_extension("sass"), Some(LanguageId::Sass));
    assert_eq!(LanguageId::from_extension("scss"), Some(LanguageId::Sass));

    // deno 条目：path_only → download 升级形态（W2），exec = `deno lsp` 子命令。
    let deno = servers.get("deno").expect("[servers.deno] missing");
    assert_eq!(deno.install, "download");
    assert!(deno.path_only.is_none(), "deno path_only 旧形态应已移除");
    assert_eq!(
        deno.exec,
        vec!["{bin}".to_string(), "lsp".to_string()],
        "入口是 deno lsp 子命令，非 --stdio flag"
    );
    assert_eq!(deno.source_commit.as_deref(), Some("7a296833"));
    let dl = deno.download.as_ref().expect("deno download table");
    assert_eq!(dl.version, "2.9.7");
    assert_eq!(dl.url_per_platform.len(), 6, "六平台资产 url/sha 1:1");
    assert_eq!(dl.sha256_per_platform.len(), 6);
    assert_eq!(
        dl.resolved_bin_path("windows-x86_64"),
        "deno.exe",
        "windows 组走单值 bin_path"
    );
    assert_eq!(
        dl.resolved_bin_path("linux-x86_64"),
        "deno",
        "unix 组走 per-platform 覆盖"
    );

    // scss 条目：路由语言名 = sass（W2 改名），pin 锚上游 DEFAULT 原值。
    let scss = servers.get("scss").expect("[servers.scss] missing");
    assert_eq!(scss.languages, vec!["sass"]);
    assert_eq!(scss.source_commit.as_deref(), Some("7a296833"));
    let scss_npm = scss.npm.as_ref().expect("scss: npm table");
    assert_eq!(scss_npm.package, "some-sass-language-server");
    assert_eq!(scss_npm.version.as_deref(), Some("2.3.8"));
    assert_eq!(
        scss_npm.npm_args.as_deref(),
        Some(["--stdio".to_string()].as_slice())
    );

    // svelte 条目：typescript-svelte-plugin 镜像锚（伴生插件，hybrid 语义核心）。
    let svelte = servers["svelte"].npm.as_ref().unwrap();
    let plugin = &svelte.secondary_packages[2];
    assert_eq!(plugin.package, "typescript-svelte-plugin");
    assert_eq!(plugin.version.as_deref(), Some("0.3.52"));
}

/// smoke R6（run 36577226543）pyright 变体门锁：--lang pyright 按 entry id 显式路由
/// [servers.pyright]（uvx pin 对账见 UVX_IDS 块），didOpen 官方口径显式映射
/// pyright→python——pyright 对自名 languageId 不识别（didOpen 被吞 → 12 连发
/// documentSymbol 零应答，帧实锚），intelephense→php 先例；同族 basedpyright 宽容
/// 照过，不加死映射。
#[test]
fn pyright_variant_door_didopen_language_id() {
    use ls_registry::adapter_for;
    use ls_registry::config::spec_for;

    // T0 分流：--lang pyright → [servers.pyright] 条目；无手写 adapter
    // （语言路由 python = PyrightAdapter T2，变体门走条目 id）。
    let (id, _) = spec_for("pyright").expect("--lang pyright must route to a spec");
    assert_eq!(id, "pyright");
    assert!(adapter_for("pyright").is_none(), "pyright 变体门 T0");

    // didOpen 官方口径：仅 pyright 显式映射；python 门本体与 basedpyright 恒等。
    assert_eq!(ls_registry::lsp_language_id("pyright"), "python");
    assert_eq!(ls_registry::lsp_language_id("python"), "python");
    assert_eq!(ls_registry::lsp_language_id("basedpyright"), "basedpyright");
}

/// W3 批（php/lua/scala/swift）验收单测：四门均为 T0（adapter_for None，session_for
/// 落 ensure_launch）；php 门按 entry id `intelephense` 显式路由（语言路由 `php` 归
/// phpactor，phpantom 避撞先例），didOpen 官方口径 intelephense→php；lua（LuaLS
/// download pin 对账）/scala（metals path_only，本批唯一新增条目）/swift（存量
/// sourcekit_lsp path_only）pin 形态锁定；扩展名分流（.php/.lua/.scala/.swift
/// 各归其门，luau 独立语言不别名）。
#[test]
fn w3_php_lua_scala_swift_doors_wired() {
    use ls_registry::LanguageId;
    use ls_registry::adapter_for;
    use ls_registry::config::spec_for;

    let servers = parsed_servers();

    // T0 分流：--lang 透传值 → spec_for 命中条目；三门 adapter_for 全 None。
    // scala 除外：W3 采纳（bd 69e batchA 缺失[高]）已升 T2。
    for (lang, want_id) in [
        ("intelephense", "intelephense"),
        ("lua", "lua"),
        ("swift", "sourcekit_lsp"),
    ] {
        let (id, _) =
            spec_for(lang).unwrap_or_else(|| panic!("--lang {lang} must route to a spec"));
        assert_eq!(id, want_id, "spec_for(\"{lang}\") routes to [servers.{id}]");
        assert!(
            adapter_for(lang).is_none(),
            "{lang}: W3 三门全 T0，adapter_for 必须为 None"
        );
    }
    let (scala_id, _) =
        spec_for("scala").unwrap_or_else(|| panic!("--lang scala must route to a spec"));
    assert_eq!(scala_id, "scala");
    assert!(
        adapter_for("scala").is_some(),
        "scala 已 T2 接管（bd 69e W3 采纳）"
    );
    // phpactor 仍占语言路由 `php`（存量条目本体不动）。
    let (php_route_id, _) =
        spec_for("php").unwrap_or_else(|| panic!("--lang php must route to a spec"));
    assert_eq!(php_route_id, "phpactor", "语言路由 php 归 phpactor（存量）");

    // LSP didOpen 官方口径：仅 intelephense→php 显式映射；lua/scala/swift 恒等。
    assert_eq!(ls_registry::lsp_language_id("intelephense"), "php");
    assert_eq!(ls_registry::lsp_language_id("lua"), "lua");
    assert_eq!(ls_registry::lsp_language_id("scala"), "scala");
    assert_eq!(ls_registry::lsp_language_id("swift"), "swift");

    // 扩展名分流：四门独占，luau 独立语言门不别名。
    assert_eq!(LanguageId::from_extension("php"), Some(LanguageId::Php));
    assert_eq!(LanguageId::from_extension("lua"), Some(LanguageId::Lua));
    assert_eq!(LanguageId::from_extension("scala"), Some(LanguageId::Scala));
    assert_eq!(LanguageId::from_extension("swift"), Some(LanguageId::Swift));
    assert_eq!(
        LanguageId::from_str_opt("luau"),
        Some(LanguageId::Luau),
        "luau ≠ lua（W6 批独立语言门）"
    );
    assert_eq!(
        LanguageId::from_str_opt("intelephense"),
        Some(LanguageId::Php),
        "php 门 entry id 别名"
    );

    // php 门：intelephense npm 条目 pin 锚（phase3 NPM_IDS 已有，此处锁冒烟门消费面）。
    let intel = servers
        .get("intelephense")
        .expect("[servers.intelephense] missing");
    assert_eq!(intel.install, "npm");
    let intel_npm = intel.npm.as_ref().expect("intelephense: npm table");
    assert_eq!(intel_npm.package, "intelephense");
    assert_eq!(intel_npm.version.as_deref(), Some("1.14.4"));
    assert_eq!(
        intel_npm.npm_args.as_deref(),
        Some(["--stdio".to_string()].as_slice())
    );

    // lua 门：LuaLS download pin 对账（上游 lua_ls.py@43ae0211 release 3.15.0）。
    let lua = servers.get("lua").expect("[servers.lua] missing");
    assert_eq!(lua.languages, vec!["lua"]);
    assert_eq!(lua.install, "download");
    let lua_dl = lua.download.as_ref().expect("lua: download table");
    assert_eq!(lua_dl.version, "3.15.0");
    assert_eq!(lua_dl.archive, "tar.gz");
    assert!(
        lua_dl.url_per_platform.contains_key("linux-x86_64"),
        "CI ubuntu 门必需平台"
    );
    assert_eq!(
        lua_dl.sha256_per_platform.len(),
        lua_dl.url_per_platform.len(),
        "url/sha 1:1"
    );

    // scala 门：metals path_only（本批唯一新增条目；上游 DEFAULT_METALS_VERSION
    // 1.6.4 锚写进 install_hint，path_only 无资产无版本字段）。
    let scala = servers.get("scala").expect("[servers.scala] missing");
    assert_eq!(scala.languages, vec!["scala"]);
    assert_eq!(scala.install, "path_only");
    let scala_po = scala.path_only.as_ref().expect("scala: path_only table");
    assert_eq!(scala_po.binary_name, "metals");
    assert!(
        scala_po.install_hint.contains("metals_2.13:1.6.4"),
        "install_hint 必须携带上游 pin 锚: {:?}",
        scala_po.install_hint
    );

    // swift 门：存量 sourcekit_lsp path_only（Xcode 工具链形态，ubuntu 门 PLATFORM SKIP）。
    let sk = servers
        .get("sourcekit_lsp")
        .expect("[servers.sourcekit_lsp] missing");
    assert_eq!(sk.languages, vec!["swift"]);
    assert_eq!(sk.install, "path_only");
    assert_eq!(
        sk.path_only
            .as_ref()
            .expect("swift: path_only table")
            .binary_name,
        "sourcekit-lsp"
    );
}

/// W4 批（haskell/groovy/ocaml/erlang/perl/r/crystal/zig/fortran/pascal）验收单测：
/// 九门 T0 分流（adapter_for None → ensure_launch），groovy 无条目（上游 H 类自备
/// JAR，angular/java 不入表先例）；lsp_language_id 十门恒等；存量条目 exec 修复锁
/// （haskell `--lsp` / erlang `--transport stdio`——裸启动分别是 usage 打印即退 /
/// TCP 模式，terraform serve 同类坑）；zls path_only→download 升级六平台 pin 锚
/// （GitHub API assets[].digest，2026-09-29）；perl/r 新条目 launch argv 逐字锁
/// （↖ mirror @7a296833）；扩展名闭环 + resolve 大小写不敏感回归。
#[test]
fn w4_ten_doors_wired() {
    use ls_registry::LanguageId;
    use ls_registry::adapter_for;
    use ls_registry::config::spec_for;

    let servers = parsed_servers();

    // T0 分流 + 显式路由闭环（fortran/pascal 为存量门接线闭合）。
    for (lang, want_id) in [
        ("fortran", "fortls"),
        ("pascal", "pascal"),
        ("haskell", "haskell_ls"),
        ("ocaml", "ocamllsp"),
        ("erlang", "erlang_ls"),
        ("perl", "perl_ls"),
        ("r", "r_ls"),
        ("crystal", "crystalline"),
        ("zig", "zls"),
    ] {
        let (id, _) =
            spec_for(lang).unwrap_or_else(|| panic!("--lang {lang} must route to a spec"));
        assert_eq!(id, want_id, "spec_for(\"{lang}\") routes to [servers.{id}]");
        assert!(
            adapter_for(lang).is_none(),
            "{lang}: W4 门全 T0，adapter_for 必须为 None"
        );
    }
    // groovy：上游 H 类用户自备 JAR（四路线全灭），无条目；LanguageId 占位 + 扩展名
    // 路由使 resolve 落到明确 no-entry 报错（SKIP 候选待 PM 裁决）。
    assert!(
        spec_for("groovy").is_none(),
        "groovy 无条目（上游 H 类自备 ls_jar_path，angular/java 先例）"
    );
    assert!(adapter_for("groovy").is_none());

    // LSP didOpen 官方口径：十门恒等（上游 adapter 第四参逐一核实 = 内部名）。
    for lang in [
        "fortran", "pascal", "haskell", "groovy", "ocaml", "erlang", "perl", "r", "crystal", "zig",
    ] {
        assert_eq!(ls_registry::lsp_language_id(lang), lang);
    }

    // 扩展名闭环（from_extension 大小写不敏感：键统一小写 + to_lowercase 匹配）。
    assert_eq!(LanguageId::from_extension("f90"), Some(LanguageId::Fortran));
    assert_eq!(LanguageId::from_extension("F90"), Some(LanguageId::Fortran));
    assert_eq!(LanguageId::from_extension("pas"), Some(LanguageId::Pascal));
    assert_eq!(LanguageId::from_extension("hs"), Some(LanguageId::Haskell));
    assert_eq!(LanguageId::from_extension("lhs"), Some(LanguageId::Haskell));
    assert_eq!(
        LanguageId::from_extension("groovy"),
        Some(LanguageId::Groovy)
    );
    assert_eq!(LanguageId::from_extension("ml"), Some(LanguageId::Ocaml));
    assert_eq!(LanguageId::from_extension("mli"), Some(LanguageId::Ocaml));
    assert_eq!(LanguageId::from_extension("erl"), Some(LanguageId::Erlang));
    assert_eq!(LanguageId::from_extension("pl"), Some(LanguageId::Perl));
    assert_eq!(LanguageId::from_extension("t"), Some(LanguageId::Perl));
    assert_eq!(LanguageId::from_extension("r"), Some(LanguageId::R));
    assert_eq!(LanguageId::from_extension("RMD"), Some(LanguageId::R));
    assert_eq!(LanguageId::from_extension("cr"), Some(LanguageId::Crystal));
    assert_eq!(LanguageId::from_extension("zig"), Some(LanguageId::Zig));
    assert_eq!(LanguageId::from_extension("zon"), Some(LanguageId::Zig));

    assert_eq!(
        ls_registry::resolve(std::path::Path::new("main.f90")),
        Some(LanguageId::Fortran)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("SMOKE.PP")),
        Some(LanguageId::Pascal)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("Main.hs")),
        Some(LanguageId::Haskell)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("main.erl")),
        Some(LanguageId::Erlang)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("script.PL")),
        Some(LanguageId::Perl)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("plot.r")),
        Some(LanguageId::R)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("app.cr")),
        Some(LanguageId::Crystal)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("build.zon")),
        Some(LanguageId::Zig)
    );

    // haskell_ls exec 修复锁：↖ mirror haskell_language_server.py@7a296833
    // [wrapper, "--lsp", "--cwd", workdir]（--cwd 省略 = 进程 cwd 即项目根）。
    let hls = servers
        .get("haskell_ls")
        .expect("[servers.haskell_ls] missing");
    assert_eq!(hls.install, "path_only");
    assert_eq!(hls.exec, vec!["{bin}", "--lsp"]);

    // erlang_ls exec 修复锁：↖ mirror erlang_language_server.py@7a296833
    // ProcessLaunchInfo [erlang_ls, "--transport", "stdio"]（默认 transport TCP）。
    let els = servers
        .get("erlang_ls")
        .expect("[servers.erlang_ls] missing");
    assert_eq!(els.install, "path_only");
    assert_eq!(els.exec, vec!["{bin}", "--transport", "stdio"]);

    // perl_ls 新条目：launch argv 逐字（`perl -MPerl::LanguageServer -e ...`）。
    let perl = servers.get("perl_ls").expect("[servers.perl_ls] missing");
    assert_eq!(perl.languages, vec!["perl"]);
    assert_eq!(perl.install, "path_only");
    assert_eq!(
        perl.exec,
        vec![
            "{bin}",
            "-MPerl::LanguageServer",
            "-e",
            "Perl::LanguageServer::run"
        ]
    );
    assert_eq!(
        perl.path_only
            .as_ref()
            .expect("perl: path_only table")
            .binary_name,
        "perl"
    );

    // r_ls 新条目：launch 串逐字（`R --vanilla --quiet --slave -e ...`）。
    let r = servers.get("r_ls").expect("[servers.r_ls] missing");
    assert_eq!(r.languages, vec!["r"]);
    assert_eq!(r.install, "path_only");
    assert_eq!(
        r.exec,
        vec![
            "{bin}",
            "--vanilla",
            "--quiet",
            "--slave",
            "-e",
            "options(languageserver.debug_mode = FALSE); languageserver::run()"
        ]
    );
    assert_eq!(
        r.path_only
            .as_ref()
            .expect("r: path_only table")
            .binary_name,
        "R"
    );

    // zls download 升级锚（regal/deno 先例：存量条目独立锚定，不入 NEW_DOWNLOAD_IDS）。
    let zls = servers.get("zls").expect("[servers.zls] missing");
    assert_eq!(zls.install, "download");
    assert_eq!(zls.source_commit.as_deref(), Some("7a296833"));
    let zls_dl = zls.download.as_ref().expect("zls: download table");
    assert_eq!(zls_dl.version, "0.16.0");
    assert_eq!(zls_dl.archive, "tar.xz");
    assert_eq!(zls_dl.url_per_platform.len(), 6, "六平台 pin");
    assert_eq!(
        zls_dl
            .sha256_per_platform
            .get("linux-x86_64")
            .map(String::as_str),
        Some("ded6d562a0b86ee878b1ddf70ffab2797ce3cdca3b02d6077548f9d56dff96b6"),
        "GitHub API assets[].digest 2026-09-29 锚"
    );
    assert_eq!(
        zls_dl
            .bin_path_per_platform
            .get("linux-x86_64")
            .map(String::as_str),
        Some("zls"),
        "unix bin 无 .exe（deno 同款）"
    );

    // crystalline canary：legacy path_only 锚不被 download 化侵蚀（smoke 走 curl 钉 URL，
    // 条目升级留待上游 crystalline 完整多平台资产 + PM 裁决）。
    assert_eq!(
        servers
            .get("crystalline")
            .expect("[servers.crystalline] missing")
            .install,
        "path_only"
    );
}

/// W5 七门：真门 gleam/qml/lean4/julia + SKIP 候选 wolfram/gdscript/msl。
/// gleam/qml/lean4 = 存量条目接线（G 类批量收录，本体不动）；julia/wolfram = 本批
/// 新增条目（path_only + 运行时 exec argv / WolframKernel 探测面）；gdscript/msl
/// 无条目（HOST SKIP 候选待 PM 裁决，angular/java/groovy 先例）。
#[test]
fn w5_seven_doors_wired() {
    use ls_registry::LanguageId;
    use ls_registry::adapter_for;
    use ls_registry::config::spec_for;

    let servers = parsed_servers();

    // T0 路由闭环：四真门语言名 + lean4 条目 id 双语义命中；门本体无 adapter。
    for (lang, want_id) in [
        ("gleam", "gleam"),
        ("qml", "qmlls"),
        ("lean", "lean4"),
        ("lean4", "lean4"),
        ("julia", "julia"),
        ("wolfram", "wolfram"),
    ] {
        let (id, _) =
            spec_for(lang).unwrap_or_else(|| panic!("--lang {lang} must route to a spec"));
        assert_eq!(id, want_id, "spec_for(\"{lang}\") routes to [servers.{id}]");
        assert!(
            adapter_for(lang).is_none(),
            "{lang}: W5 门全 T0，adapter_for 必须为 None"
        );
    }
    // gdscript/msl：HOST SKIP 候选无条目（--lang 落空报明确 no entry）。
    assert!(
        spec_for("gdscript").is_none(),
        "gdscript 无条目（godot 编辑器 TCP 宿主，TransportKind 仅 Stdio）"
    );
    assert!(
        spec_for("msl").is_none(),
        "msl 无条目（上游 LS = serena 内嵌 pygls 脚本，非独立发行）"
    );
    assert!(adapter_for("gdscript").is_none());
    assert!(adapter_for("msl").is_none());

    // LSP didOpen 官方口径：七门恒等（lean = VS Code lean 扩展官方 languageId；
    // lean4 仅条目 id 不进换算面）。
    for lang in [
        "gleam", "qml", "lean", "julia", "wolfram", "gdscript", "msl",
    ] {
        assert_eq!(ls_registry::lsp_language_id(lang), lang);
    }

    // 扩展名闭环（resolve 走 EXT_TABLE）。
    assert_eq!(LanguageId::from_extension("gleam"), Some(LanguageId::Gleam));
    assert_eq!(LanguageId::from_extension("qml"), Some(LanguageId::Qml));
    assert_eq!(LanguageId::from_extension("lean"), Some(LanguageId::Lean));
    assert_eq!(LanguageId::from_extension("jl"), Some(LanguageId::Julia));
    assert_eq!(LanguageId::from_extension("wl"), Some(LanguageId::Wolfram));
    assert_eq!(LanguageId::from_extension("nb"), Some(LanguageId::Wolfram));
    assert_eq!(LanguageId::from_extension("gd"), Some(LanguageId::Godot));
    assert_eq!(LanguageId::from_extension("mrc"), Some(LanguageId::Msl));
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("main.gleam")),
        Some(LanguageId::Gleam)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("MAIN.QML")),
        Some(LanguageId::Qml)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("Main.lean")),
        Some(LanguageId::Lean)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("main.jl")),
        Some(LanguageId::Julia)
    );
    assert_eq!(
        ls_registry::resolve(std::path::Path::new("main.wl")),
        Some(LanguageId::Wolfram)
    );

    // gleam/qml/lean4 存量条目接线锁（G 类本体不动）：入口形态 = 上游 launch 逐字。
    let gleam = servers.get("gleam").expect("[servers.gleam] missing");
    assert_eq!(gleam.languages, vec!["gleam"]);
    assert_eq!(gleam.install, "path_only");
    assert_eq!(
        gleam.exec,
        vec!["{bin}", "lsp"],
        "gleam lsp 子命令（deno 先例）"
    );
    let qml = servers.get("qmlls").expect("[servers.qmlls] missing");
    assert_eq!(qml.languages, vec!["qml"]);
    assert_eq!(
        qml.path_only
            .as_ref()
            .expect("qml: path_only table")
            .binary_name,
        "qmlls"
    );
    let lean4 = servers.get("lean4").expect("[servers.lean4] missing");
    assert_eq!(lean4.languages, vec!["lean"]);
    assert_eq!(
        lean4.exec,
        vec!["{bin}", "--server"],
        "lean --server（上游 launch）"
    );

    // julia 新条目：launch argv 逐字（repo_root 尾参省略 = runserver choose_env
    // pwd 回落，T0 spawn cwd = 项目根实锚 supervisor/src/lib.rs session_for）。
    let julia = servers.get("julia").expect("[servers.julia] missing");
    assert_eq!(julia.languages, vec!["julia"]);
    assert_eq!(julia.extensions, vec![".jl"]);
    assert_eq!(julia.install, "path_only");
    assert_eq!(
        julia.exec,
        vec![
            "{bin}",
            "--startup-file=no",
            "--history-file=no",
            "-e",
            "using LanguageServer; runserver()"
        ]
    );
    assert_eq!(
        julia
            .path_only
            .as_ref()
            .expect("julia: path_only table")
            .binary_name,
        "julia"
    );

    // wolfram 新条目：WolframKernel 探测面 + LSPServer paclet 启动串逐字。
    let wolfram = servers.get("wolfram").expect("[servers.wolfram] missing");
    assert_eq!(wolfram.languages, vec!["wolfram"]);
    assert_eq!(wolfram.extensions, vec![".wl", ".nb"]);
    assert_eq!(wolfram.install, "path_only");
    assert_eq!(
        wolfram.exec,
        vec![
            "{bin}",
            "-noprompt",
            "-noinit",
            "-run",
            "Needs[\"LSPServer`\"];LSPServer`StartServer[]"
        ]
    );
    assert_eq!(
        wolfram
            .path_only
            .as_ref()
            .expect("wolfram: path_only table")
            .binary_name,
        "WolframKernel"
    );
}
