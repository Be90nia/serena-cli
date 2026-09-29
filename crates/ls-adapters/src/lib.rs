//! ls-adapters —— LSP 语言服务器适配器抽象与 T2 手写模块。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/*.py`（逐 quirk 抄译）。
//!
//! ## trait `LanguageServerAdapter`（ARCHITECTURE §4.1 定稿）
//!
//! 9 个方法：id / languages / launch_info(async) / initialize_patches /
//! set_project_root / on_server_ready / wait_for_index / request_hooks /
//! supports_implementation。
//! `launch_info` 改 async 是 ARCH 相对 DESIGN §4 草稿的修订（A2）—— 慢 IO 探测
//! 不再强迫调用方起 `spawn_blocking`。
//!
//! ## 分层
//!
//! - `ls-runtime` 定义 `LaunchInfo` / `TransportKind`（ARCH §4.1 字段），由本 crate
//!   产出、被 `lsp-core::transport::stdio::pump` 消费 —— 一进一出闭环。
//! - `ls-core` 不 import 本 crate（ARCH 分层铁律），但本 crate 通过 `Session` 类型
//!   作为 trait 方法参数出现 —— 这是 lsp-types 之外的唯一耦合。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use lsp_types::InitializeParams;

pub mod astro;
pub mod basedpyright_server;
pub mod bash;
pub mod clangd;
pub mod csharp_ls;
pub mod css;
pub mod deno;
pub mod gopls;
pub mod html;
pub mod jdtls;
pub mod jedi_server;
pub mod json;
pub mod powershell;
pub mod pyre_server;
pub mod pyright;
pub mod rust_analyzer;
pub mod sass;
pub mod svelte;
pub mod ty_server;
pub mod typescript;
pub mod vue;

