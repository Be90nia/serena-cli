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
        // 升版锚例外：kotlin 263.4702.0（上游 DEFAULT_KOTLIN_LSP_VERSION + sha 锚）取自
        // 7a296833 kotlin_language_server.py 与同 commit 的 downloaded_dependency_hashes.json；
        // 其余条目数据源锚 43ae0211。
        let anchor = match *id {
            "kotlin" => "7a296833",
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
        68,
        "存量 38（14 legacy + 24 A 类）+ Phase 3 新收 24 + astro（7a296833）+ bd 56a 第一批 +2 docker/sql + 第二批 +2 pgls/sqls-mysql + 后续批 +1 css（html/yaml/marksman/kotlin/dart 为存量条目，本批零新增）"
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
        dl.url_per_platform.get("windows-x86_64").map(String::as_str),
        Some("https://github.com/sqls-server/sqls/releases/download/v0.2.48/sqls-windows-0.2.48.zip")
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
        kdl.url_per_platform.get("windows-x86_64").map(String::as_str),
        Some("https://download-cdn.jetbrains.com/language-server/kotlin-server/263.4702.0/kotlin-server-263.4702.0.win.zip")
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
        ddl.url_per_platform.get("windows-x86_64").map(String::as_str),
        Some("https://storage.googleapis.com/dart-archive/channels/stable/release/3.7.1/sdk/dartsdk-windows-x64-release.zip")
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
        dl.url_per_platform.get("windows-x86_64").map(String::as_str),
        Some("https://github.com/supabase-community/postgres-language-server/releases/download/0.25.7/postgres-language-server_x86_64-pc-windows-msvc.exe")
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
    use ls_registry::adapter_for;
    use ls_registry::config::spec_for;
    use ls_registry::LanguageId;

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
    assert_eq!(
        LanguageId::from_str_opt("pgsql"),
        Some(LanguageId::Pgsql)
    );
    assert_eq!(
        LanguageId::from_str_opt("postgres"),
        Some(LanguageId::Pgsql),
        "postgres 别名同归 Pgsql"
    );
    assert_eq!(
        LanguageId::from_str_opt("mysql"),
        Some(LanguageId::Mysql)
    );
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
    use ls_registry::adapter_for;
    use ls_registry::config::spec_for;
    use ls_registry::LanguageId;

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
    assert_eq!(
        LanguageId::from_extension("md"),
        Some(LanguageId::Markdown)
    );
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
