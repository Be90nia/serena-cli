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

pub use ls_adapters::LanguageId;
use ls_adapters::{
    LanguageServerAdapter, angular::AngularAdapter, astro::AstroAdapter, bash::BashAdapter,
    clangd::ClangdAdapter, csharp_ls::CsharpLsAdapter, css::CssAdapter, deno::DenoAdapter,
    gopls::GoplsAdapter, html::HtmlAdapter, jdtls::JdtlsAdapter, json::JsonAdapter,
    nextflow::NextflowAdapter, powershell::PowerShellAdapter, pyrefly_server::PyreflyServerAdapter,
    pyright::PyrightAdapter, rust_analyzer::RustAnalyzerAdapter, sass::SassAdapter,
    scala::ScalaAdapter, svelte::SvelteAdapter, typescript::TypescriptLanguageServerAdapter,
    vts::VtsAdapter, vue::VueAdapter,
};

/// 扩展名 → LanguageId 静态表（小写键）。
///
/// M3 覆盖 7 个 LanguageId（M0 仅 cpp 系）。
/// ts/js 同走 TypeScript LS —— 解析时归到 TypeScript；adapter_for 按 lang 维度分。
/// `pub(crate)`：config.rs 加载 external-servers.toml 时检测扩展名撞表并 warn。
pub(crate) const EXT_TABLE: &[(&str, LanguageId)] = &[
    // C / C++
    ("c", LanguageId::Cpp),
    ("cpp", LanguageId::Cpp),
    ("cc", LanguageId::Cpp),
    ("cxx", LanguageId::Cpp),
    ("h", LanguageId::Cpp),
    ("hpp", LanguageId::Cpp),
    // Markdown（T0 配置驱动：servers.toml marksman，Task 21 双路径）
    ("md", LanguageId::Markdown),
    ("markdown", LanguageId::Markdown),
    // YAML（T0 配置驱动：servers.toml yaml 条目，bd 56a 后续批次）
    ("yaml", LanguageId::Yaml),
    ("yml", LanguageId::Yaml),
    // SQL / Dockerfile（T0 配置驱动：servers.toml sql/docker，bd 56a 第一批）。
    // 本批 sql 独占 .sql（pgsql/mysql 第二批用 --lang 显式覆盖，不进本表）。
    ("sql", LanguageId::Sql),
    ("dockerfile", LanguageId::Docker),
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
    // Bash / shell（bash-language-server 兼容 POSIX sh 语法）
    ("sh", LanguageId::Bash),
    ("bash", LanguageId::Bash),
    // JSON（jsonc = 带注释 JSON，同一 LS 接管）
    ("json", LanguageId::Json),
    ("jsonc", LanguageId::Json),
    // PowerShell
    ("ps1", LanguageId::PowerShell),
    ("psm1", LanguageId::PowerShell),
    ("psd1", LanguageId::PowerShell),
    // Vue 单文件组件
    ("vue", LanguageId::Vue),
    // Kotlin / Dart（bd 56a 后续批，T0 配置驱动：servers.toml kotlin/dart 条目）
    ("kt", LanguageId::Kotlin),
    ("kts", LanguageId::Kotlin),
    ("dart", LanguageId::Dart),
    // Astro 单文件组件（ts/js 仍归 TypeScript —— 上游 superset 优先级 1 的
    // 项目级覆盖我们无对应机制，--lang astro 显式指定即达同效）
    ("astro", LanguageId::Astro),
    // HTML / CSS（bd 56a 后续批）
    ("html", LanguageId::Html),
    ("htm", LanguageId::Html),
    ("css", LanguageId::Css),
    // Rego / Nextflow（W1b 批，T0 配置驱动：servers.toml regal/nextflow 条目）。
    // ansible 不进本表：.yaml/.yml 归 Yaml 门（momus 裁决的冲突归属），
    // ansible 仅 --lang 显式路由可达。
    ("rego", LanguageId::Rego),
    ("nf", LanguageId::Nextflow),
    // TOML / Terraform / Cue / Nix（W1a 批，T0 配置驱动：servers.toml
    // toml/terraform/cue/nixd 条目）。
    ("toml", LanguageId::Toml),
    ("tf", LanguageId::Terraform),
    ("tfvars", LanguageId::Terraform),
    ("cue", LanguageId::Cue),
    ("nix", LanguageId::Nix),
    // W2 批：svelte 单文件组件；sass 双扩展（.css 留在 css 门，上游把 .css 也给
    // some-sass 的路由我们不抄）。deno 不进本表——TS 家族归 TypeScript 门，
    // 仅 --lang deno 显式路由可达（pgsql/mysql 先例）。
    ("svelte", LanguageId::Svelte),
    ("sass", LanguageId::Sass),
    ("scss", LanguageId::Sass),
    // W3 批：php/lua/scala/swift 各自独占扩展名（上游 get_priority superset 无冲突：
    // .php/.lua/.scala/.swift 此前无归属）。
    ("php", LanguageId::Php),
    ("lua", LanguageId::Lua),
    ("scala", LanguageId::Scala),
    ("swift", LanguageId::Swift),
    // W4 批：fortran 大小写不敏感族 / pascal 主形态 / haskell / groovy / ocaml /
    // erlang / perl / r 族 / crystal / zig。groovy 门本体 HOST SKIP 候选（上游 H 类
    // 自备 JAR 无条目）——扩展名占位使 resolve 落到明确的 no-entry 报错而非静默未知。
    // ocaml .ml/.mli 与泛用后缀（.config/.app/.inc 等）不冲突：MATLAB 的 .m 不在本表。
    ("f90", LanguageId::Fortran),
    ("f95", LanguageId::Fortran),
    ("f03", LanguageId::Fortran),
    ("f08", LanguageId::Fortran),
    ("f", LanguageId::Fortran),
    ("for", LanguageId::Fortran),
    ("fpp", LanguageId::Fortran),
    ("pas", LanguageId::Pascal),
    ("pp", LanguageId::Pascal),
    ("hs", LanguageId::Haskell),
    ("lhs", LanguageId::Haskell),
    ("groovy", LanguageId::Groovy),
    ("gvy", LanguageId::Groovy),
    ("ml", LanguageId::Ocaml),
    ("mli", LanguageId::Ocaml),
    ("erl", LanguageId::Erlang),
    ("hrl", LanguageId::Erlang),
    ("pl", LanguageId::Perl),
    ("pm", LanguageId::Perl),
    ("t", LanguageId::Perl),
    ("r", LanguageId::R),
    ("rmd", LanguageId::R),
    ("rnw", LanguageId::R),
    ("cr", LanguageId::Crystal),
    ("zig", LanguageId::Zig),
    ("zon", LanguageId::Zig),
    // W5 批：七门各自独占扩展名，无存量冲突（.m 归 matlab、.ts 归 TypeScript 均不涉）。
    // wolfram 双扩展 .wl + .nb；gdscript/msl 门本体 SKIP 候选——扩展名占位使 resolve
    // 落到明确的 no-entry 报错而非静默未知（groovy 同款）。
    ("gleam", LanguageId::Gleam),
    ("qml", LanguageId::Qml),
    ("lean", LanguageId::Lean),
    ("jl", LanguageId::Julia),
    ("wl", LanguageId::Wolfram),
    ("nb", LanguageId::Wolfram),
    ("gd", LanguageId::Godot),
    ("mrc", LanguageId::Msl),
    // W6 批（上游对拍采纳）：十二门扩展名收口——条目全部已在 servers.toml（B/C 批
    // 交付模板的「G 类欠账清单」），本批仅补路由。.m 刻意不收（MATLAB 歧义已裁决）。
    ("clj", LanguageId::Clojure),
    ("cljs", LanguageId::Clojure),
    ("cljc", LanguageId::Clojure),
    ("edn", LanguageId::Clojure),
    ("elm", LanguageId::Elm),
    ("hx", LanguageId::Haxe),
    ("luau", LanguageId::Luau),
    ("fs", LanguageId::FSharp),
    ("fsi", LanguageId::FSharp),
    ("fsx", LanguageId::FSharp),
    ("bsl", LanguageId::Bsl),
    ("os", LanguageId::Bsl),
    ("sv", LanguageId::SystemVerilog),
    ("svh", LanguageId::SystemVerilog),
    ("v", LanguageId::SystemVerilog),
    ("vh", LanguageId::SystemVerilog),
    ("tex", LanguageId::Latex),
    ("bib", LanguageId::Latex),
    ("sol", LanguageId::Solidity),
    ("ada", LanguageId::Ada),
    ("adb", LanguageId::Ada),
    ("ads", LanguageId::Ada),
    ("al", LanguageId::Al),
    // hlsl 族 15 个（↖ mirror hlsl.py@7a296833 上游支持全清单）。
    ("hlsl", LanguageId::Hlsl),
    ("hlsli", LanguageId::Hlsl),
    ("fx", LanguageId::Hlsl),
    ("fxh", LanguageId::Hlsl),
    ("cginc", LanguageId::Hlsl),
    ("compute", LanguageId::Hlsl),
    ("shader", LanguageId::Hlsl),
    ("glsl", LanguageId::Hlsl),
    ("vert", LanguageId::Hlsl),
    ("frag", LanguageId::Hlsl),
    ("geom", LanguageId::Hlsl),
    ("tesc", LanguageId::Hlsl),
    ("tese", LanguageId::Hlsl),
    ("comp", LanguageId::Hlsl),
    ("wgsl", LanguageId::Hlsl),
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
singleton!(BASH, BashAdapter);
singleton!(JSON, JsonAdapter);
singleton!(POWERSHELL, PowerShellAdapter);
singleton!(VUE, VueAdapter);
singleton!(ASTRO, AstroAdapter);
singleton!(HTML, HtmlAdapter);
singleton!(CSS, CssAdapter);
singleton!(SVELTE, SvelteAdapter);
singleton!(DENO, DenoAdapter);
singleton!(SASS, SassAdapter);
singleton!(VTS, VtsAdapter);
singleton!(NEXTFLOW, NextflowAdapter);
singleton!(SCALA, ScalaAdapter);
singleton!(PYREFLY, PyreflyServerAdapter);
singleton!(ANGULAR, AngularAdapter);

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

/// 路径 → 语言名（external-ls-registration-design §2 extension 路由）。
///
/// 先查内置 `EXT_TABLE`（手写语言优先，external 声明同扩展名时内置胜）；未命中查
/// external-servers.toml 声明的 extensions → 该条目 `languages[0]`（session_for /
/// spec_for 按语言名走配置驱动启动）。两者皆未命中 → None。
pub fn resolve_lang_name(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    EXT_TABLE
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, lang)| lang.as_str())
        .or_else(|| config::external_table().and_then(|t| config::match_external_ext(t, &ext)))
}