/// 语言标识：与 `servers.toml` `languages` 字段、claude 端 ProjectCtx.language 一一对应。
///
/// M0 只落地 `Cpp`（clangd）；其它值（Python/Rust/Go/...）随 ls-registry Task 9 + M2/M3
/// `servers.toml` 扩展。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageId {
    Cpp,
    Rust,
    Python,
    Go,
    TypeScript,
    CSharp,
    Java,
    /// T0 配置驱动（servers.toml，Task 19）：无手写 T2 适配器的语言从 here 起。
    Markdown,
    /// T0 配置驱动（bd 56a 第一批，Δ 自有设计：上游 serena 无 docker/sql 注册）。
    Docker,
    Sql,
    Bash,
    Json,
    PowerShell,
    Vue,
    /// T2 手写双服务器适配器（astro.rs，上游 7a296833）。
    Astro,
    /// T2 手写适配器（bd 56a 后续批：html.rs 镜像上游 7a296833；css.rs Δ 自有设计，
    /// 同包 vscode-langservers-extracted 双入口）。
    Html,
    Css,
    /// T0 配置驱动（bd 56a 第二批，Δ 上游无此语言）：.sql 扩展名归 sql 门，
    /// 本两门经 `--lang pgsql|mysql` 显式路由到 servers.toml 条目，无扩展名映射。
    Pgsql,
    Mysql,
    /// T0 配置驱动（servers.toml yaml 条目，bd 56a 后续批次接线）：npm
    /// yaml-language-server，扩展名 .yaml/.yml。
    Yaml,
    /// T0 配置驱动（servers.toml kotlin 条目，bd 56a 后续批）：JetBrains managed
    /// Kotlin LSP（下载锚 7a296833，钉 263.4702.0），扩展名 .kt/.kts。
    Kotlin,
    /// T0 配置驱动（servers.toml dart 条目，bd 56a 后续批）：Dart SDK 内置
    /// analysis server（`dart language-server`），扩展名 .dart。
    Dart,
    /// T0 配置驱动（W1b 批，servers.toml ansible/regal/nextflow 条目）。
    /// ↖ mirror: oraios/serena@7a296833 ansible_language_server.py（npm
    /// @ansible/ansible-language-server `--stdio`；无 documentSymbol，
    /// vscode-ansible#601 NOT_PLANNED）。
    Ansible,
    /// T0 配置驱动（W1b 批）：↖ mirror: regal_server.py@7a296833（单文件二进制
    /// `regal language-server`）。
    Rego,
    /// T0 配置驱动（W1b 批）：↖ mirror: nextflow_language_server.py@7a296833
    /// （fat JAR `java -jar`，JDK ≥17；npm 无此包）。
    Nextflow,
    /// T0 配置驱动（W1a 批，servers.toml toml 条目 = taplo）。
    /// ↖ mirror: taplo_server.py@43ae0211（GitHub release 单文件 gz/zip，0.10.0
    /// sha 内嵌；`taplo lsp stdio`）。
    Toml,
    /// T0 配置驱动（W1a 批，servers.toml terraform 条目 = terraform-ls）。
    /// ↖ mirror: terraform_ls.py@43ae0211（hashicorp release zip 0.36.5 sha 内嵌；
    /// `terraform-ls serve`——无 serve 子命令只打印帮助即退出）。
    Terraform,
    /// T0 配置驱动（W1a 批，servers.toml cue 条目 = cue CLI 内置 LSP，Δ 收录：
    /// 上游无独立 cue 适配器，v0.16.1 cmd/cue/cmd/lsp.go 实锚 `cue lsp`）。
    Cue,
    /// T0 配置驱动（W1a 批，servers.toml nixd 条目 = source 构建形态）。
    /// ↖ mirror: nixd_ls.py@43ae0211（上游同样要求 Nix 工具链；release 无预编译
    /// 资产，CI 冒烟 HOST skip）。
    Nix,
    /// T2 手写 hybrid 双服务器适配器（W2 批，svelte.rs，上游 7a296833）：主
    /// svelteserver + 伴生 typescript-language-server 挂 typescript-svelte-plugin，
    /// 扩展名 .svelte。
    Svelte,
    /// T2 手写适配器（W2 批，deno.rs，上游 7a296833）：`deno lsp` 子命令入口。
    /// TS 家族扩展名不抢——仅 `--lang deno` 显式路由可达（pgsql/mysql 先例），
    /// probe_extensions 空表，by_shebang 的 deno 解释器仍归 TypeScript。
    Deno,
    /// T2 手写适配器（W2 批，sass.rs，上游 7a296833）：some-sass-language-server
    ///（npm），扩展名 .sass/.scss（.css 归 css 门；servers.toml 条目 id 仍 scss，
    /// 路由语言名 = sass，didOpen 官方 languageId = "scss"）。
    Sass,
    /// T0 配置驱动（W3 批，servers.toml phpactor/intelephense 条目）：语言路由
    /// `php` 归 phpactor（download PHAR，存量避撞先例同 phpantom）；PM 拍板冒烟门
    /// 走 intelephense（npm，`--lang intelephense` 按 entry id 显式路由，phpantom
    /// 同款）。↖ mirror: intelephense.py@43ae021。
    Php,
    /// T0 配置驱动（W3 批，servers.toml lua 条目 = LuaLS lua-language-server）。
    /// ↖ mirror: lua_ls.py@43ae021（GitHub release tar.gz 3.15.0 sha 内嵌）。
    Lua,
    /// T0 配置驱动（W3 批，servers.toml scala 条目 = metals，path_only 形态）。
    /// ↖ mirror: scala_language_server.py@43ae021（上游 PATH 有 metals 则用之，
    /// 否则 coursier bootstrap org.scalameta:metals_2.13；上游另应答 import 构建
    /// 提示——我们 T0 不应答，无 build 工程的 fixture 走 metals standalone PC）。
    Scala,
    /// T0 配置驱动（W3 批，servers.toml sourcekit_lsp 条目，path_only）：macOS
    /// Xcode/Swift 工具链自带，ubuntu runner 无工具链（冒烟门 PLATFORM SKIP）。
    /// ↖ mirror: sourcekit_ls.py@43ae021。
    Swift,
    /// T0 配置驱动（W4 批，servers.toml fortls 条目 = uvx fortls 3.2.2）。
    /// ↖ mirror: fortran_language_server.py@7a296833。矩阵门上轮已 PASS（--lang
    /// fortran 走 spec_for 字符串路径可达），本批补 LanguageId/EXT_TABLE 接线闭合
    /// （每门交付模板；扩展名大小写不敏感族 .f90/.F90 同命中——本表键统一小写）。
    Fortran,
    /// T0 配置驱动（W4 批，servers.toml pascal 条目 = pasls download v0.2.0）。
    /// ↖ mirror: pascal_server.py@7a296833（FPC runtime 完整功能需 PP/FPCDIR；
    /// 矩阵门为存量 BUDGET skip——apt fpc ~400MB 超单门预算，PM 裁决在案）。
    Pascal,
    /// T0 配置驱动（W4 批，servers.toml haskell_ls 条目 = wrapper `--lsp`，本批
    /// exec 修复：裸 wrapper 打印 usage 即退）。
    /// ↖ mirror: haskell_language_server.py@7a296833（PATH 探测 wrapper；bare 文件
    /// 走 default cradle 调 ghc）。
    Haskell,
    /// W4 批：上游 H 类（groovy_language_server.py@7a296833 要求用户自备
    /// ls_jar_path JAR；npm 无 LS 包、GroovyLanguageServer GitHub releases=[]、
    /// apt 无 LS——无 T0 对应物，angular/java 不入表先例，矩阵门 HOST SKIP 候选
    /// 待 PM 裁决）。仅 LanguageId/扩展名占位，--lang groovy 查表落空报 no entry。
    Groovy,
    /// T0 配置驱动（W4 批，servers.toml ocamllsp 条目 = PATH 探测裸启动）。
    /// ↖ mirror: ocaml_lsp_server.py@7a296833（`opam exec -- which ocamllsp` 取
    /// 路径后直启 = path_only 语义；安装 = opam install ocaml-lsp-server；
    /// OCaml 5.1.0 不兼容为上游明示）。
    Ocaml,
    /// T0 配置驱动（W4 批，servers.toml erlang_ls 条目 = `--transport stdio`，
    /// 本批 exec 修复：默认 transport 是 TCP）。
    /// ↖ mirror: erlang_language_server.py@7a296833；运行另需 Erlang/OTP runtime。
    Erlang,
    /// T0 配置驱动（W4 批，servers.toml perl_ls 条目 = `perl -MPerl::LanguageServer
    /// -e Perl::LanguageServer::run`，launch argv 逐字）。
    /// ↖ mirror: perl_language_server.py@7a296833（上游另应答
    /// workspace/configuration——T0 走 lsp-core 未注册请求默认 null 成功应答）。
    Perl,
    /// T0 配置驱动（W4 批，servers.toml r_ls 条目 = `R --vanilla --quiet --slave
    /// -e ...languageserver::run()`，launch argv 逐字）。
    /// ↖ mirror: r_language_server.py@7a296833（CRAN languageserver 包 + R runtime，
    /// 均不托管安装）。
    R,
    /// T0 配置驱动（W4 批，servers.toml crystalline 条目 = PATH 探测裸启动）。
    /// ↖ mirror: crystal_language_server.py@7a296833（shutil.which 裸启动；
    /// documentSymbol 上游注释 "work reliably"；definition 每会话仅首个命中为上游
    /// 已知缺陷，与本接线无关）。
    Crystal,
    /// T0 配置驱动（W4 批，servers.toml zls 条目 = 本批 path_only → download 升级
    /// v0.16.0 六平台）。
    /// ↖ mirror: zls.py@7a296833（裸 `zls`；上游 init options 注入 zig_exe_path，
    /// 无值时 zls 自检 PATH 上的 zig——与 PATH 探测形态等价；zig 同 minor 配对）。
    Zig,
    /// T0 配置驱动（W5 批，servers.toml gleam 条目 = `gleam lsp` 子命令，deno/cue
    /// 同款 CLI 内置 LS）。↖ mirror: gleam_language_server.py@7a296833（PATH 探测
    /// gleam 编译器本体；无 init options；上游另等首批 $/progress 依赖解析——T0 无
    /// 此等待门，LS 依赖下载窗口由工具层超时承担）。
    Gleam,
    /// T0 配置驱动（W5 批，servers.toml qmlls 条目 = Qt 6 官方 qmlls 裸启动）。
    /// ↖ mirror: qml_language_server.py@7a296833（上游 which 顺序 qmlls6 → qmlls；
    /// 我们条目单名 binary_name=qmlls——apt 装机 /usr/bin/qmlls6，smoke 门 ln -sf
    /// 对齐，Debian install 清单实锚）。
    Qml,
    /// T0 配置驱动（W5 批，servers.toml lean4 条目 = `lean --server`，lean 是
    /// LSP didOpen 官方 languageId，lean4 是条目 id——zls/zig 双名先例）。
    /// ↖ mirror: lean4_language_server.py@7a296833（Δ 未抄上游 lake env 注入
    /// LEAN_PATH/LEAN_SRC_PATH——跨文件语义需 lake 工程，standalone fixture 走
    /// 基础符号）。
    Lean,
    /// T0 配置驱动（W5 批，servers.toml julia 条目 = `julia -e 'using LanguageServer;
    /// runserver()'`）。↖ mirror: julia_server.py@7a296833（Δ 上游尾参 repo_root
    /// 省略——runserver choose_env 回落链含 pwd 上溯，T0 spawn cwd = 项目根实锚；
    /// Δ workspace/configuration 应答与 didChangeConfiguration 补发未抄——T0 走
    /// lsp-core 未注册请求默认 null 成功应答，lint 设置回落 LS 默认）。
    Julia,
    /// LICENSE SKIP 候选（W5 批，待 PM 裁决）：LS = WolframKernel 捆绑的 LSPServer
    /// paclet（Mathematica 13.0+ / Wolfram Engine 12.1+，无独立安装渠道），条目为
    /// 持有 Wolfram 安装的用户保留探测面。↖ mirror: wolfram_language_server.py@7a296833。
    Wolfram,
    /// HOST SKIP 候选（W5 批，待 PM 裁决）：上游 adapter 是 TCP 客户端（连已运行的
    /// Godot 编辑器 :6008，从不启动进程），我们 TransportKind 仅 Stdio——无 T0 形态，
    /// servers.toml 不建条目（angular/java/groovy 不入表先例），扩展名占位使 resolve
    /// 落到明确 no-entry 报错。↖ mirror: godot_language_server.py@7a296833。
    Godot,
    /// HOST SKIP 候选（W5 批，待 PM 裁决）：上游 LS = serena 仓库内嵌 pygls 脚本
    ///（launch = [sys.executable, msl_lsp_server.py]，mIRC 脚本语言 .mrc——非 Metal），
    /// 非独立发行、我们 Rust 端不随包脚本，无 T0 对应物，不建条目。↖ mirror:
    /// msl_language_server.py@7a296833。
    Msl,
}

impl LanguageId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cpp => "cpp",
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Go => "go",
            Self::TypeScript => "typescript",
            Self::CSharp => "csharp",
            Self::Java => "java",
            Self::Markdown => "markdown",
            Self::Docker => "docker",
            Self::Sql => "sql",
            Self::Bash => "bash",
            Self::Json => "json",
            Self::PowerShell => "powershell",
            Self::Vue => "vue",
            Self::Astro => "astro",
            Self::Html => "html",
            Self::Css => "css",
            Self::Pgsql => "pgsql",
            Self::Mysql => "mysql",
            Self::Yaml => "yaml",
            Self::Kotlin => "kotlin",
            Self::Dart => "dart",
            Self::Ansible => "ansible",
            Self::Rego => "rego",
            Self::Nextflow => "nextflow",
            Self::Toml => "toml",
            Self::Terraform => "terraform",
            Self::Cue => "cue",
            Self::Nix => "nix",
            Self::Svelte => "svelte",
            Self::Deno => "deno",
            Self::Sass => "sass",
            Self::Php => "php",
            Self::Lua => "lua",
            Self::Scala => "scala",
            Self::Swift => "swift",
            // W4 批：十门恒等（官方 LSP languageId 与内部名一致，覆盖测试锁死）。
            Self::Fortran => "fortran",
            Self::Pascal => "pascal",
            Self::Haskell => "haskell",
            Self::Groovy => "groovy",
            Self::Ocaml => "ocaml",
            Self::Erlang => "erlang",
            Self::Perl => "perl",
            Self::R => "r",
            Self::Crystal => "crystal",
            Self::Zig => "zig",
            // W5 批：七门恒等（lean = 官方 languageId，lean4 留作条目 id；gdscript =
            // 上游语言名——godot 编辑器宿主门）。
            Self::Gleam => "gleam",
            Self::Qml => "qml",
            Self::Lean => "lean",
            Self::Julia => "julia",
            Self::Wolfram => "wolfram",
            Self::Godot => "gdscript",
            Self::Msl => "msl",
        }
    }
    /// 反向：lang 字符串 → LanguageId。未知返 None。
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "cpp" => Some(Self::Cpp),
            "rust" => Some(Self::Rust),
            "python" => Some(Self::Python),
            "go" => Some(Self::Go),
            "typescript" | "javascript" => Some(Self::TypeScript),
            "csharp" => Some(Self::CSharp),
            "java" => Some(Self::Java),
            "markdown" => Some(Self::Markdown),
            "docker" => Some(Self::Docker),
            "sql" => Some(Self::Sql),
            "bash" => Some(Self::Bash),
            "json" => Some(Self::Json),
            "powershell" | "pwsh" => Some(Self::PowerShell),
            "vue" => Some(Self::Vue),
            "astro" => Some(Self::Astro),
            "html" => Some(Self::Html),
            "css" => Some(Self::Css),
            "pgsql" | "postgres" => Some(Self::Pgsql),
            "mysql" => Some(Self::Mysql),
            "yaml" => Some(Self::Yaml),
            "kotlin" => Some(Self::Kotlin),
            "dart" => Some(Self::Dart),
            "ansible" => Some(Self::Ansible),
            "rego" => Some(Self::Rego),
            "nextflow" => Some(Self::Nextflow),
            "toml" => Some(Self::Toml),
            "terraform" => Some(Self::Terraform),
            "cue" => Some(Self::Cue),
            "nix" => Some(Self::Nix),
            "svelte" => Some(Self::Svelte),
            "deno" => Some(Self::Deno),
            "sass" => Some(Self::Sass),
            // W3 批：intelephense 别名（phpantom 同款按 entry id 显式路由——
            // 语言路由 `php` 归 phpactor 条目，冒烟门 --lang intelephense）。
            "php" | "intelephense" => Some(Self::Php),
            "lua" => Some(Self::Lua),
            "scala" => Some(Self::Scala),
            "swift" => Some(Self::Swift),
            // W4 批：十门恒等（上游 adapter 第四参 = 内部名，逐一核实）。
            "fortran" => Some(Self::Fortran),
            "pascal" => Some(Self::Pascal),
            "haskell" => Some(Self::Haskell),
            "groovy" => Some(Self::Groovy),
            "ocaml" => Some(Self::Ocaml),
            "erlang" => Some(Self::Erlang),
            "perl" => Some(Self::Perl),
            "r" => Some(Self::R),
            "crystal" => Some(Self::Crystal),
            "zig" => Some(Self::Zig),
            // W5 批：七门恒等（lean4 是条目 id 非语言名，zls/zig 双名先例——
            // `--lang lean4` 走 spec_for 按 id 命中 [servers.lean4]，不经此处）。
            "gleam" => Some(Self::Gleam),
            "qml" => Some(Self::Qml),
            "lean" => Some(Self::Lean),
            "julia" => Some(Self::Julia),
            "wolfram" => Some(Self::Wolfram),
            "gdscript" => Some(Self::Godot),
            "msl" => Some(Self::Msl),
            _ => None,
        }
    }
    /// 扩展名（不含点，大小写不敏感） → LanguageId。未知返 None。
    /// 锚：local/ls-lang-extensions.md（71 个 LS 真实枚举）；本表只覆盖有 LanguageId
    /// 值的入口。其余扩展名后续按需扩 LanguageId 后同步加。
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            // C/C++：上游列了 ~28 个扩展（CUDA/HIP/OpenCL/Arduino）；本表覆盖 M3 主线。
            "c" | "cc" | "cpp" | "cxx" | "h" | "hpp" | "hh" | "hxx" => Some(Self::Cpp),
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "go" => Some(Self::Go),
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => Some(Self::TypeScript),
            "cs" => Some(Self::CSharp),
            "java" => Some(Self::Java),
            "md" | "markdown" => Some(Self::Markdown),
            "sql" => Some(Self::Sql),
            "dockerfile" => Some(Self::Docker),
            "sh" | "bash" => Some(Self::Bash),
            "json" | "jsonc" => Some(Self::Json),
            "ps1" | "psm1" | "psd1" => Some(Self::PowerShell),
            "vue" => Some(Self::Vue),
            "astro" => Some(Self::Astro),
            "html" | "htm" => Some(Self::Html),
            "css" => Some(Self::Css),
            "yaml" | "yml" => Some(Self::Yaml),
            "kt" | "kts" => Some(Self::Kotlin),
            "dart" => Some(Self::Dart),
            // W2 批：svelte 单文件组件；sass 双扩展（.css 留在 css 门）。
            // deno 无扩展名映射——TS 家族归 TypeScript 门，仅 --lang deno 显式路由。
            "svelte" => Some(Self::Svelte),
            "sass" | "scss" => Some(Self::Sass),
            // W1b 批：rego/nextflow 各自独占扩展名；ansible 无扩展名映射——.yaml/.yml
            // 归 Yaml 门（momus 裁决的冲突归属），ansible 仅 --lang 显式路由可达。
            "rego" => Some(Self::Rego),
            "nf" => Some(Self::Nextflow),
            // W1a 批：.toml/.cue/.nix 各自独占；.tf/.tfvars 归 Terraform（taplo 只吃
            // .toml；terraform-ls 官方扩展对）。
            "toml" => Some(Self::Toml),
            "tf" | "tfvars" => Some(Self::Terraform),
            "cue" => Some(Self::Cue),
            "nix" => Some(Self::Nix),
            // pgsql/mysql（bd 56a）无专属扩展名：.sql 归 Sql 门（上游 get_priority
            // superset 的默认归属语义），本两门经 --lang pgsql|mysql 显式路由。
            // W3 批：php/lua/scala/swift 各自独占扩展名（.phtml 等上游 intelephense
            // 超集扩展暂不收——M3 主线口径同 cpp）。
            "php" => Some(Self::Php),
            "lua" => Some(Self::Lua),
            "scala" => Some(Self::Scala),
            "swift" => Some(Self::Swift),
            // W4 批：fortran 大小写不敏感族（.F90 同命中——键统一小写 + to_lowercase
            // 匹配）；pascal 主形态 .pas/.pp（.lpr/.dpr/.dpk/.inc 项目文件不收，
            // W1a tfstate 同款子集语义）；ocaml .ml/.mli（.re/.rei Reason 变体不收）；
            // erlang .erl/.hrl（.config/.app 泛用后缀不抢）；perl .pl/.pm/.t；
            // r 族 .r/.rmd/.rnw（上游 .R/.Rmd 大小写不敏感同命中）；groovy .groovy/
            // .gvy（门本体 HOST SKIP 候选，扩展名占位使 resolve 落到明确 no-entry 报错）。
            "f90" | "f95" | "f03" | "f08" | "f" | "for" | "fpp" => Some(Self::Fortran),
            "pas" | "pp" => Some(Self::Pascal),
            "hs" | "lhs" => Some(Self::Haskell),
            "groovy" | "gvy" => Some(Self::Groovy),
            "ml" | "mli" => Some(Self::Ocaml),
            "erl" | "hrl" => Some(Self::Erlang),
            "pl" | "pm" | "t" => Some(Self::Perl),
            "r" | "rmd" | "rnw" => Some(Self::R),
            "cr" => Some(Self::Crystal),
            "zig" | "zon" => Some(Self::Zig),
            // W5 批：七门各自独占扩展名，无存量冲突（.m 归 matlab、.ts 归 TypeScript
            // 均不涉）。wolfram 双扩展 .wl（纯源码）+ .nb（notebook）；gdscript/msl
            // 门本体 SKIP 候选，扩展名占位使 resolve 落到明确 no-entry 报错（groovy 同款）。
            "gleam" => Some(Self::Gleam),
            "qml" => Some(Self::Qml),
            "lean" => Some(Self::Lean),
            "jl" => Some(Self::Julia),
            "wl" | "nb" => Some(Self::Wolfram),
            "gd" => Some(Self::Godot),
            "mrc" => Some(Self::Msl),
            _ => None,
        }
    }
}