/// 语言在当前注册状态下是否存在真实请求路径（blindtest v5 P1-1 判据源，与安装态
/// 正交：installed 只代表缓存包在盘）。true = external 注册拥有（ls-use 生效面）/
/// T2 adapter 直路由 / EXT_TABLE 扩展名路由（T0 配置驱动）三者任一；false = 该
/// 语言目前无从文件名到达（.rb/.m 类——包在盘也路由失败，BAD_ARGS unsupported）。
pub fn language_routable(lang: &str) -> bool {
    let l = lang.to_ascii_lowercase();
    if config::spec_source(&l) == Some("external") {
        return true;
    }
    if adapter_for(&l).is_some() {
        return true;
    }
    if let Some(id) = LanguageId::from_str_opt(&l) {
        return EXT_TABLE.iter().any(|(_, t)| *t == id);
    }
    false
}

/// 避撞变体条目 → LS 家族真语言。这些条目的 `languages` 用自身 id 占位避撞
/// （[servers.csharp_ls] / [servers.omnisharp] / [servers.pyright] 注释先例：
/// "X 语言路由归 Y 条目 / 手写 adapter 接管，本条仅按 id 显式安装"）——真语言
/// 经家族门路由。entry_routable 的折算表；家族语言不可路由时条目仍如实 false
/// （solargraph→ruby：.rb 无映射，两代都 false）。
const VARIANT_ENTRY_FAMILY: &[(&str, &str)] = &[
    ("pyright", "python"),
    ("jedi", "python"),
    ("python_ty", "python"),
    ("basedpyright", "python"),
    ("csharp_ls", "csharp"),
    ("omnisharp", "csharp"),
    ("typescript_ls", "typescript"),
    ("crystalline_source", "crystal"),
    ("phpantom", "php"),
    ("solargraph", "ruby"),
];