/// 适配器上下文：supervisor 启动时把 `(project_root, language)` 传进来。
///
/// ARCH §4.1 草稿示例方法签名里是 `&ProjectCtx`；当前 M0 只读 project_root 决定 cwd
/// 与未来 M3 的 compile_commands 路径。本结构就是 ctx 的物理形态。
#[derive(Debug, Clone)]
pub struct ProjectCtx {
    pub project_root: PathBuf,
}

/// 请求改写钩子（ARCH §4.1 修订 A2：拆 `pre_request(&mut Session)` 为值对象，便于组合/测试）。
///
/// M0 / T0 默认空实现：方法名白名单为空表示「无额外 hook」；clangd T2 视需要在 `M3+`
/// 注入方法特异改写（如 textDocument/definition 调整 → retry 计数）。
#[derive(Debug, Default, Clone)]
pub struct RequestHooks {
    /// 这些方法的请求会被允许跳过 retry（占位；M0 留空）。
    method_allowlist: Vec<&'static str>,
}

impl RequestHooks {
    /// 公开：让测试断言「默认无 hook」与未来 adapter 构造带 hook 的实例。
    pub fn method_allowlist(&self) -> &[&'static str] {
        &self.method_allowlist
    }

    /// T0 / M0 适配器一般用 `RequestHooks::default()` 即可。
    pub fn new() -> Self {
        Self::default()
    }
}

/// root 下候选探针文件（首个存在者胜出）：`.gitignore`/`README.md` 几乎所有仓库都有，
/// 其余是各语言工程标记兜底。
const PROBE_CANDIDATES: &[&str] = &[
    ".gitignore",
    "README.md",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
];

/// `wait_for_index` 默认实现探针的重试间隔。
const RETRY_PAUSE: Duration = Duration::from_millis(250);

/// LanguageId → 探针源文件扩展名（不含点）。探针必须是 adapter 语言的真实源文件：
/// 对 rust workspace 探 Cargo.toml 会让 rust-analyzer 报 -32603（它只吃 .rs 的
/// documentSymbol）→ 就绪门 no-op → 未就绪期调用挂满超时。
fn probe_extensions(lang: &LanguageId) -> &'static [&'static str] {
    match lang {
        LanguageId::Cpp => &["cpp", "cc", "cxx", "c", "hpp", "h"],
        LanguageId::Rust => &["rs"],
        LanguageId::Python => &["py"],
        LanguageId::Go => &["go"],
        LanguageId::Java => &["java"],
        LanguageId::CSharp => &["cs"],
        LanguageId::TypeScript => &["ts", "tsx", "js", "jsx"],
        // T0 注册语言，无 T2 适配器/LS 就绪门语义；探针走工程标记名单。
        LanguageId::Markdown => &[],
        LanguageId::Yaml => &[],
        LanguageId::Docker => &["dockerfile"],
        LanguageId::Sql => &["sql"],
        LanguageId::Bash => &["sh", "bash"],
        LanguageId::Json => &["json"],
        LanguageId::PowerShell => &["ps1", "psm1", "psd1"],
        LanguageId::Vue => &["vue"],
        LanguageId::Astro => &["astro"],
        LanguageId::Html => &["html", "htm"],
        LanguageId::Css => &["css"],
        LanguageId::Pgsql | LanguageId::Mysql => &[],
        LanguageId::Kotlin => &["kt", "kts"],
        LanguageId::Dart => &["dart"],
        // W1b 批：ansible 仅 --lang 显式路由（.yaml/.yml 归 Yaml 门），探针走工程
        // 标记名单——与 Markdown/Yaml/Pgsql|Mysql 同款空表语义。
        LanguageId::Ansible => &[],
        LanguageId::Rego => &["rego"],
        LanguageId::Nextflow => &["nf"],
        // W1a 批：.toml/.tf/.tfvars/.cue/.nix 均为真实 LS 可解析的源文件扩展名。
        LanguageId::Toml => &["toml"],
        LanguageId::Terraform => &["tf", "tfvars"],
        LanguageId::Cue => &["cue"],
        LanguageId::Nix => &["nix"],
        LanguageId::Svelte => &["svelte"],
        // deno：TS 家族扩展名不抢 → 探针候选名单（.gitignore/README 等）兜底。
        LanguageId::Deno => &[],
        LanguageId::Sass => &["sass", "scss"],
        // W3 批：四门均为真实 LS 可解析的源文件扩展名。
        LanguageId::Php => &["php"],
        LanguageId::Lua => &["lua"],
        LanguageId::Scala => &["scala"],
        LanguageId::Swift => &["swift"],
        // W4 批：十门真实源文件扩展名（T0，wait_for_index 默认实现 root 扫描用；
        // groovy 门本体 SKIP，扩展名表仍如实声明）。
        LanguageId::Fortran => &["f90", "f95", "f03", "f08", "f", "for", "fpp"],
        LanguageId::Pascal => &["pas", "pp"],
        LanguageId::Haskell => &["hs", "lhs"],
        LanguageId::Groovy => &["groovy", "gvy"],
        LanguageId::Ocaml => &["ml", "mli"],
        LanguageId::Erlang => &["erl", "hrl"],
        LanguageId::Perl => &["pl", "pm", "t"],
        LanguageId::R => &["r", "rmd", "rnw"],
        LanguageId::Crystal => &["cr"],
        LanguageId::Zig => &["zig", "zon"],
        // W5 批：七门真实源文件扩展名（wolfram/gdscript/msl 门本体 SKIP 候选，
        // 扩展名表仍如实声明——groovy 同款）。
        LanguageId::Gleam => &["gleam"],
        LanguageId::Qml => &["qml"],
        LanguageId::Lean => &["lean"],
        LanguageId::Julia => &["jl"],
        LanguageId::Wolfram => &["wl", "nb"],
        LanguageId::Godot => &["gd"],
        LanguageId::Msl => &["mrc"],
    }
}