/// ls-list/doctor 条目级 routable（blindtest v5 P1-1 判据源）：语言门任一可路由，
/// 或条目 id 本身可路由，或为家族语言可路由的避撞变体条目。
pub fn entry_routable(id: &str, languages: &[String]) -> bool {
    if languages.iter().any(|l| language_routable(l)) || language_routable(id) {
        return true;
    }
    let Some((_, fam)) = VARIANT_ENTRY_FAMILY.iter().find(|(v, _)| *v == id) else {
        return false;
    };
    language_routable(fam)
}

/// 扩展名是否被内置 servers.toml 条目的 `extensions` 字段登记 → 返回该条目首选
/// 语言名。仅供 resolve 失败分支的报错文案分流（「注册了但无 adapter」vs「真未知
/// 扩展名」）；路由本身不读此表（文件探测权威归 EXT_TABLE，spec.rs 字段注释）。
pub fn registered_lang_for_extension(ext: &str) -> Option<&'static str> {
    let e = ext.trim_start_matches('.').to_ascii_lowercase();
    config::builtin_entries().into_iter().find_map(|(_, spec)| {
        let hit = spec
            .extensions
            .iter()
            .any(|x| x.trim_start_matches('.').eq_ignore_ascii_case(&e));
        hit.then(|| spec.languages.first().map(String::as_str))?
    })
}

/// 语言字符串 → adapter 单例。M3 覆盖 7 手写语言。
///
/// T0 配置驱动语言（servers.toml，如 markdown）**不走此处**——它们的启动经
/// `config::ensure_launch`（Task 19），supervisor 接线归 Task 21。
/// 返回 `Arc` 让调用方按 trait 对象持有；`Arc::ptr_eq` 在两次调用间成立（LazyLock 单例）。
/// 未知语言 / 空串返回 `None`。
pub fn adapter_for(lang: &str) -> Option<Arc<dyn LanguageServerAdapter>> {
    // 变体门显式路由（smoke R4）：typescript_vts 是 LS 变体不是语言，不进
    // LanguageId 枚举——字符串直路由（vts.rs 模块文档）。deno 显式路由门同款思路。
    if lang == "typescript_vts" {
        return Some(VTS.clone());
    }
    // bd serena-rust-9z0x：语言被 external-servers.toml 条目拥有（ls-use 注册 /
    // 用户手编，merged 命中 external = 用户显式胜出）时 T2 硬编码路由让位——
    // supervisor 冷启动回落 ensure_launch 走注册条目（path_only 绝对路径可达）。
    // 否则 ls-use 对 T2 语言（python→PYRIGHT 等）永远无效：adapter 自行 PATH
    // 探测继续失败报 not installed，还建议用户重做已做过且无效的 ls-use
    // （NitpickAI F1 实锤死循环）。
    if config::spec_source(lang) == Some("external") {
        return None;
    }
    let id = LanguageId::from_str_opt(lang)?;
    Some(match id {
        // r8.5 adopt：devsense_php 是 T0 配置驱动（servers.toml npm 条目），无手写
        // 单例——此处返回 None，调用方回落 Task21 双路径的 ensure_launch 支
        // （table_hit 按 languages=["php_devsense"] 命中 devsense_php 条目）。
        LanguageId::PhpDevsense => return None,
        LanguageId::Cpp => CLANGD.clone(),
        LanguageId::Rust => RUST_ANALYZER.clone(),
        LanguageId::Python => PYRIGHT.clone(),
        LanguageId::Go => GOPLS.clone(),
        LanguageId::TypeScript => TYPESCRIPT.clone(),
        LanguageId::CSharp => CSHARP_LS.clone(),
        LanguageId::Java => JDTLS.clone(),
        LanguageId::Bash => BASH.clone(),
        LanguageId::Json => JSON.clone(),
        LanguageId::PowerShell => POWERSHELL.clone(),
        LanguageId::Vue => VUE.clone(),
        LanguageId::Astro => ASTRO.clone(),
        // bd 56a 后续批：html/css 手写 T2（同包 vscode-langservers-extracted 双入口）。
        LanguageId::Html => HTML.clone(),
        LanguageId::Css => CSS.clone(),
        // W2 批：svelte/deno/sass 手写 T2（svelte = hybrid 双服务器，astro 同构；
        // deno 注入 enable/lint 初始化选项并处理 lsp 子命令入口；sass 注入 somesass
        // 配置片 + somesass configuration 应答）。
        LanguageId::Svelte => SVELTE.clone(),
        LanguageId::Deno => DENO.clone(),
        LanguageId::Sass => SASS.clone(),
        // T0 配置驱动语言（markdown/yaml）：无手写 adapter（见 config::ensure_launch）。
        LanguageId::Markdown | LanguageId::Yaml => return None,
        // T0 配置驱动（bd 56a 第一批）：docker/sql 走 servers.toml docker/sql 条目。
        LanguageId::Docker | LanguageId::Sql => return None,
        // T0 配置驱动（bd 56a 第二批）：pgsql/mysql 走 servers.toml pgls/sqls-mysql 条目。
        LanguageId::Pgsql | LanguageId::Mysql => return None,
        // T0 配置驱动（bd 56a 后续批）：kotlin/dart 走 servers.toml kotlin/dart 条目
        // （download 形态，ensure_launch 接管）。
        LanguageId::Kotlin | LanguageId::Dart => return None,
        // T0 配置驱动（W1b 批）：ansible/regal 条目（npm/download 形态，
        // ensure_launch 接管）；ansible 仅 --lang 显式路由可达。
        LanguageId::Ansible | LanguageId::Rego => return None,
        // nextflow：T2 接管会话（W3 采纳——config 推送外的 references flush/
        // 符号名前缀剥离/progress 等待需适配器挂点，bd 69e findings batchC）；
        // servers.toml 条目留 install/doctor（did_change_config 通道仍按 spec 应用）。
        LanguageId::Nextflow => NEXTFLOW.clone(),
        // pyrefly：T2 接管会话（W3b 采纳——workspace/configuration 必答 + pyrefly.toml
        // 边界 + $/progress 索引等待需适配器挂点，bd 69e findings batchA 缺失[高]）；
        // servers.toml [servers.python_pyrefly].uvx 留 install/doctor 面。.py 归主
        // Python 门，--lang python_pyrefly 显式路由可达。
        LanguageId::PythonPyrefly => PYREFLY.clone(),
        // angular：T2 三服务器编排（W3b 采纳——主 tsls 挂 @angular/language-service
        // 插件 + ngserver/html 伴生经 session_for_file 重路由，bd 69e findings
        // batchC）；servers.toml [servers.angular].npm 留 install/doctor 面（四包
        // 同装）。.ts/.html 归既有门，--lang angular 显式路由可达。
        LanguageId::Angular => ANGULAR.clone(),
        // T0 配置驱动（W1a 批）：toml(taplo)/terraform(terraform-ls)/cue(cue lsp
        // 内置)/nixd(source 构建形态) 条目，ensure_launch 接管。
        LanguageId::Toml | LanguageId::Terraform | LanguageId::Cue | LanguageId::Nix => {
            return None;
        }
        // T0 配置驱动（W3 批）：php(phpactor/intelephense 双条目，--lang intelephense
        // 按 entry id 显式路由)/lua(LuaLS)/swift(sourcekit-lsp path_only) 走
        // servers.toml 条目，ensure_launch 接管。
        LanguageId::Php | LanguageId::Lua | LanguageId::Swift => return None,
        // scala：T2 接管会话（W3 采纳——build-root 探测作 workspaceFolders、
        // MetalsProgressTracker 首查等待、showMessageRequest 自动应答 "Import
        // build"，bd 69e findings batchA 缺失[高]）；servers.toml 条目留 install/doctor。
        LanguageId::Scala => SCALA.clone(),
        // T0 配置驱动（W4 批）：fortran(fortls uvx)/pascal(pasls download)/haskell
        // (haskell_ls path_only)/ocaml(ocamllsp path_only)/erlang(erlang_ls
        // path_only)/perl(perl_ls path_only)/r(r_ls path_only)/crystal(crystalline
        // path_only)/zig(zls download) 走 servers.toml 条目，ensure_launch 接管。
        // groovy 无条目（上游 H 类自备 ls_jar_path JAR，angular/java 不入表先例），
        // --lang groovy 在 spec_for 落空报 no entry。
        LanguageId::Fortran
        | LanguageId::Pascal
        | LanguageId::Haskell
        | LanguageId::Groovy
        | LanguageId::Ocaml
        | LanguageId::Erlang
        | LanguageId::Perl
        | LanguageId::R
        | LanguageId::Crystal
        | LanguageId::Zig => return None,
        // T0 配置驱动（W5 批）：gleam(`gleam lsp` 子命令)/qml(qmlls 裸启动)/lean
        // (`lean --server`，条目 id lean4)/julia(julia -e runserver，解释器+LanguageServer.jl
        // 包形态，perl/r 同款)/wolfram(WolframKernel LSPServer paclet，LICENSE SKIP
        // 候选仍留条目——持有安装的用户可探测，haskell_ls 语义) 走 servers.toml 条目，
        // ensure_launch 接管。gdscript/msl 无条目（godot=TCP 连已运行编辑器、msl=
        // serena 内嵌 pygls 脚本，均 HOST SKIP 候选，angular/java/groovy 不入表先例），
        // --lang gdscript|msl 在 spec_for 落空报 no entry。
        LanguageId::Gleam
        | LanguageId::Qml
        | LanguageId::Lean
        | LanguageId::Julia
        | LanguageId::Wolfram => {
            return None;
        }
        LanguageId::Godot | LanguageId::Msl => return None,
        // W6 批（上游对拍采纳）：十二门条目已在 servers.toml（clojure/elm/haxe/
        // luau/fsharp/bsl/systemverilog/latex/solidity/ada/al/hlsl），本批仅补
        // LanguageId/扩展名路由——走 ensure_launch 接管（W4 十门同款）。
        LanguageId::Clojure
        | LanguageId::Elm
        | LanguageId::Haxe
        | LanguageId::Luau
        | LanguageId::FSharp
        | LanguageId::Bsl
        | LanguageId::SystemVerilog
        | LanguageId::Latex
        | LanguageId::Solidity
        | LanguageId::Ada
        | LanguageId::Al
        | LanguageId::Hlsl => return None,
    })
}