/// 源文件探测时的跳过目录（构建产物 / VCS / 依赖树，进去只会浪费扫描时间）。
pub(crate) const PROBE_SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "build",
    "dist",
    ".git",
    ".hg",
    ".svn",
    "__pycache__",
    "vendor",
];

/// root 下找 adapter 语言的首个真实源文件（限深 4 层），触发 LS 的项目 lazy-load。
/// 找不到 → `None`（调用方回退工程标记名单）。
fn find_language_source_file(root: &Path, langs: &[LanguageId], depth: u8) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let exts: Vec<&str> = langs
        .iter()
        .flat_map(|l| probe_extensions(l).iter().copied())
        .collect();
    if exts.is_empty() {
        return None;
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .collect();
    entries.sort();
    // 先文件（同层浅优先），再子目录递归。
    for path in &entries {
        if path.is_file()
            && path
                .extension()
                .is_some_and(|e| exts.contains(&e.to_string_lossy().to_ascii_lowercase().as_str()))
        {
            return Some(path.clone());
        }
    }
    for path in &entries {
        if path.is_dir() {
            let name = path.file_name().map(|n| n.to_string_lossy().to_string());
            if name
                .as_deref()
                .is_some_and(|n| PROBE_SKIP_DIRS.contains(&n) || n.starts_with('.'))
            {
                continue;
            }
            if let Some(hit) = find_language_source_file(path, langs, depth - 1) {
                return Some(hit);
            }
        }
    }
    None
}

/// root 下选就绪探针 URI：优先 adapter 语言的真实源文件（虚拟 URI 不会触发 LS 的项目
/// lazy-load，首个真实工具请求就得独自承担全量索引 —— cold-start hang 根因，见
/// local/cold-start-hang-diagnosis.md；且语言不符的探针会被 LS 拒收，rust-analyzer
/// 对 Cargo.toml 报 -32603）。无语言源文件再退工程标记名单，root 未设置 / 名单全空
/// 退 `fallback` 虚拟 URI（向后兼容旧行为）。
pub(crate) fn probe_uri_for_root(root: &Path, langs: &[LanguageId], fallback: &str) -> String {
    if let Some(source) = find_language_source_file(root, langs, 4) {
        return lsp_core::docsync::path_to_uri_str(&source);
    }
    for name in PROBE_CANDIDATES {
        let candidate = root.join(name);
        if candidate.is_file() {
            return lsp_core::docsync::path_to_uri_str(&candidate);
        }
    }
    fallback.to_string()
}

/// 适配器 trait 签名（ARCHITECTURE §4.1 完整定稿）。
///
/// 九个方法 —— 任何 `T0`（配置驱动）或 `T2`（手写）实现都覆盖。`set_project_root` /
/// `on_server_ready` / `wait_for_index` / `supports_implementation` 给默认实现，
/// 让 T0 模板零代码可用。
#[async_trait]
pub trait LanguageServerAdapter: Send + Sync {
    /// 稳定标识（"clangd" / "rust-analyzer" / ...），对应 `servers.toml` 的 key 与
    /// 上游 `LanguageServerId` 值。
    fn id(&self) -> &'static str;

    /// 本 adapter 服务哪些语言（= supervisor 实例键的 language 维度）。
    fn languages(&self) -> &'static [LanguageId];

    /// 解析启动方式：PATH 查找 / 依赖下载 / 环境变量组装。可能很慢（下载），故 async。
    ///
    /// ↖ mirror: dependency_provider.py@43ae021 `create_launch_command(_env)`
    async fn launch_info(
        &self,
        ctx: &ProjectCtx,
    ) -> anyhow::Result<ls_runtime::process::LaunchInfo>;

    /// 基础 InitializeParams 补丁（capabilities 声明、初始化选项）。
    ///
    /// ↖ mirror: ls.py@43ae021 `_create_base_initialize_params` + initialize_params.py 构造器
    fn initialize_patches(&self, _base: &mut InitializeParams) {}

    /// LS 就绪后钩子：注册 notification handler、等待服务器特有就绪事件/索引完成。
    ///
    /// ↖ mirror: ls.py@43ae021 `on_server_started` / `start` 中的子类等待逻辑。
    async fn on_server_ready(&self, _session: &lsp_core::session::Session) -> anyhow::Result<()> {
        Ok(())
    }

    /// [`Self::on_server_ready`] 的 `Arc` 变体：编排型 adapter（vue hybrid 的伴生
    /// 生命周期绑定需要 `Weak<Session>`）覆写本方法而非 trait 全员改签名；默认转发。
    /// supervisor 统一调本方法。
    async fn on_session_ready(
        &self,
        session: &std::sync::Arc<lsp_core::session::Session>,
    ) -> anyhow::Result<()> {
        Self::on_server_ready(self, session).await
    }

    /// hybrid 语言的语义会话：`.vue` 的 script 类型语义（hover/诊断等）由伴生
    /// TypeScript LS（tsserver + `@vue/typescript-plugin`）承载，主 Vue LS 只承载
    /// SFC 结构与模板语义（↖ mirror 上游双 server 分工：Vue LS 处理 .vue 结构、
    /// TypeScript LS 处理 TS 语义）。返回 `Some` 时 supervisor 把 hover /
    /// signature-help 等位置类语义请求路由到该会话，并把伴生 publishDiagnostics
    /// 并入同一诊断缓存。未拉起 / 已退场 → `None`（调用方回退主会话）。
    fn semantic_session(&self, _root: &Path) -> Option<std::sync::Arc<lsp_core::session::Session>> {
        None
    }

    /// 写类工具（rename / replace-body）入口的索引等待（PLAN Phase 3.2）。
    ///
    /// 默认实现：对被操作文件 `file` 反复发 `textDocument/documentSymbol` 探针，直到
    /// LS 应答或 `timeout` 耗尽；未确认就绪也返回 `Ok` —— 失败回退与 `on_server_ready`
    /// 一致（放行，工具自身请求负责最终报错）。探针必须打在目标文件上：`on_server_ready`
    /// 的根探针（.gitignore/README）不保证该文件符号数据已就绪 —— cold-start 下 rename
    /// 命中 TOOL_TIMEOUT / replace-body 拿到错位 range 的对症点。
    ///
    /// ↖ mirror: ls.py@43ae021 `request_rename` / `replace_text_in_symbol`（上游无此
    /// 等待，索引未就绪时失败或超时）。
    async fn wait_for_index(
        &self,
        session: &lsp_core::session::Session,
        file: &Path,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        let uri = lsp_core::docsync::path_to_uri_str(file);
        let params = serde_json::json!({ "textDocument": { "uri": uri } });
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Ok(()); // 未确认就绪也放行 —— 回退契约同 on_server_ready
            }
            match session
                .request::<serde_json::Value>(
                    "textDocument/documentSymbol",
                    params.clone(),
                    remaining,
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(_) => tokio::time::sleep(RETRY_PAUSE).await,
            }
        }
    }

    /// 记录当前项目 root，供 `on_server_ready` 选真实文件探针（触发 LS 项目索引）。
    /// 默认空实现：无文件探针需求的 adapter（如 jdtls 的 language/status 路径）不必覆盖。
    fn set_project_root(&self, _root: &Path) {}

    /// 请求改写钩子（quirk 用：改 params、注入额外通知）。默认无操作。
    fn request_hooks(&self) -> RequestHooks {
        RequestHooks::default()
    }

    /// 该服务器是否支持 `textDocument/implementation`（能力探测的静态先验）。
    ///
    /// ↖ mirror: ls_config.py@43ae021 `supports_implementation_request`
    fn supports_implementation(&self) -> bool {
        false
    }

    /// 跨文件引用类查询（references）发请求前的 `$/progress` 索引等待。
    ///
    /// ↖ mirror: ls.py@43ae021 `_wait_for_cross_file_references_if_needed`（调用位次：
    ///           didOpen 之后、请求之前）
    ///           ↖ mirror: @cf54869a 修订 —— 首查 latch 只覆盖 start-grace 等待；
    ///           后续查询若仍有在飞索引 token 则继续 drain，避免 tsserver 后台索引期
    ///           跨文件查询静默返回空/部分结果。
    /// 默认空实现：不跟踪 `$/progress` 的 LS 直接放行。等待/超时不报错 —— 超时由
    /// 实现方 warn 后放行（上游 permissive 行为），请求自身超时兜底。
    async fn wait_for_cross_file_index(&self, _session: &lsp_core::session::Session) {}
}

/// 在 PATH 中查找可执行文件（去 UNC 前缀 dunce），返回 None 表示未找到。
///
/// ponylabel: 不引 `which` crate —— `which = "6"` 是最小 CLI which 替代，但 std PATH 遍历
/// ~15 行就够；避免新依赖。详见 PLAN Task 8 决策记录。
///
/// dunce 在 Windows 下把 `\\?\C:\...` 退化为 `C:\...`，避免污染 LSP URI。
pub(crate) fn which_no_unc(name: &str) -> Option<PathBuf> {
    // Windows 不含无扩展名候选：PATH 上的同名裸文件是 npm/sh shim（sh 脚本，
    // CreateProcess 无法执行），先命中会令下游 spawn 失败 —— doctor 报 npm MISS
    // 而 `where.exe npm` 命中的根因。.exe/.cmd/.bat 才是 Windows 可执行形态。
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in exts {
            let mut candidate = dir.join(name);
            if !ext.is_empty() {
                candidate.set_extension(&ext[1..]);
            }
            if candidate.is_file() {
                // dunce 去 Windows UNC 前缀（\\?\C:\... → C:\...）。
                return Some(dunce::canonicalize(&candidate).unwrap_or(candidate));
            }
        }
    }
    // 兜底：LLVM 标准安装路径（用户环境 Windows winget 默认 D:/Program Files）——
    // PATH 没挂时也能用，避免"装了却报 not installed"的 UX 撕裂。仅 Windows。
    // 测试可通过设 `SERENA_SKIP_LLVM_FALLBACK=1` 临时禁用以验证 not-installed 路径。
    if cfg!(windows)
        && std::env::var_os("SERENA_SKIP_LLVM_FALLBACK").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        && matches!(name, "clangd" | "clangd.exe" | "clangd.cmd" | "clangd.bat")
    {
        for dir in ["D:/Program Files/LLVM/bin", "C:/Program Files/LLVM/bin"] {
            let p = std::path::Path::new(dir).join("clangd.exe");
            if p.is_file() {
                return Some(dunce::canonicalize(&p).unwrap_or(p));
            }
        }
    }
    None
}

/// 在 PATH 中查找可执行文件并去除 Windows UNC 前缀（servers.toml path_only/下载
/// 产物探测共用；`which_no_unc` 的公开薄壳）。
pub fn which_path(name: &str) -> Option<PathBuf> {
    which_no_unc(name)
}

/// `launch_info` 找不到目标时的标准错误：`LS_NOT_INSTALLED` 语义（ARCH §6.3）。
pub(crate) fn not_installed_error(name: &str, install_hint: &str) -> anyhow::Error {
    anyhow::anyhow!("language server `{name}` not found in PATH; install_hint: {install_hint}")
}

/// 把 `OsString` 列表转 `Vec<OsString>`（方便 stub 写出）。
#[allow(dead_code)]
pub(crate) fn os_args<I, S>(items: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    items.into_iter().map(Into::into).collect()
}