/// bd serena-rust-c6pb：客户端侧同层复算「该语言 LS 在当前进程环境可否拉起」——
/// 与 supervisor 冷启动同一判定层：T2 adapter 走 `launch_info`（纯路径解析，
/// 不 spawn），否则走 `config::ensure_launch`（PATH / serena 缓存探测，
/// auto_install=false 永不触网）。Ok = 可拉起（daemon 却报 LS_NOT_INSTALLED 即
/// daemon spawn 环境/二进制与当前 shell 错位）；Err = 本机确实没有。
pub async fn probe_launch(lang: &str, project_root: &Path) -> Result<(), String> {
    let ctx = ls_adapters::ProjectCtx {
        project_root: project_root.to_path_buf(),
    };
    if let Some(adapter) = adapter_for(lang) {
        return adapter
            .launch_info(&ctx)
            .await
            .map(|_| ())
            .map_err(|e| format!("{e:#}"));
    }
    config::ensure_launch(lang, None, false, false).map(|_| ())
}

/// 内部语言名 → LSP didOpen 的 languageId。多数语言与内部名恒等；LSP 官方 languageId
/// 与内部 id 不一致时在此登记（`Session::set_language_id` 前的唯一换算点，session_for
/// 统一调用）。bd 56a：内部 "docker" 的 LSP 官方 languageId 是 "dockerfile"——发错值
/// dockerfile-ls 侧语义未定义（三关可能静默降级），故显式映射而非赌 LS 宽容。
pub fn lsp_language_id(lang: &str) -> String {
    match lang {
        "docker" => "dockerfile".to_string(),
        // bd 56a 第二批：LSP 官方 SQL languageId 是 "sql"（VS Code 口径）；pgls/sqls
        // 实测对任意值宽容，但同 docker 原则显式映射而非赌 LS 行为。
        "pgsql" | "mysql" => "sql".to_string(),
        // W2 批：deno lsp 的 didOpen 基础 languageId 用官方 "typescript"（per-file
        // 覆盖在 deno.rs on_session_ready）；sass 的官方口径是 "scss"（.sass 文件由
        // sass.rs per-file 覆盖为 "sass"）。svelte 恒等走 default 臂。
        "deno" => "typescript".to_string(),
        "sass" => "scss".to_string(),
        // W3 批：冒烟门 --lang intelephense 按 entry id 路由（语言 `php` 归
        // phpactor），didOpen 官方口径是 "php"（docker→dockerfile 同款显式映射，
        // 不赌 LS 对自名的宽容）。lua/scala/swift 恒等走 default 臂。
        "intelephense" => "php".to_string(),
        // r8.5 adopt：devsense_php 同 intelephense——entry id 显式路由，didOpen
        // 官方口径 "php"，不赌 LS 对自名 languageId 的宽容（lib.rs:711 内嵌
        // 单测与 servers_toml_coverage doors_wired 双断言锚）。
        "php_devsense" => "php".to_string(),
        // smoke R6：python 变体门 --lang pyright 按 entry id 显式路由（pgsql/mysql
        // 先例），didOpen 官方口径是 "python"——pyright 对自名 languageId 不识别，
        // didOpen 被吞后全部请求零应答（run 36577226543 帧实锚；intelephense→php
        // 同款显式映射，不赌 LS 对自名的宽容）。
        "pyright" => "python".to_string(),
        // smoke R4：typescript_vts 变体门的 didOpen 官方口径是 "typescript"
        // （vtsls 消费 TS 文档；变体门 pgsql→sql 同款换算）。
        "typescript_vts" => "typescript".to_string(),
        // blindtest v5 P2-G 帧实锚（SERENA_RECORD tsrec.jsonl）：--lang
        // typescript_ls 走 T0 spec 路由时 didOpen languageId 发内部变体名
        // "typescript_ls" → TLS 不把文档纳入 TS 诊断管线（零 publishDiagnostics、
        // hover/symbol 靠 URI 兜底仍活）→ 注入错误全漏报。官方口径 "typescript"
        // （pyright→python 同款显式映射，不赌 LS 对自名的宽容）。
        "typescript_ls" => "typescript".to_string(),
        // 上游对拍采纳 W6（批次 A 锚）：ty_server.py:68-70 / pyrefly_server.py:226-228
        // `_get_language_id_for_file` 强制发 "python"——两 LS 不赌自名宽容
        // （pyright→python 同款显式映射）。
        "python_ty" | "python_pyrefly" => "python".to_string(),
        // W3b 批：angular 门主会话是 typescript-language-server（挂 angular plugin），
        // didOpen 官方口径 "typescript"；.html 不经主会话打开（session_for_file
        // 重路由到伴生，伴生自设 languageId）。
        "angular" => "typescript".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    //! 单元测试覆盖"表内 vs 表外"语义；集成行为（Arc 单例、adapter trait）放 `tests/resolve.rs`。
    use super::*;
    use std::path::PathBuf;

    /// smoke R6（run 36577226543）：pyright 变体门 didOpen 官方口径锁——pyright 对
    /// 自名 languageId 不识别（didOpen 被吞 → 12 连发 documentSymbol 零应答），
    /// 显式映射 pyright→python（intelephense→php 先例）；basedpyright 门宽容照过，
    /// 不加死映射。改映射必同步改此断言。
    #[test]
    fn pyright_variant_didopen_language_id() {
        assert_eq!(lsp_language_id("pyright"), "python");
        // python 门本体与其余变体恒等（不加多余映射）；ty/pyrefly 例外——上游两
        // 适配器强制发 "python"（W6 批锚，见 lsp_language_id），原恒等断言随之改。
        assert_eq!(lsp_language_id("python"), "python");
        assert_eq!(lsp_language_id("basedpyright"), "basedpyright");
        assert_eq!(lsp_language_id("python_ty"), "python");
        assert_eq!(lsp_language_id("python_pyrefly"), "python");
    }

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
        // lua 曾锁 None（未路由）；W3 批收编后移步 w3_php_lua_scala_swift_t0_routing
        // 正向锁定，此处不再重复。
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
            // bd serena-rust-9z0x：语言被 external-servers.toml 注册拥有时 T2 让位
            // （adapter_for 返 None → 冷启动回落 ensure_launch 走注册条目）——本机
            // 注册表存在该语言的 external 条目属预期形态，跳过而非误报。
            if config::spec_source(lang) == Some("external") {
                continue;
            }
            let a = adapter_for(lang).unwrap_or_else(|| panic!("missing adapter for {lang}"));
            let b = adapter_for(lang).unwrap();
            assert!(Arc::ptr_eq(&a, &b), "singleton broken for {lang}");
            let _ = a.id();
        }
        assert!(adapter_for("lua").is_none());
    }

    /// Wave 1/2：--lang bash/json/powershell/vue/astro 必须路由到手写 T2 adapter（supervisor
    /// session_for 的 T2 优先分支）。
    #[test]
    fn adapter_for_routes_wave1_languages() {
        for lang in ["bash", "json", "powershell", "vue", "astro"] {
            let a =
                adapter_for(lang).unwrap_or_else(|| panic!("--lang {lang} 必须路由到 T2 adapter"));
            let b = adapter_for(lang).unwrap();
            assert!(Arc::ptr_eq(&a, &b), "singleton broken for {lang}");
        }
        // T2 侧 languages 声明与 lang 字符串闭环（路由一致性）。
        assert_eq!(
            adapter_for("bash").unwrap().languages(),
            &[LanguageId::Bash]
        );
        assert_eq!(
            adapter_for("json").unwrap().languages(),
            &[LanguageId::Json]
        );
        assert_eq!(
            adapter_for("powershell").unwrap().languages(),
            &[LanguageId::PowerShell]
        );
        assert_eq!(adapter_for("vue").unwrap().languages(), &[LanguageId::Vue]);
        assert_eq!(
            adapter_for("astro").unwrap().languages(),
            &[LanguageId::Astro]
        );
        // alias：from_str_opt 接受 pwsh（LanguageId 层），adapter_for 同语义。
        assert!(adapter_for("pwsh").is_some());
    }

    /// smoke R4：typescript_vts 变体门路由闭环——adapter_for 字符串直路由（不经
    /// LanguageId），languages 归 TypeScript，lsp_language_id 官方口径换算。
    #[test]
    fn adapter_for_routes_typescript_vts() {
        let a = adapter_for("typescript_vts")
            .unwrap_or_else(|| panic!("--lang typescript_vts 必须路由到 T2 adapter"));
        let b = adapter_for("typescript_vts").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "singleton broken for typescript_vts");
        assert_eq!(a.id(), "vtsls");
        assert_eq!(a.languages(), &[LanguageId::TypeScript]);
        // didOpen 官方口径：变体名 → "typescript"（pgsql→sql 同款换算）。
        assert_eq!(lsp_language_id("typescript_vts"), "typescript");
        // spec_for 仍按 entry id 命中（install CLI 走 T0 表，adapter 只接管会话）。
        assert!(config::spec_for("typescript_vts").is_some());
    }

    /// bd 56a 后续批：--lang html/css 必须路由到手写 T2 adapter（同包双入口），
    /// 扩展名 html/htm/css 走 EXT_TABLE，lsp_language_id 恒等。
    #[test]
    fn adapter_for_routes_html_css() {
        for lang in ["html", "css"] {
            let a =
                adapter_for(lang).unwrap_or_else(|| panic!("--lang {lang} 必须路由到 T2 adapter"));
            let b = adapter_for(lang).unwrap();
            assert!(Arc::ptr_eq(&a, &b), "singleton broken for {lang}");
        }
        assert_eq!(
            adapter_for("html").unwrap().languages(),
            &[LanguageId::Html]
        );
        assert_eq!(adapter_for("css").unwrap().languages(), &[LanguageId::Css]);
        // lsp_language_id 恒等（LSP 官方口径同内部名）。
        assert_eq!(lsp_language_id("html"), "html");
        assert_eq!(lsp_language_id("css"), "css");
        // 扩展名解析（EXT_TABLE）：html/htm 归 Html 门，css 归 Css 门。
        assert_eq!(
            resolve(&PathBuf::from("index.html")),
            Some(LanguageId::Html)
        );
        assert_eq!(resolve(&PathBuf::from("page.htm")), Some(LanguageId::Html));
        assert_eq!(resolve(&PathBuf::from("style.css")), Some(LanguageId::Css));
    }

    /// W2 批：--lang svelte/deno/sass 必须路由到手写 T2 adapter；deno 显式路由门
    /// （TS 家族扩展名不进 EXT_TABLE）；lsp_language_id 官方口径换算。
    #[test]
    fn adapter_for_routes_w2_languages() {
        for lang in ["svelte", "deno", "sass"] {
            let a =
                adapter_for(lang).unwrap_or_else(|| panic!("--lang {lang} 必须路由到 T2 adapter"));
            let b = adapter_for(lang).unwrap();
            assert!(Arc::ptr_eq(&a, &b), "singleton broken for {lang}");
        }
        assert_eq!(
            adapter_for("svelte").unwrap().languages(),
            &[LanguageId::Svelte]
        );
        assert_eq!(
            adapter_for("deno").unwrap().languages(),
            &[LanguageId::Deno]
        );
        assert_eq!(
            adapter_for("sass").unwrap().languages(),
            &[LanguageId::Sass]
        );
        // lsp_language_id 官方口径：deno→"typescript"、sass→"scss"、svelte 恒等。
        assert_eq!(lsp_language_id("deno"), "typescript");
        assert_eq!(lsp_language_id("sass"), "scss");
        assert_eq!(lsp_language_id("svelte"), "svelte");
        // 扩展名解析：.svelte/.sass/.scss；deno 无扩展名路由（显式路由门）。
        assert_eq!(
            resolve(&PathBuf::from("app.svelte")),
            Some(LanguageId::Svelte)
        );
        assert_eq!(
            resolve(&PathBuf::from("style.sass")),
            Some(LanguageId::Sass)
        );
        assert_eq!(
            resolve(&PathBuf::from("style.scss")),
            Some(LanguageId::Sass)
        );
        for ext in ["ts", "tsx", "js", "jsx", "mjs", "cjs"] {
            assert_ne!(
                resolve(&PathBuf::from(format!("main.{ext}"))),
                Some(LanguageId::Deno),
                "{ext}: deno 不抢 TS 家族扩展名"
            );
        }
    }

    /// Wave 1 扩展名解析：sh/bash/json/jsonc/ps1/psm1/psd1/vue（resolve 锁定）。
    #[test]
    fn resolve_wave1_extensions() {
        assert_eq!(resolve(&PathBuf::from("a.sh")), Some(LanguageId::Bash));
        assert_eq!(resolve(&PathBuf::from("a.bash")), Some(LanguageId::Bash));
        assert_eq!(resolve(&PathBuf::from("pkg.json")), Some(LanguageId::Json));
        assert_eq!(
            resolve(&PathBuf::from("tsconfig.jsonc")),
            Some(LanguageId::Json)
        );
        assert_eq!(
            resolve(&PathBuf::from("x.ps1")),
            Some(LanguageId::PowerShell)
        );
        assert_eq!(
            resolve(&PathBuf::from("x.psm1")),
            Some(LanguageId::PowerShell)
        );
        assert_eq!(
            resolve(&PathBuf::from("x.psd1")),
            Some(LanguageId::PowerShell)
        );
        assert_eq!(resolve(&PathBuf::from("x.astro")), Some(LanguageId::Astro));
        assert_eq!(resolve(&PathBuf::from("App.vue")), Some(LanguageId::Vue));
    }

    /// bd 56a 第一批：docker/sql 走 T0（无手写 adapter），servers.toml 语言路由 +
    /// LSP didOpen languageId 换算（docker → 官方 "dockerfile"）。
    #[test]
    fn batch56a_docker_sql_t0_routing_and_lsp_language_id() {
        // T0：无手写 adapter（session_for 落 config::ensure_launch）。
        assert!(adapter_for("docker").is_none());
        assert!(adapter_for("sql").is_none());
        // servers.toml 语言路由命中（spec_for 按 languages 扫描）。
        assert!(config::spec_for("docker").is_some());
        assert!(config::spec_for("sql").is_some());
        // LSP 官方 languageId 换算：docker→dockerfile；sql 及既有语言恒等。
        assert_eq!(lsp_language_id("docker"), "dockerfile");
        assert_eq!(lsp_language_id("sql"), "sql");
        assert_eq!(lsp_language_id("rust"), "rust");
        // 扩展名解析（EXT_TABLE）。
        assert_eq!(resolve(&PathBuf::from("schema.sql")), Some(LanguageId::Sql));
        assert_eq!(
            resolve(&PathBuf::from("app.dockerfile")),
            Some(LanguageId::Docker)
        );
    }

    /// W3 批：php/lua/swift 全走 T0（无手写 adapter，session_for 落
    /// ensure_launch）；php 门按 entry id `intelephense` 显式路由（语言 `php` 归
    /// phpactor 条目，phpantom 避撞先例），didOpen 官方口径换算 intelephense→php；
    /// lua/swift 恒等（不加死映射）。scala 原本同批 T0，W3 采纳（bd 69e batchA
    /// 缺失[高]）已升 T2（build-root 探测/metals 编排）。
    #[test]
    fn w3_php_lua_scala_swift_t0_routing() {
        // T0：语言名 + intelephense 别名无手写 adapter（scala 除外，已 T2）。
        // r8.5：php_devsense 也走 T0（按 entry id 显式路由，无手写 adapter）。
        for lang in ["php", "intelephense", "lua", "swift", "php_devsense"] {
            assert!(adapter_for(lang).is_none(), "{lang}: W3 三门全 T0");
        }
        assert!(
            adapter_for("scala").is_some(),
            "scala 已 T2 接管（bd 69e W3 采纳）"
        );
        // servers.toml 语言路由 / entry id 路由命中。
        assert!(config::spec_for("lua").is_some());
        assert!(config::spec_for("scala").is_some());
        assert!(config::spec_for("swift").is_some());
        assert!(config::spec_for("intelephense").is_some());
        assert!(
            config::spec_for("devsense_php").is_some(),
            "r8.5 adopt: devsense_php 条目收录"
        );
        // LSP didOpen languageId：仅 intelephense→php 显式映射，其余恒等。
        // r8.5：devsense_php 也走"→php"映射（同 intelephense 理由）。
        assert_eq!(lsp_language_id("intelephense"), "php");
        assert_eq!(lsp_language_id("php_devsense"), "php");
        assert_eq!(lsp_language_id("php"), "php");
        assert_eq!(lsp_language_id("lua"), "lua");
        assert_eq!(lsp_language_id("scala"), "scala");
        assert_eq!(lsp_language_id("swift"), "swift");
        // 扩展名解析（EXT_TABLE）：.php/.lua/.scala/.swift 各归其门；luau 是独立
        // 语言门，"luau" 不别名到 Lua。
        assert_eq!(resolve(&PathBuf::from("main.php")), Some(LanguageId::Php));
        assert_eq!(resolve(&PathBuf::from("main.lua")), Some(LanguageId::Lua));
        assert_eq!(
            resolve(&PathBuf::from("Main.scala")),
            Some(LanguageId::Scala)
        );
        assert_eq!(
            resolve(&PathBuf::from("Main.swift")),
            Some(LanguageId::Swift)
        );
        // W6 批：Luau 独立语言门收编（原 None 样本锁随变体加入作废——"不别名到
        // Lua" 语义升级为 Some(Luau)）。
        assert_eq!(LanguageId::from_str_opt("luau"), Some(LanguageId::Luau));
        assert_eq!(
            LanguageId::from_str_opt("intelephense"),
            Some(LanguageId::Php)
        );
        assert_eq!(
            LanguageId::from_str_opt("php_devsense"),
            Some(LanguageId::PhpDevsense)
        );
    }

    /// W5 七门：真门 gleam/qml/lean/julia + wolfram 有条目（LICENSE SKIP 候选），
    /// gdscript/msl 无条目（HOST SKIP 候选）；lean4 是条目 id 非别名。
    #[test]
    fn w5_seven_doors_t0_routing() {
        // 七门全无手写 adapter（T0 / SKIP）。
        for lang in [
            "gleam", "qml", "lean", "julia", "wolfram", "gdscript", "msl",
        ] {
            assert!(adapter_for(lang).is_none(), "{lang}: W5 七门全 T0/SKIP");
        }
        // servers.toml 路由：语言名 + entry id 双语义（lean4 = 条目 id，zls/zig 先例）。
        for lang in ["gleam", "qml", "lean", "lean4", "julia", "wolfram"] {
            assert!(
                config::spec_for(lang).is_some(),
                "--lang {lang} 必须命中 servers.toml 条目"
            );
        }
        assert!(
            config::spec_for("gdscript").is_none(),
            "godot HOST SKIP 候选无条目"
        );
        assert!(
            config::spec_for("msl").is_none(),
            "msl HOST SKIP 候选无条目"
        );
        // LSP didOpen languageId：七门恒等（lean = 官方口径；lean4 仅条目 id，
        // 不经 LanguageId/lsp_language_id 换算面）。
        for lang in [
            "gleam", "qml", "lean", "julia", "wolfram", "gdscript", "msl",
        ] {
            assert_eq!(lsp_language_id(lang), lang);
        }
        // 扩展名解析（EXT_TABLE）：七门各归其门。
        assert_eq!(
            resolve(&PathBuf::from("main.gleam")),
            Some(LanguageId::Gleam)
        );
        assert_eq!(resolve(&PathBuf::from("Main.qml")), Some(LanguageId::Qml));
        assert_eq!(resolve(&PathBuf::from("Main.lean")), Some(LanguageId::Lean));
        assert_eq!(resolve(&PathBuf::from("main.jl")), Some(LanguageId::Julia));
        assert_eq!(
            resolve(&PathBuf::from("main.wl")),
            Some(LanguageId::Wolfram)
        );
        assert_eq!(
            resolve(&PathBuf::from("main.nb")),
            Some(LanguageId::Wolfram)
        );
        assert_eq!(resolve(&PathBuf::from("main.gd")), Some(LanguageId::Godot));
        assert_eq!(resolve(&PathBuf::from("main.mrc")), Some(LanguageId::Msl));
        assert_eq!(LanguageId::from_str_opt("lean4"), None);
    }

    /// W6 十二门（上游对拍采纳）：条目已在 servers.toml，本批补 LanguageId/EXT_TABLE
    /// 路由——全 T0（无手写 adapter）、--lang 语言名命中条目、扩展名各归其门、
    /// didOpen languageId 恒等。.m 刻意不收（MATLAB 歧义已裁决）。
    #[test]
    fn w6_twelve_doors_t0_routing() {
        // 全部无手写 adapter（T0，走 ensure_launch）。
        for lang in [
            "clojure",
            "elm",
            "haxe",
            "luau",
            "fsharp",
            "bsl",
            "systemverilog",
            "latex",
            "solidity",
            "ada",
            "al",
            "hlsl",
        ] {
            assert!(adapter_for(lang).is_none(), "{lang}: W6 十二门全 T0");
            assert!(
                config::spec_for(lang).is_some(),
                "--lang {lang} 必须命中 servers.toml 条目"
            );
            assert_eq!(lsp_language_id(lang), lang, "{lang}: didOpen 恒等");
        }
        // 扩展名解析：代表样本覆盖各族（全清单在 EXT_TABLE 注释 + from_extension）。
        for (path, want) in [
            ("Main.clj", LanguageId::Clojure),
            ("Main.cljs", LanguageId::Clojure),
            ("Main.cljc", LanguageId::Clojure),
            ("Main.edn", LanguageId::Clojure),
            ("Main.elm", LanguageId::Elm),
            ("Main.hx", LanguageId::Haxe),
            ("Main.luau", LanguageId::Luau),
            ("Main.fs", LanguageId::FSharp),
            ("Main.fsi", LanguageId::FSharp),
            ("Main.fsx", LanguageId::FSharp),
            ("Main.bsl", LanguageId::Bsl),
            ("Main.os", LanguageId::Bsl),
            ("Main.sv", LanguageId::SystemVerilog),
            ("Main.svh", LanguageId::SystemVerilog),
            ("Main.v", LanguageId::SystemVerilog),
            ("Main.vh", LanguageId::SystemVerilog),
            ("Main.tex", LanguageId::Latex),
            ("Main.bib", LanguageId::Latex),
            ("Main.sol", LanguageId::Solidity),
            ("Main.ada", LanguageId::Ada),
            ("Main.adb", LanguageId::Ada),
            ("Main.ads", LanguageId::Ada),
            ("Main.al", LanguageId::Al),
            ("Main.hlsl", LanguageId::Hlsl),
            ("Main.fx", LanguageId::Hlsl),
            ("Main.cginc", LanguageId::Hlsl),
            ("Main.comp", LanguageId::Hlsl),
            ("Main.wgsl", LanguageId::Hlsl),
        ] {
            assert_eq!(resolve(&PathBuf::from(path)), Some(want), "{path}");
        }
    }

    /// blindtest v5 P1-1：routable 判据与安装态正交——installed 只代表缓存包在盘。
    /// 可路由 = EXT_TABLE 扩展名路由（含 T0）或 T2 adapter；.rb/.m 无映射即 false
    /// （对应 v5「包在盘但首答 unsupported extension」的 13/40 谎报面）。
    #[test]
    fn language_routable_tracks_request_paths_not_install_state() {
        // T2 adapter 门。
        for lang in ["rust", "csharp", "python", "vue"] {
            assert!(language_routable(lang), "{lang}: T2 门可路由");
        }
        // T0 但 EXT_TABLE 有扩展名路由。
        for lang in ["kotlin", "ada", "markdown", "sql"] {
            assert!(language_routable(lang), "{lang}: T0 扩展名路由可达");
        }
        // 无扩展名映射、无 adapter：包在盘也路由失败（v5 实锤类）。
        for lang in ["ruby", "matlab", "solargraph", "unknown_lang_xx"] {
            assert!(!language_routable(lang), "{lang}: 必须判不可路由");
        }
        // 大小写归一。
        assert!(language_routable("Rust"));
    }

    /// blindtest v5 P1-1：条目级判据——T2 install-face 条目（languages 占位避撞）
    /// 按家族语言折算；家族语言本身不可路由（ruby 系）则仍如实 false。
    #[test]
    fn entry_routable_folds_variant_entries_to_family_language() {
        let langs = |s: &str| vec![s.to_string()];
        // T2 install-face / 避撞变体：真语言路由可达。
        for id in ["pyright", "jedi", "basedpyright", "python_ty"] {
            assert!(entry_routable(id, &langs(id)), "{id}: python 家族可路由");
        }
        assert!(entry_routable("csharp_ls", &langs("csharp_ls")));
        assert!(entry_routable("omnisharp", &langs("omnisharp")));
        assert!(entry_routable("typescript_ls", &langs("typescript_ls")));
        assert!(entry_routable("crystalline_source", &langs("crystalline_source")));
        // 语言门直通的条目（languages 即真语言）。
        assert!(entry_routable("kotlin", &langs("kotlin")));
        assert!(entry_routable("ada", &langs("ada")));
        // ruby 家族不可路由（.rb 无 EXT_TABLE 映射）：包在盘也如实 false。
        for id in ["solargraph", "ruby_lsp"] {
            assert!(!entry_routable(id, &langs(id)), "{id}: ruby 家族不可路由");
        }
        assert!(!entry_routable("matlab", &langs("matlab")));
        // 未知条目。
        assert!(!entry_routable("zz_entry_xx", &langs("zz_entry_xx")));
    }

    /// blindtest v5 P1-1：resolve 失败分支的文案分流判据——.rb/.m 在内置表有登记
    /// （extensions 字段），报「注册了但无 adapter」；真未知扩展名返 None。
    #[test]
    fn registered_lang_for_extension_splits_error_copy() {
        assert_eq!(registered_lang_for_extension("rb"), Some("ruby"));
        assert_eq!(registered_lang_for_extension(".rb"), Some("ruby"));
        assert_eq!(registered_lang_for_extension("m"), Some("matlab"));
        assert_eq!(registered_lang_for_extension("zzunsup99"), None);
    }
}