/// 检查路径是否存在（用于 PATH 查找的 std 替代）。
#[allow(dead_code)]
pub(crate) fn exists(p: &Path) -> bool {
    p.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyAdapter;

    #[async_trait]
    impl LanguageServerAdapter for DummyAdapter {
        fn id(&self) -> &'static str {
            "dummy"
        }

        fn languages(&self) -> &'static [LanguageId] {
            &[]
        }

        async fn launch_info(
            &self,
            _ctx: &ProjectCtx,
        ) -> anyhow::Result<ls_runtime::process::LaunchInfo> {
            unimplemented!("probe tests 不启动 LS")
        }

        // set_project_root / on_server_ready / 其余方法走默认实现。
    }

    #[test]
    fn probe_uri_prefers_language_source_over_marker_files() {
        let dir = tempfile::tempdir().unwrap();
        // rust workspace：.gitignore 与 Cargo.toml 都在，但探针必须选 .rs ——
        // rust-analyzer 对非源文件 documentSymbol 报 -32603（BD serena-rust-s3q）。
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src").join("lib.rs"), "fn main() {}\n").unwrap();
        let uri = probe_uri_for_root(dir.path(), &[LanguageId::Rust], "file:///__fallback__");
        assert!(uri.ends_with("lib.rs"), "探针必须是语言源文件: {uri}");
    }

    #[test]
    fn probe_uri_falls_back_to_markers_without_language_source() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        let uri = probe_uri_for_root(dir.path(), &[LanguageId::Rust], "file:///__fallback__");
        assert!(uri.starts_with("file:///"), "必须是 file URI: {uri}");
        assert!(
            uri.ends_with(".gitignore"),
            "无语言源文件应退工程标记: {uri}"
        );
    }

    #[test]
    fn probe_uri_falls_back_when_no_candidate_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            probe_uri_for_root(dir.path(), &[], "file:///__fallback__"),
            "file:///__fallback__"
        );
    }

    #[test]
    fn probe_uri_skips_build_and_hidden_dirs_when_scanning() {
        let dir = tempfile::tempdir().unwrap();
        // target/ 里的 .rs 不是用户源码 —— 不得被探针选中。
        std::fs::create_dir_all(dir.path().join("target").join("debug")).unwrap();
        std::fs::write(dir.path().join("target").join("debug").join("junk.rs"), "").unwrap();
        assert_eq!(
            probe_uri_for_root(dir.path(), &[LanguageId::Rust], "file:///__fallback__"),
            "file:///__fallback__"
        );
    }

    #[test]
    fn probe_uri_picks_first_existing_candidate_in_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        // 空 langs → 语言扫描跳过 → 纯名单路径（README.md 胜出）。
        let uri = probe_uri_for_root(dir.path(), &[], "file:///__fallback__");
        assert!(uri.ends_with("README.md"), "按候选序取首个存在者: {uri}");
    }

    #[test]
    fn probe_extensions_w1b_gates() {
        // ansible 仅 --lang 显式路由（.yaml/.yml 归 Yaml 门，momus 裁决）→ 空表
        // 走工程标记名单，与 Markdown/Yaml 同语义；rego/nextflow 各自独占扩展名。
        assert!(probe_extensions(&LanguageId::Ansible).is_empty());
        assert_eq!(probe_extensions(&LanguageId::Rego), &["rego"]);
        assert_eq!(probe_extensions(&LanguageId::Nextflow), &["nf"]);
    }

    #[test]
    fn default_set_project_root_is_noop() {
        // 默认空实现可调用 —— 现有/未来不覆盖 set_project_root 的 adapter 不破坏。
        DummyAdapter.set_project_root(Path::new("D:/anywhere"));
    }

    // === wait_for_index 默认实现（SERENA_REPLAY 手写 JSONL 驱动，不依赖外部进程）===
    //
    // env 是进程全局的：两个用例共用一把 tokio Mutex 串行化，防止互相污染。

    static REPLAY_ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// 手写回放文件：只含 initialize 应答。documentSymbol 响应不排队 —— replay 泵
    /// 启动即排空入站队列，先到的帧会因请求尚未 pending 而被丢弃；happy path 用例
    /// 改经 `Client::handle_message` 延迟注入。
    fn write_replay(dir: &Path) -> PathBuf {
        let lines = [
            r#"--> {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            r#"<-- {"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#,
        ];
        let path = dir.join("replay.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    async fn boot_replay_session(replay: &Path) -> std::sync::Arc<lsp_core::session::Session> {
        // SAFETY: REPLAY_ENV 保证本进程内独占访问 SERENA_REPLAY。
        unsafe { std::env::set_var("SERENA_REPLAY", replay) };
        let session = lsp_core::session::Session::start(
            None,
            lsp_core::init_params::base_initialize_params(),
        )
        .await
        .expect("replay session Ready");
        // SAFETY: 同上 —— 用完即清，不污染同进程其他用例。
        unsafe { std::env::remove_var("SERENA_REPLAY") };
        session
    }

    #[tokio::test]
    async fn default_wait_for_index_returns_once_ls_answers_document_symbol() {
        let _env = REPLAY_ENV.lock().await;
        let dir = tempfile::tempdir().unwrap();
        let session = boot_replay_session(&write_replay(dir.path())).await;
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        // 假 LS：200ms 后补投 documentSymbol 响应（id 2 —— client next_id 从 1 起，
        // initialize 占 1）。handle_message 与真泵同一条分发路径。
        let fake_ls = {
            let client = session.client().clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                client.handle_message(lsp_core::framing::JsonRpc::response_ok(
                    serde_json::Value::Number(2.into()),
                    serde_json::json!([]),
                ));
            })
        };

        let started = std::time::Instant::now();
        let res = DummyAdapter
            .wait_for_index(&session, &file, Duration::from_secs(5))
            .await;
        assert!(res.is_ok(), "LS 已应答 documentSymbol → Ok");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "响应注入后探针应立即成功，实际 {:?}",
            started.elapsed()
        );
        fake_ls.await.unwrap();
    }

    #[tokio::test]
    async fn default_wait_for_index_gives_up_after_deadline_still_ok() {
        let _env = REPLAY_ENV.lock().await;
        let dir = tempfile::tempdir().unwrap();
        // documentSymbol 永远无回执（= LS 一直索引中）。
        let session = boot_replay_session(&write_replay(dir.path())).await;
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let started = std::time::Instant::now();
        // 外层 5s 护栏：默认实现必须在 deadline（600ms）附近返回，不允许挂死。
        let res = tokio::time::timeout(
            Duration::from_secs(5),
            DummyAdapter.wait_for_index(&session, &file, Duration::from_millis(600)),
        )
        .await
        .expect("deadline 后必须返回");
        assert!(res.is_ok(), "超时也放行 —— 回退契约同 on_server_ready");
        assert!(
            started.elapsed() >= Duration::from_millis(550),
            "应熬满 deadline 而非立即返回，实际 {:?}",
            started.elapsed()
        );
    }
}
