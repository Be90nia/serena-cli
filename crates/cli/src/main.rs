//! serena-cli —— single-binary LSP CLI（PLAN Task 10/16 / ARCHITECTURE §2）。
//!
//! 三种模式：
//! - `--direct`：进程内直连 LS（M0 冒烟/单测路径）。
//! - `--daemon`：本进程作为常驻 daemon（lock 仲裁 + HTTP + reaper）。
//! - 默认（无 flag）：转发模式——读 lock → TCP 探活 → 活着转发 / 死了 lazy-spawn
//!   自身 `--daemon`（I5：CREATE_NO_WINDOW + 新进程组 + 句柄不继承 + stdin/stdout→NULL）。
//!
//! 管理命令：`status` / `stop-all`。
//!
//! Windows 启动即 `SetConsoleOutputCP(65001)` —— 中文 Windows conhost 默认 GBK 码页。
//!
//! Exit code 协议（ARCH §6.3）：
//! 0 成功 / 1 工具失败（含 LS 未装）/ 2 用法错 / 3 daemon 或传输故障 /
//! 4 wait-ready 超时（bd serena-rust-55m）。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::json;
use supervisor::{Supervisor, SupervisorTrait, ToolError};

mod lint_shell;

/// 转发超时（工具请求 300s；管理命令 3s，daemon 卡死时 stop-all 快速失败）。
const FORWARD_TIMEOUT: Duration = Duration::from_secs(300);
const MGMT_TIMEOUT: Duration = Duration::from_secs(3);
/// lazy-spawn 后等 daemon 就绪的总窗口。
const SPAWN_WAIT: Duration = Duration::from_secs(10);
/// bd fakewait：draining 老 daemon 退净的接管等待上限。daemon 侧 drain_window
/// 15s（daemon/src/serve.rs，常量不跨 crate 暴露，此处客户端镜像）+ 收尾余量；
/// finish_shutdown = 删 lock → 立即 process::exit(0)，两事件毫秒级先后。
const DRAIN_TAKEOVER_WAIT: Duration = Duration::from_secs(20);
/// 残留 daemon 端口探活超时（bd 3ab）。
const RESIDUAL_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// 终止残留进程后等 listen socket 释放再复测的间隔。
const REAP_RECHECK_DELAY: Duration = Duration::from_millis(300);

/// bd de4：负载波峰下 loopback connect 瞬断实测 5-10%（daemon accept 处理不过来）。
/// 客户端 4 次指数退避（50/100/200/400ms）实测 12 线程并发 0% 错误（T2 验证；
/// 2 次退避仍 3.4%）。
const CONNECT_BACKOFF_MS: [u64; 4] = [50, 100, 200, 400];

/// CLI 侧共享 HTTP client：本机回环，建连 2s 封顶（管理面失败即报，daemon 侧自愈）。
fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .build()
        .expect("reqwest client build")
}

/// 批3 可发现性（ADR serena-rust-ai-experience-9.5 批3）：help 首屏高频 5 命令
/// 速查。高频依据 = 盲测 v4.5 三任务链（T1 warm→overview / T2 find-symbol→
/// replace-body→diagnostics / T3 rename→undo）跨链交集 + 安全网。
const BEFORE_HELP_GUIDE: &str = "\
高频 5 命令速查（全量清单见文末按用途分组；细节 <cmd> --help）:
  warm <LANG>                                 预热 LS+索引，开工先发（或 --lang <LANG>）
  find-symbol <QUERY>                         按名找符号（精确名；宽查询加 --lang/--limit）
  symbol-body <FILE> <SYMBOL>                 按名取符号体（读类最省）
  replace-body <FILE> <SYMBOL> --with <NEW>   换符号体（先 --dry-run 看 patch 预览）
  undo                                        回滚上一步写操作（事务级，一次全回）
";

/// 杠精 ke2a：65 个功能子命令无分组导读 → AI 一次 help 定位候选命令，减少盲猜轮次。
const AFTER_HELP_GUIDE: &str = "\
按用途找命令（全 65 个，不含 help 元命令；用法细节 `<cmd> --help`）:
  读/导航      overview symbol-tree read-file list-dir find-file search find-symbol symbol-body edit-context containing-symbol defining-symbol repo-map
  语义查询     def refs hover find-implementations find-referencing-symbols find-referencing-code-snippets completion signature-help code-action document-highlight call-hierarchy type-hierarchy moniker document-link inlay-hint folding-range semantic-tokens code-lens
  诊断         diagnostics workspace-diagnostic
  写/编辑      replace-body replace-text-in-symbol insert-text-before-symbol insert-text-after-symbol delete-text-in-symbol insert-at-line replace-lines delete-lines rename-symbol safe-delete-symbol create-text-file format format-range
  事务/工作流  undo redo diff find-test test recipe
  LS/环境      status project-info change-history warm wait-ready stop-all install uninstall ls-use ls-list ls-remove doctor shell lint-shell

约定: 行号一律 1-based 含端；stdout 只出 JSON（成功=data 载荷 / 失败=error 对象 {code,message,retryable}，用法错 rc=2 同流），stderr=[warn]/[hint]/[error] 人读行；
写类命令支持全局 --dry-run（返 would_write[].patch 预览）；--max-tokens/--compress/--json 全局可用。";

#[derive(Parser, Debug)]
#[command(
    name = "serena-cli",
    version,
    about = "serena-rust LSP CLI",
    before_help = BEFORE_HELP_GUIDE,
    after_help = AFTER_HELP_GUIDE,
    // 复测4 yim0-邻接: 零参数 cmd=None 曾直通 forward 的 expect panic(rc=101), 改为 usage rc=2
    arg_required_else_help = true
)]
struct Cli {
    /// 直连模式：单进程拉 LS 直调（M0 路径）。
    #[arg(long, conflicts_with = "daemon")]
    direct: bool,

    /// daemon 模式：本进程作为常驻 daemon。
    #[arg(long)]
    daemon: bool,

    /// 项目根（--direct 模式必填；转发模式透传给 daemon）。
    #[arg(long, value_name = "ROOT")]
    project: Option<PathBuf>,

    /// 完整 JSON 输出：关闭默认的紧凑裁剪（默认已是 JSON、紧凑形态；本 flag 保留全部字段）。
    #[arg(long, global = true)]
    json: bool,

    /// 覆盖文件扩展名探测：多语言项目用 (如 --lang typescript 在 .ts 项目里用 TS LS)。
    /// find-symbol 不指定时也用它过滤到单 LS。
    #[arg(long, global = true, value_name = "LANG")]
    lang: Option<String>,

    /// 全局工具请求超时（毫秒）。优先级最高：CLI > servers.toml
    /// `[defaults].timeout_ms` > 30s 默认。`--index-timeout` 单独覆盖 workspace/symbol
    /// 等长操作（默认 120s）。
    #[arg(long, global = true, value_name = "MS")]
    request_timeout: Option<u32>,

    /// 索引型工具（workspace/symbol、workspace/diagnostic 等）超时（毫秒）。默认 120s。
    #[arg(long, global = true, value_name = "MS")]
    index_timeout: Option<u32>,

    /// 批2-A：语义工具渐进首答的就绪等待上限（毫秒）。默认 15s——预算内 LS
    /// 答复则全量结果；超时立即降级返回（degraded/warmup 标记），不再 120s 死等。
    #[arg(long, global = true, value_name = "MS")]
    warmup_timeout: Option<u32>,

    /// 限制响应大小（soft limit，约 4 字节 ≈ 1 token）。集合型响应超限时截断
    /// 条目并标 `truncated:true`（仍是成功，退出码不变）。
    #[arg(long, global = true, value_name = "N")]
    max_tokens: Option<u64>,

    /// 精简输出：删掉 container/container_name/kind 等冗余字段，省 token。
    #[arg(long, global = true)]
    compress: bool,

    /// 编排 invocation id（UUID v4）。缺省自动生成；显式指定用于
    /// 幂等重放与跨 agent 排障（daemon 重放日志按此索引）。
    #[arg(long, global = true, value_name = "ID")]
    invocation_id: Option<String>,

    /// 写类命令干跑——执行完整定位/计算/校验但不落盘、不进 undo 事务；
    /// 成功返回附 `dry_run:true` + `applied:false` + `would_apply:true` 与
    /// `would_write[{file,patch}]`（unified diff 预览，防大文件全文 token 炸弹）。
    /// 仅写类命令消费，读类忽略。
    #[arg(long, global = true)]
    dry_run: bool,

    /// 子命令；`--daemon` 模式下可省略。
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// 列出文件顶层符号。
    Overview {
        file: String,
        /// 返上次调用以来增量（added/removed）而非全集；首次返全集。
        #[arg(long)]
        delta: bool,
    },
    /// 聚合目录下源码文件符号（跨文件符号树；依赖 LS，逐文件可复用缓存）。
    SymbolTree {
        dir: String,
        /// 保险丝：最多扫描文件数（超出截断并标 truncated）。
        #[arg(long, value_name = "N", default_value_t = 200)]
        max_files: usize,
        /// 只列符号名含此子串的条目（大小写不敏感）。
        #[arg(long, value_name = "PATTERN")]
        grep: Option<String>,
        /// 只保留包含链深度 < N 的符号（顶层=0）。
        #[arg(long, value_name = "N")]
        max_depth: Option<usize>,
        /// 只列文件清单（零 LS 调用）。
        #[arg(long, default_value_t = false)]
        files_only: bool,
    },
    /// 跳转到符号定义（textDocument/definition）。line/col 为 1-based。
    Def { file: String, line: u32, col: u32 },
    /// 列出引用（textDocument/references）。line/col 1-based。
    Refs {
        file: String,
        line: u32,
        col: u32,
        /// 返上次调用以来增量（added/removed）而非全集；首次返全集。
        #[arg(long)]
        delta: bool,
    },
    /// 鼠标位置符号的 type / doc（textDocument/hover）。line/col 为 1-based。
    Hover { file: String, line: u32, col: u32 },
    /// 拉取文件诊断（pull diagnostics）；`--wait-gen N` 等到 generation ≥ N 再返回。
    Diagnostics {
        file: String,
        /// 等 diagnostics generation >= N（替代盲轮询 5s）；0=立即返回当前；仍受 5s 上限。
        #[arg(long, value_name = "GEN")]
        wait_gen: Option<u64>,
    },
    /// 全 workspace 跨文件符号查找（workspace/symbol）。
    FindSymbol {
        /// 子串或正则（取决于 LSP server 行为，clangd 默认子串）。
        query: String,
        /// 上限。
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// 输出形态：brief = `"name file:line:col"` 单串（最省）；
        /// full（默认）= 既有紧凑 wire；json = 全字段形态。
        #[arg(long, value_enum, default_value_t = OutFormat::Full)]
        format: OutFormat,
        /// 返上次调用以来增量（added/removed）而非全集；首次返全集。
        #[arg(long)]
        delta: bool,
    },
    /// 符号的所有实现位置（textDocument/implementation）。line/col 为 1-based。
    FindImplementations {
        file: String,
        line: u32,
        col: u32,
        /// 返上次调用以来增量（added/removed）而非全集；首次返全集。
        #[arg(long)]
        delta: bool,
    },
    /// 跨文件 rename（textDocument/rename）。line/col 为 1-based。
    RenameSymbol {
        file: String,
        line: u32,
        col: u32,
        /// 新名。
        #[arg(long = "to")]
        new_name: String,
    },
    /// workspace/search：跨文件正则搜索。
    Search {
        pattern: String,
        /// glob 过滤文件路径（如 **/*.cpp）。
        #[arg(long)]
        path_glob: Option<String>,
        /// 最大结果数（默认 50；批1-B 防噪硬上限）。
        #[arg(long, default_value_t = 50)]
        max_results: u32,
        /// 仅保留注释行命中（I：--comments-only）。
        #[arg(long, default_value_t = false)]
        comments_only: bool,
        /// 大小写敏感（默认不敏感）。
        #[arg(long, default_value_t = false)]
        case_sensitive: bool,
        /// 同符号多行命中只留首条（需命中带所属符号，未装饰行全保留）。
        #[arg(long, default_value_t = false)]
        distinct_symbols: bool,
        /// 排除文件 glob（可多次；同 path_glob 语法，如 `*_measure.py`）。命中即跳过，
        /// 不计数不读取（杠精 cv1e：测量脚本等噪音不进 token 账单）。
        #[arg(long, value_name = "GLOB")]
        exclude: Vec<String>,
        /// 逃生：不尊重 .gitignore（.git/ 与内置 ignore 目录仍排除）。
        #[arg(long, default_value_t = false)]
        no_ignore: bool,
        /// 输出形态：brief = grep 风格 `file:line:col: text` 单串；
        /// full（默认）= 既有全形态（search 无紧凑裁剪层，full 与 json 同形）。
        #[arg(long, value_enum, default_value_t = OutFormat::Full)]
        format: OutFormat,
    },
    /// 按行范围读文件（1-based 含端）。`--start/--end` 为 `--start-line/--end-line`
    /// 短别名（杠精 cqns：猜错旗标名 = 一轮浪费调用，别名 + 长名同价）。
    ReadFile {
        file: String,
        /// 起始行（1-based，默认 1）。
        #[arg(long)]
        start_line: Option<u32>,
        /// 结束行（1-based 含端，默认 EOF）。
        #[arg(long)]
        end_line: Option<u32>,
        /// `--start-line` 短别名（二选一，双给拒）。
        #[arg(long)]
        start: Option<u32>,
        /// `--end-line` 短别名（二选一，双给拒）。
        #[arg(long)]
        end: Option<u32>,
        /// bd a14g：soft limit（content 字节 = N*4 − 32 元数据留余），超按整行砍
        /// （留半行丢），响应附 `truncated:true/total_bytes/total_tokens`；0 = BAD_ARGS。
        #[arg(long, value_name = "N")]
        max_tokens: Option<u64>,
    },
    /// 列出目录项（不递归）。
    ListDir { path: String },
    /// 按文件名 glob 查找文件（限深 5）。
    FindFile {
        /// glob 模式（如 main.cpp）。
        name_pattern: String,
    },
    /// 所有引用 + 每个 ref 落在哪个外层符号里。line/col 为 1-based。
    FindReferencingSymbols {
        file: String,
        line: u32,
        col: u32,
        /// 按 (container, file) 分桶聚合 + 翻页。
        #[arg(long)]
        grouped: bool,
        #[arg(long, default_value_t = 1)]
        page: usize,
        #[arg(long, default_value_t = 20)]
        page_size: usize,
        /// 静默空时附 LS 原始响应 200B 快照（aap4；或 SERENA_DEBUG_RAW=1）。
        #[arg(long, default_value_t = false)]
        debug_raw: bool,
    },
    // bd serena-rust-bxd（内部追踪号，不入 --help）
    /// 所有引用 + 每个 ref 前后 N 行。line/col 为 1-based。
    /// `--symbol <NAME>` 直查：免两步 find-symbol 拿坐标；
    /// 给了 --symbol 则 FILE/LINE/COL 可省（内部解析首命中，命中多个时 warning
    /// 提示用了哪个，零命中 rc=2）。
    FindReferencingCodeSnippets {
        file: Option<String>,
        line: Option<u32>,
        col: Option<u32>,
        /// 符号名直查：documentSymbol 缓存精确名优先、其次前缀，转 line/col。
        #[arg(long, value_name = "NAME")]
        symbol: Option<String>,
        /// 每个 ref 上下文行数（前后对称）。
        #[arg(long, default_value_t = 3)]
        context_lines: u32,
        /// 上限。
        #[arg(long, default_value_t = 20)]
        max_results: u32,
        /// 静默空时附 LS 原始响应 200B 快照（aap4；或 SERENA_DEBUG_RAW=1）。
        #[arg(long, default_value_t = false)]
        debug_raw: bool,
    },
    /// 取符号体切片（position-free；documentSymbol 定位）。符号名：位置第二参或
    /// `--symbol`（二选一；与 find-referencing-code-snippets 参数形状对齐）。
    SymbolBody {
        /// 目标文件（相对 root）。
        file: String,
        /// 符号名（位置第二参，兼容保留）。
        symbol: Option<String>,
        /// 符号名旗标（与位置第二参等价二选一）。
        #[arg(long = "symbol", value_name = "NAME")]
        symbol_flag: Option<String>,
    },
    /// AI 编辑主路径聚合：单次返回 body + callers + doc + tests。
    EditContext {
        /// 目标文件（相对 root）。
        file: String,
        /// 符号名（位置第二参，兼容保留）。
        symbol: Option<String>,
        /// 符号名旗标（与位置第二参等价二选一）。
        #[arg(long = "symbol", value_name = "NAME")]
        symbol_flag: Option<String>,
    },
    /// 全 workspace 符号地图（按调用热度 top N）。
    RepoMap {
        /// top N 符号（默认 20；超过按文件+顶层符号清单截断）。
        #[arg(long, default_value_t = 20)]
        top_n: u32,
    },
    /// 预热 LS + 索引：开工前一发，首个真实工具调用免吃冷启动。
    /// 超时返 partial:true（LS 已启动、索引未确认），不阻塞。
    Warm {
        /// 要预热的语言。缺省时按项目根清单探测（Cargo.toml/pyproject.toml/
        /// tsconfig.json/package.json）；探测失败 rc=2。
        lang: Option<String>,
        /// 双形态：与位置参 LANG 等价（`warm --lang rust` ≡ `warm rust`）。
        #[arg(long = "lang", value_name = "LANG")]
        lang_flag: Option<String>,
        /// 就绪等待上限（秒）。
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
    },
    // bd serena-rust-55m / bxd（内部追踪号，不入 --help）
    /// 阻塞到就绪：循环探测。`--stage symbol` =
    /// overview 首符号非空即就绪（符号索引层，秒级）；`--stage indexing` =
    /// RA Indexing progress end 真就绪屏障（bd 0vj1；hover/def 的 prime-caches
    /// 路径收敛，预算按 Cargo.lock 规模 60-300s）；`--stage semantic`（默认，
    /// 保持现行为）= hover contents 非空（类型分析层；未就绪响应带 warning，
    /// 解析即判据）。就绪 exit 0；超时 exit 4。探测间隔 500ms 起指数退避到 2s
    /// 封顶，进度（含阶段）单行打 stderr。
    WaitReady {
        /// 探测目标文件（默认项目内首个源文件，路径相对项目根或绝对）。
        #[arg(long, value_name = "FILE")]
        file: Option<String>,
        /// 就绪等待上限（秒）。可用环境变量 SERENA_WAIT_READY_TIMEOUT_SECS
        /// 覆盖默认 120s（显式 --timeout 优先；非法值 warn + 用默认）。
        #[arg(long, value_name = "N")]
        timeout: Option<u64>,
        /// 就绪档位：symbol = 符号索引可用；semantic = hover 类型分析可用；
        /// def = 语义解析层可用（def 非空，与 def/refs 同层——hover 就绪后 def
        /// 仍可能空窗，紧接着要跑 def/refs 的用这档）。
        #[arg(long, value_enum, default_value_t = WaitStage::Semantic)]
        stage: WaitStage,
    },
    /// 替换符号体（写门 + hash 对账 + 原子写）。
    ReplaceBody {
        file: String,
        symbol: String,
        /// 新符号体完整文本（`--new-body` 同义别名，与 recipe fix-bug 互通）。
        #[arg(long = "with", visible_alias = "new-body")]
        new_body: String,
    },
    /// 在 symbol 体内替换 old → new（行级字节切片）。
    ReplaceTextInSymbol {
        file: String,
        symbol: String,
        /// 待替换原文。
        old_text: String,
        /// 新文。
        new_text: String,
    },
    /// 在 symbol 开头插入 text。
    /// 文本可走位置参数或 `--with`（与 replace-body 同形）。
    InsertTextBeforeSymbol {
        file: String,
        symbol: String,
        text: Option<String>,
        #[arg(long = "with")]
        with: Option<String>,
    },
    /// 在 symbol 末尾插入 text。文本可走位置参数或 `--with`。
    InsertTextAfterSymbol {
        file: String,
        symbol: String,
        text: Option<String>,
        #[arg(long = "with")]
        with: Option<String>,
    },
    /// 在 symbol 体内删除 [start_line, end_line] 切片（1-based 含端）。
    DeleteTextInSymbol {
        file: String,
        symbol: String,
        start_line: u32,
        end_line: u32,
    },
    /// 安全删除符号：无引用才删；有引用拒删并列出引用位置。
    SafeDeleteSymbol { file: String, symbol: String },
    /// 在 line（1-based）前插入内容，原行下移；line = 总行数+1 即追加。
    /// 文本可走位置参数或 `--with`。
    InsertAtLine {
        file: String,
        line: u32,
        text: Option<String>,
        #[arg(long = "with")]
        with: Option<String>,
        /// 可选：上次 read-file 返回的全文 hash，不符拒写。
        #[arg(long)]
        expected_hash: Option<String>,
    },
    /// 用新内容替换 [start_line, end_line]（1-based 含端）。
    /// 文本可走位置参数或 `--with`。
    ReplaceLines {
        file: String,
        start_line: u32,
        end_line: u32,
        text: Option<String>,
        #[arg(long = "with")]
        with: Option<String>,
        /// 可选：上次 read-file 返回的全文 hash，不符拒写。
        #[arg(long)]
        expected_hash: Option<String>,
    },
    /// 删除 [start_line, end_line]（1-based 含端）。
    DeleteLines {
        file: String,
        start_line: u32,
        end_line: u32,
        /// 可选：上次 read-file 返回的全文 hash，不符拒写。
        #[arg(long)]
        expected_hash: Option<String>,
    },
    /// 新建文件（已存在 = 参数错）。写入自动进 undo 事务（created=true）。
    /// 内容可走位置参数、`--with`、`--stdin` 或 `--content-file`（多行内容
    /// shell 引号难写干净，stdin/文件退路 bash 友好）。
    CreateTextFile {
        file: String,
        /// 文件完整内容。
        content: Option<String>,
        #[arg(long = "with")]
        with: Option<String>,
        /// 从 stdin 读全文（与其他内容来源互斥）。
        #[arg(long)]
        stdin: bool,
        /// 从文件读全文（与其他内容来源互斥）。
        #[arg(long, value_name = "PATH")]
        content_file: Option<String>,
    },
    /// 回滚最近的写事务（IDE undo）。project_root 由 --project 或 cwd 定位。
    /// 遇外部编辑冲突（sha 不符）：该事务 discarded 并**立即停止**——不回滚
    /// 更老事务（时间线已乱），返回已处理部分 + rc=2（stopped_early 标记）。
    Undo {
        /// 回滚事务数。
        #[arg(long, value_name = "N", default_value_t = 1)]
        steps: u32,
        /// 列出 undo 栈概览（txn id/时间/文件数/摘要），不执行回滚。
        #[arg(long)]
        list: bool,
    },
    /// 重放最近被 undo 的事务（IDE redo）。
    Redo,
    /// 运行测试（cargo/npm 后端按路径自动选；只读源码，产物不进 undo 栈）。
    Test {
        /// 测试目标：crate/test 目录或测试文件路径（相对项目根或绝对）。
        file: String,
        /// 测试名过滤器（cargo test 位置参数 / npm 透传）。
        #[arg(value_name = "NAME")]
        name: Option<String>,
    },
    /// 写事务写前写后对照（读 undo store；缺省 = 最近活跃事务）。
    Diff {
        /// 事务号（缺省 = 最近活跃事务；显式 id 也可读已回滚的 undone 事务）。
        #[arg(value_name = "TXN_ID")]
        txn_id: Option<u64>,
        /// 输出 unified diff（patch -p1 / git apply 可直接消费）。
        #[arg(long)]
        patch: bool,
    },
    /// 按启发式链找符号的测试（tests/ 镜像 → 测试目录/命名 → super:: 单测 → LS refs）。
    /// 符号名：位置参数或 `--symbol`（二选一；杠精 cqns：与 symbol-body /
    /// find-referencing-code-snippets 的 `--symbol` 形状对齐）。
    FindTest {
        /// 符号名（位置参数；给了 `--symbol` 可省）。
        symbol: Option<String>,
        /// 符号名别名旗标（与位置参数等价，双给拒）。
        #[arg(long = "symbol", value_name = "NAME")]
        symbol_flag: Option<String>,
    },
    /// 预定义工作流编排（8 recipe 单入口）。写步各自独立 undo
    /// 事务；单步失败即停并逆序回滚已完成的写步（报告含 undo 结果）。
    Recipe {
        /// recipe 名：fix-bug|add-feature|rename|add-test|refactor-extract|refactor-rename|review-diff|explore
        name: String,
        /// 位置参数：fix-bug/rename/refactor-extract = <file> <sym>；add-feature
        /// = <name>；add-test/refactor-rename = <sym>；explore = <path>；
        /// review-diff = [txn-id]。
        #[arg(value_name = "ARG")]
        args: Vec<String>,
        /// fix-bug：替换后的新函数体（缺省 = 只跑分析链，不写；`--with` 同义
        /// 别名，与 replace-body 互通）。
        #[arg(long, visible_alias = "with")]
        new_body: Option<String>,
        /// rename / refactor-rename：新名。
        #[arg(long)]
        to: Option<String>,
        /// refactor-extract：抽取出的新 fn 名。目标限 .rs（该重构仅 Rust 实现；
        /// 其他语言拒绝并说明原因）。
        #[arg(long = "as", value_name = "NEW")]
        as_name: Option<String>,
        /// add-feature：stub 落盘目标文件（.rs）。
        #[arg(long, value_name = "FILE")]
        target: Option<String>,
        /// add-feature：测试文件（与 --tests 同给才启用测试步）。
        #[arg(long, value_name = "FILE")]
        tests_file: Option<String>,
        /// add-feature：测试代码全文。
        #[arg(long)]
        tests: Option<String>,
        /// add-test：写模板后跑一次测试后端（add-test 仅支持 .rs 目标）。
        #[arg(long)]
        run: bool,
    },
    /// 代码补全（textDocument/completion）—— AI-friendly 字段裁剪 + 自动推断 trigger。
    /// line/col 为 1-based（与 def/refs 同基线；CLI 层统一转 LSP 0-based）。
    Completion {
        file: String,
        line: u32,
        col: u32,
        /// 上限（0 = 不限，默认 5）。
        #[arg(long, default_value_t = 5)]
        limit: u32,
        /// 显式 trigger char（如 "." / "::"）；省略时按 file 后缀自动推断。
        #[arg(long)]
        trigger: Option<String>,
    },
    /// 按位置反查最深层包含符号（documentSymbol walk）。无命中返空数组（合法）。
    /// line/col 为 1-based。
    ContainingSymbol { file: String, line: u32, col: u32 },
    /// 跳到定义并取完整符号信息（def + documentSymbol walk + body 切片）。
    /// def 返空 → null；定义无符号覆盖 → 空数组；C++ 重载等多定义 → 多元素。
    /// line/col 为 1-based。
    DefiningSymbol { file: String, line: u32, col: u32 },

    /// 函数调用位置的参数签名提示（textDocument/signatureHelp）。无调用位置返 null。
    /// line/col 为 1-based。
    SignatureHelp { file: String, line: u32, col: u32 },
    /// 列出可用 codeAction（textDocument/codeAction）。可选 `--kind` 过滤（如 quickfix / refactor）。
    /// line/col 为 1-based。
    CodeAction {
        file: String,
        line: u32,
        col: u32,
        #[arg(long)]
        kind: Option<String>,
    },
    /// 整文件格式化（textDocument/formatting）。返 edits[]；非编辑端用 `jq` 应用。
    Format {
        file: String,
        #[arg(long)]
        tab_size: Option<u32>,
        #[arg(long)]
        insert_spaces: Option<bool>,
    },
    /// range 内格式化（textDocument/rangeFormatting）。返 edits[]。行/列为 1-based。
    FormatRange {
        file: String,
        start_line: u32,
        start_col: u32,
        end_line: u32,
        end_col: u32,
        #[arg(long)]
        tab_size: Option<u32>,
        #[arg(long)]
        insert_spaces: Option<bool>,
    },
    /// 行范围内类型提示（textDocument/inlayHint）。行号为 1-based。
    InlayHint {
        file: String,
        start_line: u32,
        end_line: u32,
    },
    /// 光标位置的同符号高亮（textDocument/documentHighlight）。line/col 为 1-based。
    DocumentHighlight { file: String, line: u32, col: u32 },
    /// 折叠区（textDocument/foldingRange）。
    FoldingRange { file: String },
    /// 语义 token（textDocument/semanticTokens/full）。
    SemanticTokens { file: String },
    /// 代码透镜（textDocument/codeLens）。
    CodeLens { file: String },
    /// 文档链接（textDocument/documentLink）。
    DocumentLink { file: String },
    /// 调用层级：prepare / incoming / outgoing 三件套（callHierarchy/*）。
    /// prepare 的 line/col 为 1-based。
    CallHierarchy {
        /// "prepare" 用 file/line/col；"incoming"/"outgoing" 用 --item。
        op: String,
        file: Option<String>,
        line: Option<u32>,
        col: Option<u32>,
        /// prepare 返的 CallHierarchyItem JSON（incoming/outgoing 必填）。
        #[arg(long)]
        item: Option<String>,
    },
    /// 类型层级：prepare / supertypes / subtypes 三件套（typeHierarchy/*）。
    /// prepare 的 line/col 为 1-based。
    TypeHierarchy {
        op: String,
        file: Option<String>,
        line: Option<u32>,
        col: Option<u32>,
        #[arg(long)]
        item: Option<String>,
    },
    /// 全局符号标识（textDocument/moniker）。line/col 为 1-based。
    Moniker { file: String, line: u32, col: u32 },
    /// workspace 级 pull diagnostics（workspace/diagnostic）。
    WorkspaceDiagnostic,
    /// 项目元信息：project root + git branch/HEAD + daemon/LS 加载状态。
    /// 纯探测语义（同 status）：永不 lazy-spawn。
    ProjectInfo {
        /// 项目根（缺省顺序：--project > daemon active_project > 当前目录）。
        #[arg(long, value_name = "ROOT")]
        project: Option<PathBuf>,
    },
    /// daemon 状态（uptime / pid / loaded LS）。status 报告的是 daemon 全局状态
    /// —— 与项目根无关；`--project` 仅与 `project-info` 对齐命令面形状
    /// （status --project X 不报错），实际不参与 daemon 报告内容。
    Status {
        /// 项目根（接受但忽略——status 不依赖 project；与 project-info 命令面形状对齐）。
        #[arg(long, value_name = "ROOT")]
        project: Option<PathBuf>,
    },
    /// 变更历史：git log --follow 包装；--symbol 走 `-L :sym:file`
    /// 符号级跟踪。git 缺失 / 非 repo → exit 3 + stderr 原因。
    ChangeHistory {
        /// 仓库内相对路径（git 风格，正斜杠）。
        file: String,
        /// 符号名（函数/方法）；提供时忽略 --follow，走 git -L 符号级历史。
        #[arg(long)]
        symbol: Option<String>,
        /// 最大条数。
        #[arg(long, default_value_t = 20)]
        max: usize,
    },
    /// 停掉 daemon（draining + 删 lock）。
    StopAll,
    /// 安装 servers.toml 配置驱动 LS（PATH 探测 → 下载 → sha256 校验 → 落地缓存）。
    /// 手写 T2 语言（rust/python/...）不在此列——按各 LS 官方方式安装。
    /// `--all` 装全表（幂等——已装即跳过，未装按 ensure_launch 路径装）。
    Install {
        /// 单条 lang 装；`--all` 模式忽略。
        #[arg(default_value = "")]
        lang: String,
        /// 装 servers.toml 全部条目。
        #[arg(long)]
        all: bool,
    },
    /// 卸载 serena 托管的 LS 安装缓存（`{cache_root}/{id}/` 全版本/变体）。
    /// PATH 探测型（path_only/uvx）非 serena 托管，明确拒删、不受影响。
    Uninstall {
        /// lang 或 servers.toml 条目 id。
        lang: String,
        /// 输出 JSON {ok, lang, removed_dirs, bytes_freed}。
        #[arg(long)]
        json: bool,
    },
    // bd serena-rust-4ux（内部追踪号，不入 --help）；oxw0：语言名命中改按二进制名
    /// 智能匹配。注册/覆盖/列出/移除用户自装 LS（写 external-servers.toml）。
    /// 已知 server id → 整条继承内置条目，仅改指你的二进制；已知语言 → 按二进制
    /// 名智能匹配内置 server：唯一命中自动选（回显最终生效 id + 启动命令形态），
    /// 零/多命中拒改并列出候选；未知 id → `--lang <LANG> --ext .<ext>` 注册全新
    /// 语言。注册在 daemon 重启后生效。
    LsUse {
        /// 语言名或 server id（与 <path> 搭配注册/覆盖）。
        #[arg(default_value = "")]
        lang_or_id: String,
        /// LS 二进制路径（Windows .exe/.cmd/.bat；存在 + 可执行校验）。
        path: Option<String>,
        /// 列出 external 注册条目 + 每语言生效来源（注册视角；全量视图用 ls-list）。
        #[arg(long)]
        list: bool,
        /// 全新语言注册：语言名（与 --ext 成对必填）。
        #[arg(long, value_name = "LANG")]
        lang: Option<String>,
        /// 全新语言注册：扩展名（如 .mydsl；与 --lang 成对必填）。
        #[arg(long, value_name = "EXT")]
        ext: Option<String>,
        /// 移除注册条目（其余内容与注释逐字节保留）。
        #[arg(long, value_name = "ID")]
        remove: Option<String>,
    },
    /// 全量 LS 清单（内置 servers.toml 条目 × 实装状态 + external 新语言条目）。
    LsList {
        /// 人类可读表格（默认 JSON）。
        #[arg(long)]
        table: bool,
    },
    /// 卸载 serena 托管缓存里的 LS（只删 `{cache_root}/{id}/`；不碰 PATH/生态安装/
    /// external 注册路径）。未知 id 会列出已装 id。
    LsRemove {
        /// servers.toml 条目 id。
        id: String,
    },
    /// 长连接 shell（stdin/stdout JSONL）。
    ///
    /// 每行 stdin 一个 JSON 请求，响应逐行写 stdout。协议形状：
    ///   {"id":1,"cmd":"find-symbol","args":{"query":"foo","limit":10}}
    ///   {"id":2,"cmd":"status"}
    ///   {"id":3,"cmd":"exit"}
    /// 响应：{"id":<n>,"ok":true,"data":...} 或 {"id":<n>,"ok":false,"error":...}。
    /// 单 daemon 顺序多 project：跨 project 调用会隐式切换 active_project（LS
    /// session 按 project 复用池），响应带 `project switched: A -> B` warning
    /// （同 (from,to) 对 daemon 生命周期内只报一次，bd ts9d）。EOF 或 exit
    /// 请求后退出 0。
    ///
    /// 各 tool 的 args 字段名与 CLI 透传参数同名（project_root 走 shell 全局
    /// `--project`，不重复传）：find-symbol={query,limit?,format?}、read-file=
    /// {file,start_line?,end_line?,clamp?}、refs={file,line,col,depth?} 等。
    /// 完整字段表见 `crates/cli/src/main.rs` 的 `tool_request` 透传表 + 各子
    /// 命令 `--help`。
    Shell,
    /// 环境体检（6 类：运行时 / PATH / 本机 LS / daemon / 网络 / workspace cargo metadata）。
    Doctor {
        /// JSON 输出（默认人类可读）。
        #[arg(long)]
        json: bool,
        /// 尝试自动安装 MISS 的 LS（仅对 servers.toml 已收录的条目）。
        #[arg(long)]
        fix: bool,
    },
    // bd serena-rust-8ot（内部追踪号，不入 --help）
    /// 静态自审即将在 shell 执行的命令串。默认 warn-only
    /// （有 finding 也 exit 0）；`--strict` 下存在 error 级 finding → exit 2。
    LintShell {
        /// 待检查的命令串（与 --cmd-stdin 二选一）。
        #[arg(
            long,
            required_unless_present = "cmd_stdin",
            conflicts_with = "cmd_stdin"
        )]
        cmd: Option<String>,
        /// 从 stdin 读命令串。
        #[arg(long)]
        cmd_stdin: bool,
        /// JSON 输出（findings + summary）。
        #[arg(long)]
        json: bool,
        /// 存在 error 级 finding 时 exit 2（默认恒 0）。
        #[arg(long)]
        strict: bool,
    },
}

fn main() -> ExitCode {
    // Windows 主线程默认栈 1MB：clap derive 的大命令枚举在解析期递归构造/
    // 析构会打爆浅栈（Phase 5 stack overflow）。整个入口搬进 16MB 栈线程；
    // tokio multi_thread runtime 语义不变（属性宏挂在内层 async fn）。
    match std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| build_runtime().block_on(cli_main()))
        .expect("spawn cli main thread")
        .join()
    {
        Ok(code) => code,
        // panic hook 已输出信息；重抛保持原 panic 退出语义（rc=101）。
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

// worker_threads=4（2026-09-23 压测实锤）：2 worker 下 RA 冷启动+分析会把 runtime
// 吃满，HTTP/轮询 task 饿死（post_diag 循环错过推送窗口 → pending 误报）。
// thread_stack_size=16MB（批2-F 实锤）：execute_tool→tool_find_symbol→session_for
// →RA launch/wait_indexing 的 await 深链 poll 帧压爆 tokio worker 默认 2MB 栈
// （`tokio-rt-worker has overflowed its stack`，recipe add-feature 多 lang fixture
// 实锤）——poll 帧深 = await 链长度，Future 层 Box::pin 截不断它，扩线程栈是唯一
// 治本点；多 lang 工具组合的 poll 链只会更深。
fn build_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_stack_size(16 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("tokio runtime (worker_threads=4, stack=16MB)")
}

async fn cli_main() -> ExitCode {
    #[cfg(windows)]
    unsafe {
        let r = windows_sys::Win32::System::Console::SetConsoleOutputCP(65001);
        if r == 0 {
            eprintln!("warning: SetConsoleOutputCP(65001) failed");
        }
    }

    // l5nv：`?query` → find-symbol query；`cmd? …` → cmd …（短输入糖）。
    // 首个位置参数命中 '?' 形态才重写 argv 重解析；其余路径与原生 parse 等价。
    //
    // bd serena-rust-p2zp：07u5 把工具语义错误（normalize_positions / resolve_with_alias /
    // --max-tokens）转 JSON error 对象，但 clap 原生错误（missing subcommand / invalid
    // value / unknown arg）此前走裸文本 e.exit()——agent JSON 解析路径断在第一关。
    // 这里分两臂：use_stderr()=true（真错误）→ 渲染色文本保留人读 + JSON error
    // 对象走 wire 契约（agent 解析路径与 daemon 一致）；use_stderr()=false（help/
    // version）→ 走原生渲染 + exit 0。
    let raw_argv: Vec<String> = std::env::args().skip(1).collect();
    let parse_result = match rewrite_shorthand_argv(raw_argv) {
        Some(argv) => Cli::try_parse_from(std::iter::once("serena-cli".to_string()).chain(argv)),
        None => Cli::try_parse(),
    };
    let mut cli = match parse_result {
        Ok(c) => c,
        Err(e) => return clap_exit_to_json(e),
    };

    // 行号契约统一（bd serena-rust-7xv）：position 型子命令的 line/col 以 1-based
    // 收入，此处一次性就地转 LSP 0-based —— `--direct` 进程内直调与 HTTP 转发两条
    // 路径共用转换结果，supervisor / lsp-core 不感知。0 = 用法错（BAD_ARGS，exit 2）。
    // 杠精 07u5-1：客户端校验错误走与 daemon 同形的 JSON error 对象（stderr 纯文本
    // 「BAD_ARGS: …」对 JSON 解析方不可消费）。
    if let Some(sub) = cli.cmd.as_mut()
        && let Err(detail) = normalize_positions(sub)
    {
        return bad_args_exit(&detail);
    }

    // 杠精 07u5-3：--max-tokens 0 在参数层拒绝（否则截断器把整个响应清空，
    // rc=0 静默空——比报错更伤：AI 无法区分「无结果」和「自己传错了」）。
    if cli.max_tokens == Some(0) {
        return bad_args_exit("--max-tokens must be >= 1 (got 0)");
    }

    // 批3 可发现性：warm 位置参双形态——`warm <LANG>` 与 `warm --lang <LANG>`
    // 等价（三来源二选一归一，8cx5 同款契约）；皆缺才走下方项目清单探测。
    if let Some(Cmd::Warm { lang, lang_flag, .. }) = cli.cmd.as_mut()
        && let Err(detail) = warm_lang_forms(lang, lang_flag, cli.lang.as_deref())
    {
        return bad_args_exit(&detail);
    }

    // bd serena-rust-74b3：warm 缺省 LANG 按项目根清单探测（位置参/`--lang` 皆缺才探测）。
    if let Some(Cmd::Warm { lang, .. }) = cli.cmd.as_mut()
        && lang.is_none()
    {
        let root = resolve_project_root(cli.project.clone());
        match detect_project_lang(&root) {
            Some(detected) => {
                eprintln!("[hint] warm: no --lang given; detected `{detected}` from project manifests");
                *lang = Some(detected.to_string());
            }
            None => {
                eprintln!(
                    "warm: no --lang and no known project manifest (Cargo.toml / pyproject.toml / tsconfig.json / package.json) under {}; pass --lang",
                    root.display()
                );
                return ExitCode::from(2);
            }
        }
    }

    // bd serena-rust-8cx5：单文本写工具 `--with` 别名归一（与 replace-body 对齐）。
    if let Some(sub) = cli.cmd.as_mut()
        && let Err(detail) = resolve_with_alias(sub)
    {
        return bad_args_exit(&detail);
    }

    let lock_path = daemon::serve::default_lock_path();

    // 全库 tracing::warn!/info! 的唯一出口：不 init 则全部静默丢弃（排障全盲）。
    // daemon 分支自持本进程输出；forward/shell/wait-ready 链此前零 subscriber——
    // 冷启动/token 刷新/draining 重试 0 stderr 线索（bd serena-rust-53s）。
    // --direct 除外：进程内 supervisor 的 info 事件量会淹没 CLI stderr。
    // RUST_LOG 控制，默认 info；stderr —— daemon 由 lazy-spawn 时 stdout 已重定向。
    if !cli.direct {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_writer(std::io::stderr)
            .try_init();
    }

    // ---- daemon 模式：本进程做 daemon，阻塞至 shutdown ----
    if cli.daemon {
        let cfg = daemon::serve::ServeConfig {
            lock_path,
            ..Default::default()
        };
        return match daemon::serve::serve(cfg).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("daemon exited with error: {e:#}");
                ExitCode::from(3)
            }
        };
    }

    // ---- 管理命令：只走 lock/HTTP，不需要 project ----
    match &cli.cmd {
        // bd serena-rust-mfht F7：status 接受 --project 与 project-info 命令面
        // 对齐；项目根不参与 daemon 报告内容（cmd_status 忽略 _project）。
        Some(Cmd::Status { project: _ }) => return cmd_status(&lock_path).await,
        // change-history：纯本地 git log 包装（不碰 daemon/LS）。
        Some(Cmd::ChangeHistory {
            file,
            symbol,
            max,
        }) => {
            let project_root = resolve_project_root(cli.project.clone());
            return cmd_change_history(&project_root, file, symbol.as_deref(), *max);
        }
        // project-info：纯探测（读 lock + GET /status + 本地 .git 解析），不碰 LS。
        Some(Cmd::ProjectInfo { project }) => {
            return cmd_project_info(&lock_path, project.clone()).await;
        }
        Some(Cmd::StopAll) => return cmd_stop_all(&lock_path).await,
        // lint-shell：纯本地静态分析，不碰 daemon/lock。
        Some(Cmd::LintShell {
            cmd,
            cmd_stdin,
            json,
            strict,
        }) => {
            let text = if *cmd_stdin {
                match std::io::read_to_string(std::io::stdin()) {
                    Ok(buf) => buf,
                    Err(_) => {
                        eprintln!("lint-shell: failed to read command from stdin");
                        return ExitCode::from(1);
                    }
                }
            } else {
                cmd.clone().unwrap_or_default()
            };
            return ExitCode::from(lint_shell::run(&text, *json, *strict));
        }
        // install 内部自建 blocking runtime（下载），必须在阻塞线程跑。
        Some(Cmd::Install { lang, all }) => {
            if *all {
                return tokio::task::spawn_blocking(cmd_install_all)
                    .await
                    .unwrap_or(ExitCode::from(3));
            }
            if lang.is_empty() {
                eprintln!("install: --lang <ID> or --all required");
                return ExitCode::from(2);
            }
            let lang = lang.clone();
            return tokio::task::spawn_blocking(move || cmd_install(&lang))
                .await
                .unwrap_or(ExitCode::from(3));
        }
        // uninstall 纯本地 fs 操作，同 install 先例走阻塞线程。
        Some(Cmd::Uninstall { lang, json }) => {
            let lang = lang.clone();
            let json = *json;
            return tokio::task::spawn_blocking(move || cmd_uninstall(&lang, json))
                .await
                .unwrap_or(ExitCode::from(3));
        }
        // ls-use / ls-list / ls-remove：纯本地 fs/注册表操作（bd serena-rust-4ux）。
        Some(Cmd::LsUse {
            lang_or_id,
            path,
            list,
            lang,
            ext,
            remove,
        }) => {
            let id = lang_or_id.clone();
            let bin = path.clone();
            let list = *list;
            let new_lang = lang.clone();
            let new_ext = ext.clone();
            let remove = remove.clone();
            return tokio::task::spawn_blocking(move || {
                cmd_ls_use(&id, bin, list, new_lang, new_ext, remove)
            })
            .await
            .unwrap_or(ExitCode::from(3));
        }
        Some(Cmd::LsList { table }) => {
            let table = *table;
            return tokio::task::spawn_blocking(move || cmd_ls_list(table))
                .await
                .unwrap_or(ExitCode::from(3));
        }
        Some(Cmd::LsRemove { id }) => {
            let id = id.clone();
            return tokio::task::spawn_blocking(move || cmd_ls_remove(&id))
                .await
                .unwrap_or(ExitCode::from(3));
        }
        Some(Cmd::Doctor { json, fix }) => {
            let project_root = resolve_project_root(cli.project.clone());
            return cmd_doctor(*json, *fix, &lock_path, &project_root).await;
        }
        // wait-ready（bd serena-rust-55m）：阻塞到就绪（symbol/semantic 档），exit 0/4。
        Some(Cmd::WaitReady {
            file,
            timeout,
            stage,
        }) => return cmd_wait_ready(&cli, file.as_deref(), *timeout, *stage, &lock_path).await,
        Some(Cmd::Shell) => {}
        _ => {}
    }

    // ---- --direct：进程内直调（原 M0 路径）----
    if cli.direct {
        return run_direct(&cli).await;
    }

    // ---- shell：长连接 stdin/stdout JSONL ----
    if matches!(&cli.cmd, Some(Cmd::Shell)) {
        return cmd_shell(&cli).await;
    }
    // ---- 默认：转发模式（lazy-spawn；draining 窗口自愈 g0m）----
    // bd serena-rust-cwt：工具级退出码经 Ok(n) 正常返回（tracing 等 Drop 收尾
    // 有机会 flush），只有传输层失败才落 rc=3。
    match forward_with_draining_retry(&cli, &lock_path).await {
        Ok(0) => ExitCode::SUCCESS,
        Ok(n) => ExitCode::from(n),
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(3)
        }
    }
}

/// M0 --direct 路径。bd serena-rust-kns：与 forward 共用 [`tool_request`] 组装 +
/// [`inject_private_args`] 注入，经 `execute_tool` 走同一 envelope/截断管线
/// —— `--json`/`--max-tokens`/`--compress`/`--delta` 与转发模式同一语义。
async fn run_direct(cli: &Cli) -> ExitCode {
    let Some(root) = cli.project.clone() else {
        eprintln!("--direct requires --project <ROOT>");
        return ExitCode::from(2);
    };
    let root = dunce::canonicalize(&root).unwrap_or(root);
    let Some((tool, mut args)) = tool_request(&cli.cmd) else {
        eprintln!("this subcommand is daemon-mode only in M1");
        return ExitCode::from(2);
    };
    inject_private_args(&mut args, cli);
    let sup = match Supervisor::direct().await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("supervisor init failed: {e:#}");
            return ExitCode::from(1);
        }
    };
    // 用户未传 --lang 时按 file 后缀/shebang/文件名推断；显式 --lang 优先。
    let effective_lang: Option<String> = cli.lang.clone().or_else(|| autodetect_lang(cli));
    match sup
        .execute_tool(
            tool,
            &root.to_string_lossy(),
            args,
            effective_lang.as_deref(),
        )
        .await
    {
        Ok(data) => {
            // O2（bd serena-rust-bxd）对齐 forward：warning 上 stderr，不吞。
            if let Some(w) = data.get("warning").and_then(|v| v.as_str()) {
                eprintln!("[warn] {w}");
            }
            // bd serena-rust-mfht F6：hint 仅在 warning 暗示语义未就绪时打，
            // 项目切换等无关 warning 不再误导 agent「重试/等就绪」。
            if let Some(w) = data.get("warning").and_then(|v| v.as_str())
                && payload_is_empty(&data)
                && warning_suggests_index_warming(w)
            {
                eprintln!(
                    "[hint] index warming: semantic layer not ready, empty result may be false negative (rerun or use wait-ready)"
                );
            }
            match print_json(&data) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("{e}");
                    ExitCode::from(tool_error_exit(&e))
                }
            }
        }
        Err(e) => {
            let exit = tool_error_exit(&e);
            eprintln!("{e}");
            ExitCode::from(exit)
        }
    }
}

fn tool_error_exit(e: &ToolError) -> u8 {
    match e {
        ToolError::BadArgs { .. } => 2,
        ToolError::WriteConflict { .. } | ToolError::NotInstalled { .. } => 1,
        ToolError::Protocol { .. } => 1,
        ToolError::Serialize(_) => 3,
        ToolError::Core(_) | ToolError::Launch(_) => 3,
    }
}

// ==== bd serena-rust-g0m：DAEMON_DRAINING 自愈（纯客户端，与 send_with_connect_retry 正交：
// 那是连接层瞬断重试，这里是 503 语义层识别 + 退避后重试完整链路含 lazy-spawn 重新探活）====

/// stop-all 后 reaper 收尾窗口（数秒）内新请求会打到 draining daemon（503
/// DAEMON_DRAINING）。客户端退避重试总窗：覆盖收尾期，超窗原样报错不无限等。
const DRAINING_RETRY_WINDOW: Duration = Duration::from_secs(5);
const DRAINING_RETRY_BACKOFF: Duration = Duration::from_millis(300);

/// forward 链路失败分类（g0m）：draining 值得等，其余原样上报。
enum ForwardFailure {
    /// 连接失败 / 解码失败 / 非 DRAINING 503 等，重试无意义，消息原样给调用方。
    Fatal(String),
    /// 503 + body error.code=DAEMON_DRAINING：daemon 正在收尾，退避后重试完整链路。
    Draining {
        status: reqwest::StatusCode,
        payload: serde_json::Value,
    },
}

impl From<String> for ForwardFailure {
    fn from(m: String) -> Self {
        Self::Fatal(m)
    }
}

/// 503 + wire 错误码 DAEMON_DRAINING 才触发自愈（区别于其他 503）。
fn is_daemon_draining(status: reqwest::StatusCode, payload: &serde_json::Value) -> bool {
    status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        && payload
            .get("error")
            .and_then(|e| e.get("code"))
            .and_then(|c| c.as_str())
            == Some("DAEMON_DRAINING")
}

/// draining 重试循环（泛型化便于 mock 单测）：窗口内每 backoff 轮重试一次完整
/// 链路（重新探活；旧 daemon 退净 lock 释放即正常 lazy-spawn 新 daemon），
/// 超窗仍 draining 则还原既有错误文本 rc=3。
async fn retry_on_draining<F, Fut, T>(
    mut op: F,
    window: Duration,
    backoff: Duration,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ForwardFailure>>,
{
    let deadline = Instant::now() + window;
    loop {
        match op().await {
            Ok(v) => return Ok(v),
            Err(ForwardFailure::Fatal(m)) => return Err(m),
            Err(ForwardFailure::Draining { status, payload }) => {
                if Instant::now() >= deadline {
                    return Err(format!("daemon transport error {status}: {payload}"));
                }
                tracing::warn!(status = status.as_u16(), "daemon draining; retrying within window");
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

/// forward_or_spawn + draining 自愈包装（g0m）。转发模式的实际入口。
/// Ok = wire 码退出码（0 成功）；Err = 传输层失败文本（main 统一 rc=3）。
async fn forward_with_draining_retry(cli: &Cli, lock_path: &Path) -> Result<u8, String> {
    retry_on_draining(
        || forward_or_spawn(cli, lock_path),
        DRAINING_RETRY_WINDOW,
        DRAINING_RETRY_BACKOFF,
    )
    .await
}

// ==== bd serena-rust-55m：wait-ready ====

/// wait-ready 就绪档位：symbol = 符号索引可用（秒级）；
/// semantic = 类型分析可用（大 workspace 可达 120s+，历史默认判据）；
/// indexing = RA Indexing progress end 真就绪屏障（bd 0vj1，daemon 侧等待收敛）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum WaitStage {
    Symbol,
    Semantic,
    /// def 同层探针：hover 就绪 ≠ def/refs 就绪（hover ready 后 def 仍可能
    /// items:[]）。ready = 语义解析层真可用。
    Def,
    /// Indexing progress end 屏障（bd 0vj1）：判据 = daemon 侧 session_for 的
    /// Indexing 等待收敛（end 到达或分档超时兜底）后首个工具调用成功返回。
    Indexing,
}

/// 读类工具输出档位（find-symbol / search `--format`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum OutFormat {
    Brief,
    Full,
    Json,
}

/// wait-ready 超时上限（秒），默认 120s。
const WAIT_READY_DEFAULT_TIMEOUT_SECS: u64 = 120;

/// 解析 wait-ready 超时：显式 --timeout 优先；否则 SERENA_WAIT_READY_TIMEOUT_SECS
/// （非法值 warn + 用默认，对齐 j8b 的 parse_secs 惯例）；都没有 → 默认。
fn wait_ready_timeout_secs(explicit: Option<u64>, env_raw: Option<&str>) -> u64 {
    if let Some(t) = explicit {
        return t;
    }
    match env_raw.map(str::trim).map(|s| s.parse::<u64>()) {
        Some(Ok(t)) => t,
        None => WAIT_READY_DEFAULT_TIMEOUT_SECS,
        Some(Err(_)) => {
            eprintln!(
                "wait-ready: invalid SERENA_WAIT_READY_TIMEOUT_SECS={env_raw:?}; \
                 using default {WAIT_READY_DEFAULT_TIMEOUT_SECS}s"
            );
            WAIT_READY_DEFAULT_TIMEOUT_SECS
        }
    }
}

/// 探测退避：500ms 起步指数退避，2s 封顶（避免打爆 daemon）。
fn wait_ready_backoff(round: usize) -> Duration {
    Duration::from_millis(500u64 << round.min(2) as u32)
}

/// hover 响应就绪判定（we0 语义的 CLI 侧镜像，`hover_is_empty` + warning 判据）：
/// 带 "may not be ready" warning → 未就绪；空悬停（null / contents 空）→ 未就绪；
/// contents 有内容 → 就绪。
fn hover_ready(data: &serde_json::Value) -> bool {
    let not_ready = data
        .get("warning")
        .and_then(|w| w.as_str())
        .is_some_and(|w| w.contains("may not be ready"));
    if not_ready {
        return false;
    }
    match data.get("contents") {
        Some(serde_json::Value::String(s)) => !s.is_empty(),
        Some(serde_json::Value::Array(a)) => !a.is_empty(),
        Some(c) => c
            .get("value")
            .and_then(|v| v.as_str())
            .is_some_and(|v| !v.is_empty()),
        None => false,
    }
}

/// semantic 探针每轮 hover 候选符号上限：首符号 null 再试后续 1-2 个
/// （bd serena-rust-7m8：csharp-ls 首符号 range=整声明行首 / astro 模板符号 hover 合法 null）。
const SEMANTIC_PROBE_SYMBOLS: usize = 3;

/// def 响应就绪判定（bd serena-rust-y3c1）：items 非空 = 语义解析层可用
/// （与 def/refs 同层——def 空带 we0 warning 或 items:[] 均视为未就绪续等）。
fn def_ready(data: &serde_json::Value) -> bool {
    data.get("warning").is_none()
        && data
            .get("items")
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
}

/// 行内定位符号名的 UTF-16 列（LSP Position.character 契约）；只认**整词**命中
/// （bd serena-rust-gqyp 精化：子串会把 `run` 打进 `running`/参数 `x` 吃进早位），
/// 行内无该名 → None。
fn name_column_in_line(line_text: &str, name: &str) -> Option<u32> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut from = 0usize;
    while let Some(rel) = line_text[from..].find(name) {
        let start = from + rel;
        let end = start + name.len();
        let before = line_text[..start].chars().next_back().is_some_and(is_ident);
        let after = line_text[end..].chars().next().is_some_and(is_ident);
        if !before && !after {
            let col = line_text[..start].chars().map(char::len_utf16).sum::<usize>();
            return Some(col as u32);
        }
        from = end;
    }
    None
}

/// semantic 档 hover 探针候选位置（bd serena-rust-7m8）：overview 符号数组 →
/// 「符号名自身」坐标。每符号 selectionRange.start 优先；无则读探针文件 range.start
/// 起 ≤3 行窗口内找**整词**符号名（bd serena-rust-gqyp 同款精化：pyright fixture
/// 实测 supervisor overview 组装丢弃 selectionRange 恒走 fallback，且 range.start
/// 落在 def/class 声明关键字位时 LS hover 恒空）。行窗口内无该名 / 行越界
/// （模板符号等）→ **丢弃该候选**，绝不退 range.start：行首落在 use/derive/声明
/// 修饰上时 RA 对行首 stdlib token hover 恒空（bd serena-rust-b8sp），假探针会把
/// 「已就绪」永判 pending。
/// 候选偏置：Class/Function/Method/Constructor 优先（语义 hover 最稳，冷窗口
/// 局部变量 hover 常空）；无任何该类符号时不截断全量扫描，避免 import/属性形态
/// 挤掉唯一可打点符号。候选全灭由调用方在进度行点明根因。
fn hover_probe_positions(data: &serde_json::Value, file_text: Option<&str>) -> Vec<(u32, u32)> {
    let Some(symbols) = data.as_array() else {
        return Vec::new();
    };
    let semantic_kind = |s: &serde_json::Value| -> bool {
        matches!(
            s.get("kind").and_then(|k| k.as_str()),
            Some("Class" | "Function" | "Method" | "Constructor")
        )
    };
    let mut ordered: Vec<&serde_json::Value> = symbols.iter().collect();
    // 稳定排序：语义符号提前，其余保原序；全无语义符号 → 全量（不截断）。
    ordered.sort_by_key(|s| !semantic_kind(s));
    let take = if ordered.iter().any(|s| semantic_kind(s)) {
        SEMANTIC_PROBE_SYMBOLS
    } else {
        ordered.len()
    };
    let lines: Vec<&str> = file_text
        .map(|t| t.split('\n').collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    for sym in ordered.into_iter().take(take) {
        let sel_start = sym.get("selectionRange").and_then(|r| r.get("start"));
        if let Some((line, col)) = sel_start.and_then(|s| {
            Some((
                s.get("line")?.as_u64()? as u32,
                s.get("character")?.as_u64()? as u32,
            ))
        }) {
            out.push((line, col));
            continue;
        }
        let name = sym.get("name").and_then(|n| n.as_str());
        let range_start = sym.get("range").and_then(|r| r.get("start"));
        let (Some(name), Some(range_start)) = (name, range_start) else {
            continue;
        };
        let Some(start_line) = range_start.get("line").and_then(|v| v.as_u64()) else {
            continue;
        };
        for off in 0..3u64 {
            let Some(l) = lines.get((start_line + off) as usize) else {
                break;
            };
            let l = l.strip_suffix('\r').unwrap_or(l);
            if let Some(col) = name_column_in_line(l, name) {
                out.push(((start_line + off) as u32, col));
                break;
            }
        }
    }
    out
}

/// 默认探测目标：项目内首个源文件（扩展名经 ls-registry 识别即算）。
/// 浅深度优先（深度 ≤4），跳过 VCS/构建/依赖目录；找不到返回 None。
/// bd serena-rust-tjlm：清单/配置类"非源码"（toml/json/yaml 等）默认排除——
/// 它们是已注册语言但常排在真源码之前（Cargo.toml < src/*.rs），对应 LS 未装时
/// wait-ready 默认探针会陷入 LS_NOT_INSTALLED 死循环；`--file` 显式指定不受限。
fn find_first_source_file(root: &Path) -> Option<PathBuf> {
    const SKIP: [&str; 10] = [
        ".git",
        "target",
        "node_modules",
        ".venv",
        "venv",
        "dist",
        "build",
        "__pycache__",
        ".idea",
        ".vscode",
    ];
    fn probeable(p: &Path) -> bool {
        const SKIP_EXTS: [&str; 7] = ["toml", "json", "yaml", "yml", "lock", "ini", "cfg"];
        p.extension()
            .and_then(|e| e.to_str())
            .map(|e| !SKIP_EXTS.contains(&e.to_ascii_lowercase().as_str()))
            .unwrap_or(true)
    }
    fn walk(dir: &Path, depth: u8) -> Option<PathBuf> {
        if depth > 4 {
            return None;
        }
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .ok()?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !SKIP.contains(&name.as_str())
                    && let Some(hit) = walk(&p, depth + 1)
                {
                    return Some(hit);
                }
            } else if probeable(&p) && ls_registry::file_detect::detect_language(&p).is_some() {
                return Some(p);
            }
        }
        None
    }
    walk(root, 0)
}

/// wait-ready 单次探测工具调用：POST /tools/{tool}，返回 wire data。
async fn probe_tool_call(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    root: &Path,
    tool: &str,
    args: serde_json::Value,
    lang: Option<&str>,
) -> Result<serde_json::Value, String> {
    let body = json!({
        "project_root": root.to_string_lossy(),
        "args": args,
        "lang": lang,
    });
    let resp = client
        .post(format!("{base}/tools/{tool}"))
        .header("X-Serena-Token", token)
        .json(&body)
        .timeout(FORWARD_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("{tool}: {e}"))?;
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    if !status.is_success() {
        return Err(format!("transport {status}: {payload}"));
    }
    match payload.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => Ok(payload
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null)),
        _ => Err(payload.get("error").cloned().unwrap_or(payload).to_string()),
    }
}

/// probe_tool_call 折叠出的错误串里的 wire code（ok:false → error JSON 的 code）；
/// 非 JSON（transport 5xx / decode）→ None = 瞬态。
fn wire_err_code(err: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(err)
        .ok()?
        .get("code")?
        .as_str()
        .map(str::to_string)
}

/// wait-ready 探针遇确定性错误（LS_NOT_INSTALLED，静态可判）→ 打印 + fail-fast；
/// None = 瞬态错误，继续等。bd serena-rust-nqjo：未装 LS 时无限 pending 挂满
/// 超时窗，而同刻普通工具调用一发即报 LS_NOT_INSTALLED。
fn not_installed_exit(e: &str, lang: Option<&str>) -> Option<ExitCode> {
    if wire_err_code(e).as_deref() != Some("LS_NOT_INSTALLED") {
        return None;
    }
    eprintln!(
        "wait-ready: language server not installed (lang {}); {e}",
        lang.unwrap_or("(auto)")
    );
    eprintln!("hint: run `serena-cli install <lang>` (or `serena-cli doctor --fix`), then retry");
    Some(ExitCode::from(1))
}

/// l5nv：短输入糖——argv 首个位置参数 `?query` → `find-symbol query`（后续 token
/// 不动，如 `?clamp --format brief`），`cmd?` → `cmd`（剥尾 '?'，如 `list-dir? crates`）。
/// 返回 None = 无需重写（原样 parse）。取值型全局旗的值不算位置参数；
/// `--flag=value` 同 token 带值；`--` 终结符后不做糖。
fn rewrite_shorthand_argv(mut argv: Vec<String>) -> Option<Vec<String>> {
    const VALUE_FLAGS: [&str; 6] = [
        "--project",
        "--lang",
        "--request-timeout",
        "--index-timeout",
        "--max-tokens",
        "--invocation-id",
    ];
    let mut i = 0;
    while i < argv.len() {
        let tok = argv[i].clone();
        if tok == "--" {
            return None;
        }
        if tok.starts_with('-') && tok.len() > 1 {
            // 未知旗跳自身即可（未知旗 clap 报错，与糖无关）；取值旗连值一起跳。
            i += if VALUE_FLAGS.contains(&tok.as_str()) {
                2
            } else {
                1
            };
            continue;
        }
        if tok.len() > 1 && tok.starts_with('?') {
            argv.splice(i..i + 1, vec!["find-symbol".into(), tok[1..].to_string()]);
            return Some(argv);
        }
        if tok.len() > 1 && tok.ends_with('?') {
            argv[i] = tok.trim_end_matches('?').to_string();
            return Some(argv);
        }
        return None;
    }
    None
}

/// `wait-ready` 子命令（bd serena-rust-55m / bxd）：阻塞到所选档位就绪。
/// 两段探测：overview 首符号非空（符号索引起）→ semantic 档再逐符号 hover 其
/// 符号名坐标（selectionRange / range 内 name 文本定位；bd serena-rust-7m8），
/// contents 非空即就绪（类型分析起；未就绪响应带 we0 warning，`hover_ready`
/// 解析即判据）。就绪 exit 0（stderr 'ready in NNs'）；超时 exit 4。进度行带阶段
/// （`probe #N symbol-pending` / `probe #N symbol-ok hover-pending`）。
async fn cmd_wait_ready(
    cli: &Cli,
    file: Option<&str>,
    timeout: Option<u64>,
    stage: WaitStage,
    lock_path: &Path,
) -> ExitCode {
    let timeout_secs = wait_ready_timeout_secs(
        timeout,
        std::env::var("SERENA_WAIT_READY_TIMEOUT_SECS")
            .ok()
            .as_deref(),
    );
    let root = resolve_project_root(cli.project.clone());
    let probe_path = match file {
        Some(f) => PathBuf::from(f),
        None => match find_first_source_file(&root) {
            Some(p) => p,
            None => {
                eprintln!(
                    "wait-ready: no source file under {}; pass --file <FILE>",
                    root.display()
                );
                return ExitCode::from(2);
            }
        },
    };
    // 工具 contract 与其他子命令一致：相对项目根的路径。
    let rel = probe_path
        .strip_prefix(&root)
        .unwrap_or(&probe_path)
        .to_string_lossy()
        .to_string();
    let lang = cli.lang.clone().or_else(|| {
        ls_registry::file_detect::detect_language(&probe_path).map(|l| l.as_str().to_string())
    });

    // daemon 未起时先 lazy-spawn（与转发模式同一条就绪链路）。
    let (mut base, mut token) = match ensure_daemon(lock_path).await {
        Ok(v) => v,
        Err(e) => {
            eprintln!("wait-ready: {e}");
            return ExitCode::from(3);
        }
    };
    let client = http_client();
    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout_secs);
    let mut round = 0usize;
    // bd fakewait：探针失败后下一轮重跑 ensure_daemon——stop-all 发生在循环
    // 中途（或 ensure 之后就撞上）时，不重探活会拿死 base 空转满超时窗。
    let mut reensure = false;
    loop {
        if reensure {
            reensure = false;
            match ensure_daemon(lock_path).await {
                Ok((b, t)) => {
                    base = b;
                    token = t;
                }
                Err(e) => eprintln!("wait-ready: re-ensure failed (keep waiting): {e}"),
            }
        }
        // 段 1：符号索引——overview 首符号位置（LSP 0-based，wire 契约直接透传）。
        let overview = probe_tool_call(
            &client,
            &base,
            &token,
            &root,
            "overview",
            json!({"file": rel}),
            lang.as_deref(),
        )
        .await;
        if let Err(e) = &overview {
            if let Some(code) = not_installed_exit(e, lang.as_deref()) {
                ls_env_mismatch_hint(lang.as_deref(), &root).await;
                return code;
            }
            // 7m8 观测补口：.ok() 静默吞错会让「恒 symbol-pending」无法与「真未就绪」
            // 区分（实例：token 失配 403 / LS spawn 失败被误读为索引未就绪）。
            eprintln!("probe overview error (keep waiting): {e}");
            reensure = true;
        }
        let overview = overview.ok();
        let symbol_up = overview.as_ref().and_then(|data| {
            data.get(0)
                .and_then(|h| h.get("range"))
                .and_then(|r| r.get("start"))
                .and_then(|s| {
                    Some((
                        s.get("line")?.as_u64()? as u32,
                        s.get("character")?.as_u64()? as u32,
                    ))
                })
        });
        let mut probe_count = 0usize;
        if stage == WaitStage::Indexing {
            // bd 0vj1：探针 = WaitIndexing end。daemon 侧 session_for 阻塞至 Indexing
            // 等待收敛（end 到达 / 分档超时 + documentSymbol 兜底），本调用返回即屏障
            // 已过 —— 载荷不判形，就绪语义在服务端等待而非响应内容。
            if overview.is_some() {
                eprintln!("ready (indexing) in {}s", started.elapsed().as_secs());
                return ExitCode::SUCCESS;
            }
        } else if symbol_up.is_some() {
            if stage == WaitStage::Symbol {
                eprintln!("ready (symbol) in {}s", started.elapsed().as_secs());
                return ExitCode::SUCCESS;
            }
            // 段 2：类型分析——标识符偏移探针（bd serena-rust-7m8）：hover 逐符号打在
            // 符号名起点（selectionRange / range 内 name 文本定位；首符号 null 再试后续
            // —— csharp-ls range.start=行首 / astro 模板符号 hover 合法 null）。
            // we0 warning / 全候选未就绪 → pending 续等；探测期瞬态 ≠ 确认就绪，
            // 等待语义不做硬失败，超时判据不变。
            // b8sp：探针文件必须按项目根拼绝对路径读——`--file src/main.rs` 是相对
            // 路径时 read_to_string 相对 CWD 解析，cwd ≠ 项目根 → 读空 → name-in-line
            // 全灭 → 7m8 探针静默退化成 range.start 行首假探针（hover 恒空）。
            let file_text = std::fs::read_to_string(if probe_path.is_absolute() {
                probe_path.clone()
            } else {
                root.join(&probe_path)
            })
            .ok();
            let positions = overview
                .as_ref()
                .map(|data| hover_probe_positions(data, file_text.as_deref()))
                .unwrap_or_default();
            probe_count = positions.len();
            for (line, col) in positions {
                if stage == WaitStage::Def {
                    // bd serena-rust-y3c1：def 同层探针——F2 实锤 hover ready 0s 后
                    // def 仍 items:[]（两能力不同层）。就绪判据直接用 def 非空，
                    // ready 承诺 = def/refs 可开干。
                    match probe_tool_call(
                        &client,
                        &base,
                        &token,
                        &root,
                        "def",
                        json!({"file": rel, "line": line, "col": col}),
                        lang.as_deref(),
                    )
                    .await
                    {
                        Ok(data) if def_ready(&data) => {
                            eprintln!("ready (def) in {}s", started.elapsed().as_secs());
                            return ExitCode::SUCCESS;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            if let Some(code) = not_installed_exit(&e, lang.as_deref()) {
                                ls_env_mismatch_hint(lang.as_deref(), &root).await;
                                return code;
                            }
                            eprintln!("probe def error (keep waiting): {e}");
                            reensure = true;
                        }
                    }
                    continue;
                }
                match probe_tool_call(
                    &client,
                    &base,
                    &token,
                    &root,
                    "hover",
                    json!({"file": rel, "line": line, "col": col}),
                    lang.as_deref(),
                )
                .await
                {
                    Ok(data) if hover_ready(&data) => {
                        eprintln!("ready in {}s", started.elapsed().as_secs());
                        return ExitCode::SUCCESS;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        if let Some(code) = not_installed_exit(&e, lang.as_deref()) {
                            ls_env_mismatch_hint(lang.as_deref(), &root).await;
                            return code;
                        }
                        eprintln!("probe error (keep waiting): {e}");
                        reensure = true;
                    }
                }
            }
        } else if stage == WaitStage::Symbol {
            // oab：RA 大项目首文件 documentSymbol 可能仍在爬升而全局符号索引已起
            // —— overview 空 ≠ 符号层未就绪（假阴性死等）。find-symbol(探针文件
            // 词干) 兜底判据（b8sp 同思路：判据素材取自探针文件自身，不依赖单文件
            // documentSymbol）。0 命中/出错 = 继续等，不造假阳性。
            let stem = probe_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if !stem.is_empty() {
                match probe_tool_call(
                    &client,
                    &base,
                    &token,
                    &root,
                    "find-symbol",
                    json!({"query": stem, "limit": 1}),
                    lang.as_deref(),
                )
                .await
                {
                    Ok(data)
                        if data
                            .get("raw_count")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0)
                            > 0 =>
                    {
                        eprintln!(
                            "ready (symbol, find-symbol `{stem}`) in {}s",
                            started.elapsed().as_secs()
                        );
                        return ExitCode::SUCCESS;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        if let Some(code) = not_installed_exit(&e, lang.as_deref()) {
                            ls_env_mismatch_hint(lang.as_deref(), &root).await;
                            return code;
                        }
                        eprintln!("probe find-symbol error (keep waiting): {e}");
                        reensure = true;
                    }
                }
            }
        }
        let progress = wait_ready_progress(stage, symbol_up.is_some(), probe_count, &rel);
        if Instant::now() >= deadline {
            // bd fdmj-F5：超时消息带最后一次探测的具体症状（等哪层 pending /
            // 探针有无落点），不再只给裸秒数让用户盲猜「再等等还是坏了」。
            eprintln!("wait-ready: not ready within {timeout_secs}s; last probe: {progress}");
            // bd serena-rust-y3c1：探针可能比实际工作负载更严（documentSymbol 层
            // 与语义解析层就绪节奏因 LS 而异）——超时不封死开工路，给降级指引。
            eprintln!("hint: documentSymbol-layer tools (overview / find-referencing-code-snippets / symbol-body) may already work; if a semantic tool returns empty, its warning field carries degraded-mode guidance");
            return ExitCode::from(4);
        }
        // bd fdmj-F5：进度行带累计等待秒数，与超时预算对照可见，盲等变可估。
        eprintln!(
            "wait-ready: probe #{round} {progress} (elapsed {}s/{timeout_secs}s)",
            started.elapsed().as_secs()
        );
        tokio::time::sleep(wait_ready_backoff(round)).await;
        round += 1;
    }
}

/// bd fdmj-F5：进度行按等待档消歧——等哪层打哪层 pending，不再一律带 symbol-ok
/// 前缀（documentSymbol 就绪对 semantic 等待者是前置态而非目标态，原形态
/// "symbol-ok hover-pending" 让超时日志看起来像有进展）。b8sp 的「候选全灭
/// 点明根因」文案保留；symbol/indexing 档语义不动（symbol-ok 即目标态）；
/// def 档的 pending 字样与实际探针对齐（原误打 hover-pending）。
fn wait_ready_progress(stage: WaitStage, symbol_up: bool, probe_count: usize, rel: &str) -> String {
    match (symbol_up, stage) {
        (false, _) => "symbol-pending".to_string(),
        (true, WaitStage::Semantic) if probe_count == 0 => {
            format!("hover-pending (no probeable identifier in {rel})")
        }
        (true, WaitStage::Semantic) => "hover-pending".to_string(),
        (true, WaitStage::Def) if probe_count == 0 => {
            format!("def-pending (no probeable identifier in {rel})")
        }
        (true, WaitStage::Def) => "def-pending".to_string(),
        // 逻辑上不可达（symbol_up 即 return），穷尽性兜底保持符号层语义。
        (true, WaitStage::Indexing | WaitStage::Symbol) => "symbol-ok".to_string(),
    }
}

/// 转发模式：探活 → 转发；死 lock → lazy-spawn --daemon → 轮询就绪 → 转发。
async fn forward_or_spawn(cli: &Cli, lock_path: &Path) -> Result<u8, ForwardFailure> {
    let entry = daemon::lockfile::read(lock_path).map_err(|e| format!("read lock: {e}"))?;
    let base = match entry {
        Some(e) if daemon::lockfile::is_alive_graceful(e.port) => {
            // bd fakewait：与 ensure_daemon 同一判定——drain 窗口内老 daemon
            // listener 仍 accept 但工具请求必 503，等退净再接管；否则 g0m 的
            // 5s 重试窗 < 15s drain 窗，0 间隔工具命令仍会 rc=3。
            if alive_but_draining(&e).await {
                match wait_drain_outcome(lock_path, e.port, DRAIN_TAKEOVER_WAIT)
                    .await
                    .map_err(ForwardFailure::from)?
                {
                    DrainOutcome::Attach(e2) => format!("http://127.0.0.1:{}", e2.port),
                    DrainOutcome::ReadyToSpawn => {
                        spawn_and_adopt(lock_path)
                            .await
                            .map_err(ForwardFailure::from)?
                    }
                }
            } else {
                format!("http://127.0.0.1:{}", e.port)
            }
        }
        _ => spawn_and_adopt(lock_path)
            .await
            .map_err(ForwardFailure::from)?,
    };

    let mut token = read_token_with_retry(lock_path).await?;

    // 用户未传 --lang 时按 file 后缀/shebang/文件名推断；显式 --lang 优先。
    let effective_lang = cli.lang.clone().or_else(|| autodetect_lang(cli));
    forward(cli, &base, &mut token, lock_path, effective_lang.as_deref()).await
}

/// 读 lock token；lock 瞬时缺失（bind 赢家尚未写回）时短暂重试，
/// 仍缺失则报错——绝不带空 token 转发（必 403 且掩盖真实状态，bd y2y）。
async fn read_token_with_retry(lock_path: &Path) -> Result<String, String> {
    for _ in 0..10 {
        match daemon::lockfile::read(lock_path) {
            Ok(Some(e)) => return Ok(e.token),
            Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => return Err(format!("read lock: {e}")),
        }
    }
    Err("daemon is serving but lock is missing; retry the command".into())
}

/// 403 自愈：stop-all × lazy-spawn 交叉时 daemon 会换代，缓存 token 随旧
/// daemon 一起失效——重读 lock 取新 token。token 确实变了返回 `Some(新token)`
/// （调用方应更新缓存并重发一次），否则 `None`（403 另有原因，如实上报）。
async fn refresh_token_if_stale(lock_path: &Path, current: &str) -> Option<String> {
    let fresh = read_token_with_retry(lock_path).await.ok()?;
    if fresh != current { Some(fresh) } else { None }
}

/// 从子命令的第一个 file 形参（Pos 0）推断 LanguageId（仅当用户未传 --lang）。
/// 复用 ls-registry::file_detect::detect_language（ext → shebang → filename 三层）。
/// 推断失败 → None（保留现状：缺 lang 时 supervisor 走默认 / 报错）。
fn autodetect_lang(cli: &Cli) -> Option<String> {
    if cli.lang.is_some() {
        return None; // 显式 --lang 优先
    }
    // 取第一个 file 形参：overview / def / refs / hover / diagnostics / read-file /
    // find-implementations / rename-symbol / find-referencing-symbols /
    // find-referencing-code-snippets / symbol-body / completion / containing-symbol /
    // defining-symbol / signature-help / code-action / format / format-range /
    // inlay-hint / document-highlight / folding-range / semantic-tokens / code-lens /
    // document-link / call-hierarchy(prepare) / type-hierarchy(prepare) / moniker /
    // search(pattern but uses pattern, not file) / insert-text-*-symbol / safe-delete-symbol /
    // replace-body / replace-text-in-symbol / delete-text-in-symbol / replace-lines /
    // delete-lines / insert-at-line
    let file_arg: Option<&str> = match &cli.cmd {
        Some(Cmd::Overview { file, .. }) => Some(file),
        Some(Cmd::Def { file, .. }) => Some(file),
        Some(Cmd::Refs { file, .. }) => Some(file),
        Some(Cmd::Hover { file, .. }) => Some(file),
        Some(Cmd::Diagnostics { file, .. }) => Some(file),
        Some(Cmd::ReadFile { file, .. }) => Some(file),
        Some(Cmd::FindImplementations { file, .. }) => Some(file),
        Some(Cmd::RenameSymbol { file, .. }) => Some(file),
        Some(Cmd::FindReferencingSymbols { file, .. }) => Some(file),
        Some(Cmd::FindReferencingCodeSnippets { file, .. }) => file.as_deref(),
        Some(Cmd::SymbolBody { file, .. }) => Some(file),
        Some(Cmd::EditContext { file, .. }) => Some(file),
        Some(Cmd::Completion { file, .. }) => Some(file),
        Some(Cmd::ContainingSymbol { file, .. }) => Some(file),
        Some(Cmd::DefiningSymbol { file, .. }) => Some(file),
        Some(Cmd::SignatureHelp { file, .. }) => Some(file),
        Some(Cmd::CodeAction { file, .. }) => Some(file),
        Some(Cmd::Format { file, .. }) => Some(file),
        Some(Cmd::FormatRange { file, .. }) => Some(file),
        Some(Cmd::InlayHint { file, .. }) => Some(file),
        Some(Cmd::DocumentHighlight { file, .. }) => Some(file),
        Some(Cmd::FoldingRange { file }) => Some(file),
        Some(Cmd::SemanticTokens { file }) => Some(file),
        Some(Cmd::CodeLens { file }) => Some(file),
        Some(Cmd::DocumentLink { file }) => Some(file),
        Some(Cmd::Moniker { file, .. }) => Some(file),
        Some(Cmd::SafeDeleteSymbol { file, .. }) => Some(file),
        Some(Cmd::ReplaceBody { file, .. }) => Some(file),
        Some(Cmd::ReplaceTextInSymbol { file, .. }) => Some(file),
        Some(Cmd::InsertTextBeforeSymbol { file, .. }) => Some(file),
        Some(Cmd::InsertTextAfterSymbol { file, .. }) => Some(file),
        Some(Cmd::DeleteTextInSymbol { file, .. }) => Some(file),
        Some(Cmd::InsertAtLine { file, .. }) => Some(file),
        Some(Cmd::ReplaceLines { file, .. }) => Some(file),
        Some(Cmd::DeleteLines { file, .. }) => Some(file),
        // call-hierarchy/type-hierarchy 的 prepare 模式才有 file
        Some(Cmd::CallHierarchy { op, file, .. }) if op == "prepare" => file.as_deref(),
        Some(Cmd::TypeHierarchy { op, file, .. }) if op == "prepare" => file.as_deref(),
        _ => None,
    };
    let file = file_arg?;
    let path = std::path::Path::new(file);
    // 内置三层探测（扩展名/shebang/文件名）→ external-servers.toml extensions 兜底。
    ls_registry::file_detect::detect_language(path)
        .map(|l| l.as_str().to_string())
        .or_else(|| ls_registry::resolve_lang_name(path).map(str::to_string))
}

/// Windows：CREATE_NO_WINDOW + CREATE_NEW_PROCESS_GROUP + 句柄不继承 + stdio→NULL。
///
/// 返回 (port, child_pid)：调用方需要子进程 PID 在 wait_ready 之后做归属反查
/// （bd dbx1：3 并发 lazy-spawn 时 OS bind 排他只能 1 赢；2 个败家子进程 bind
/// 失败立即退，但都连到胜家 listener 的 :7860 拿到 200，无 pid 反查会全部误
/// 报"已 spawn" → 日志与状态全部错配）。
fn spawn_daemon_child() -> Result<(u16, u32), String> {
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut cmd = Command::new(exe);
    cmd.arg("--daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    // stderr：默认 NULL（后台守护零输出语义不变）；SERENA_DAEMON_LOG=路径 → daemon
    // tracing（含 lsp_stderr 中继——LS 瞬死死因此前落 NULL 不可见，smoke R4 观测契约）
    // 落该文件（append）。smoke 每门 export 一次实现 per-door 归属。
    if let Some(file) = daemon_log_file() {
        cmd.stderr(Stdio::from(file));
    } else {
        cmd.stderr(Stdio::null());
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(daemon_creation_flags());
        // ponytail: DETACHED_PROCESS 让父进程退出不影响子进程 —— 缺这个 daemon 退随父 CLI。
        // bd vjm：stdio→NULL 只换掉子进程的 std 槽位，管不住 bInheritHandles=TRUE 的
        // 父进程全句柄表继承——subprocess capture 场景下 python 管道写端（本进程的
        // std out/err）被 daemon 继承，CLI 退出后管道不 EOF，父端 read()/communicate()
        // 死等且 python timeout kill 掉 CLI 后仍死等（timeout 失效）。spawn 前清掉
        // stdio 句柄的继承位，继承表里不再出现这些句柄；本进程自己读写不受影响。
        detach_stdio_inheritance();
    }
    let child = cmd.spawn().map_err(|e| format!("spawn daemon: {e}"))?;
    Ok((7860, child.id())) // M1 固定端口；M2 起 OS 分配 + lock 回填
}

/// bd dbx1：lock.pid 反查归属——胜家 = spawn_daemon_child 返回的子进程 PID。
/// 抽纯函数便于单测（写 temp lockfile + 断言）。
fn own_child_won(lock_path: &Path, spawned_pid: u32) -> bool {
    daemon::lockfile::read(lock_path)
        .ok()
        .flatten()
        .map(|e| e.pid == spawned_pid)
        .unwrap_or(false)
}

/// SERENA_DAEMON_LOG 排障钩子：设了 env 就打开该文件（append）供 daemon stderr 落盘；
/// 未设或打开失败返 None（后者先打 warn 到本进程 stderr——调用方可见，不静默吞）。
fn daemon_log_file() -> Option<std::fs::File> {
    let path = std::env::var("SERENA_DAEMON_LOG").ok()?;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => Some(f),
        Err(e) => {
            eprintln!("[warn] SERENA_DAEMON_LOG={path} open failed: {e}; daemon stderr -> null");
            None
        }
    }
}

/// lazy-spawn daemon 的进程创建 flags（独立成纯函数便于单测断言配置）。
#[cfg(windows)]
fn daemon_creation_flags() -> u32 {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP
}

/// 清当前进程 stdio 句柄的继承位：只影响此后 spawn 的子进程能否继承，本进程读写不受影响。
/// 句柄无效（无控制台场景）时静默跳过——此时本就无可泄漏的管道。
#[cfg(windows)]
fn detach_stdio_inheritance() {
    use windows_sys::Win32::Foundation::{
        HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for slot in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let h = unsafe { GetStdHandle(slot) };
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

/// TCP 探活。
fn probe(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(500),
    )
    .is_ok()
}

/// 轮询 /status 直到就绪。
async fn wait_ready(port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let url = format!("http://127.0.0.1:{port}/status");
    let client = http_client();
    let mut token: Option<String> = None;
    while Instant::now() < deadline {
        // 抓 token：spawn 后 daemon 写 lock 通常几 ms 内完成。
        if token.is_none()
            && let Some(e) = daemon::lockfile::read(&daemon::serve::default_lock_path())
                .ok()
                .flatten()
        {
            token = Some(e.token);
        }
        let mut req = client.get(&url).timeout(Duration::from_millis(500));
        if let Some(t) = &token {
            req = req.header("X-Serena-Token", t);
        }
        if let Ok(resp) = req.send().await
            && resp.status().is_success()
        {
            // bd fakewait：drain 窗口内老 daemon 的 /status 也是 200——spawn 链
            // 等子进程 bind 时会拿老 daemon 误判就绪。draining:true 继续轮询。
            // 非 JSON 200 按旧语义判就绪（保守不倒退）。
            let draining = resp
                .json::<serde_json::Value>()
                .await
                .ok()
                .and_then(|b| b.get("draining").and_then(|d| d.as_bool()))
                .unwrap_or(false);
            if !draining {
                return Ok(());
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!(
        "daemon on :{port} not ready within {timeout:?}; cold start in progress: \
         retry, or run wait-ready --stage symbol first"
    ))
}

/// connect 类瞬断退避重试：首发起发 + `CONNECT_BACKOFF_MS` 各一轮，共 5 发。
/// 仅对 `transient(e)` 为真的错误重试——连接未建立 = 请求未出网，重发无重复执行风险；
/// HTTP 4xx/5xx、daemon 工具错误（有响应即语义结果）一律不重试。超时/解码错误
/// 不在 `transient` 判定内（请求可能已到达 daemon，重发写类工具 = 重复执行）。
async fn send_with_connect_retry<T, E, F, Fut>(
    mut send: F,
    transient: fn(&E) -> bool,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    match send().await {
        Ok(v) => return Ok(v),
        Err(e) if !transient(&e) => return Err(e),
        Err(_) => {}
    }
    let last = CONNECT_BACKOFF_MS.len() - 1;
    for (i, ms) in CONNECT_BACKOFF_MS.iter().enumerate() {
        tokio::time::sleep(Duration::from_millis(*ms)).await;
        match send().await {
            Ok(v) => return Ok(v),
            // 末轮失败不再续期，如实上报。
            Err(e) if transient(&e) && i < last => {}
            Err(e) => return Err(e),
        }
    }
    unreachable!("last backoff round returns in-loop")
}

/// 子命令 → (工具名, wire args)。forward/--direct 共用同一组装（bd serena-rust-kns：
/// 两条路径同一 envelope/截断管线）。管理命令与 None 在 cli_main 已提前分流：
/// forward 侧 expect panic（原 unreachable! 语义）；--direct 侧照旧报 daemon-mode only。
fn tool_request(cmd: &Option<Cmd>) -> Option<(&'static str, serde_json::Value)> {
    Some(match cmd {
        // 本地管理命令已在 main 提前 return；到达此处即编程错误。
        Some(Cmd::Overview { file, .. }) => ("overview", json!({"file": file})),
        Some(Cmd::SymbolTree {
            dir,
            max_files,
            grep,
            max_depth,
            files_only,
        }) => {
            // 6ooi：三开关缺省不传（wire args 零新键，与默认行为逐字节一致）。
            let mut a = json!({"dir": dir, "max_files": max_files});
            if let Some(g) = grep {
                a["grep"] = json!(g);
            }
            if let Some(d) = max_depth {
                a["max_depth"] = json!(d);
            }
            if *files_only {
                a["files_only"] = json!(true);
            }
            ("symbol-tree", a)
        }
        Some(Cmd::Def { file, line, col }) => {
            ("def", json!({"file": file, "line": line, "col": col}))
        }
        Some(Cmd::Refs {
            file, line, col, ..
        }) => ("refs", json!({"file": file, "line": line, "col": col})),
        Some(Cmd::Hover { file, line, col }) => {
            ("hover", json!({"file": file, "line": line, "col": col}))
        }
        Some(Cmd::Diagnostics { file, wait_gen }) => {
            ("diagnostics", json!({"file": file, "wait_gen": wait_gen}))
        }
        Some(Cmd::FindSymbol {
            query,
            limit,
            format,
            ..
        }) => (
            "find-symbol",
            json!({"query": query, "limit": limit, "format": format}),
        ),
        Some(Cmd::FindImplementations {
            file, line, col, ..
        }) => (
            "find-implementations",
            json!({"file": file, "line": line, "col": col}),
        ),
        Some(Cmd::RenameSymbol {
            file,
            line,
            col,
            new_name,
        }) => (
            "rename-symbol",
            json!({"file": file, "line": line, "col": col, "new_name": new_name}),
        ),
        Some(Cmd::Search {
            pattern,
            path_glob,
            max_results,
            comments_only,
            case_sensitive,
            distinct_symbols,
            exclude,
            no_ignore,
            format,
        }) => (
            "search",
            json!({
                "pattern": pattern,
                "path_glob": path_glob,
                "max_results": max_results,
                "comments_only": comments_only,
                "case_sensitive": case_sensitive,
                "distinct_symbols": distinct_symbols,
                "exclude": exclude,
                "no_ignore": no_ignore,
                "format": format,
            }),
        ),
        Some(Cmd::ReadFile {
            file,
            start_line,
            end_line,
            max_tokens,
            ..
        }) => (
            "read-file",
            json!({
                "file": file,
                "start_line": start_line,
                "end_line": end_line,
                "max_tokens": max_tokens,
            }),
        ),
        Some(Cmd::ListDir { path }) => ("list-dir", json!({"path": path})),
        Some(Cmd::FindFile { name_pattern }) => {
            ("find-file", json!({"name_pattern": name_pattern}))
        }
        Some(Cmd::FindReferencingSymbols {
            file,
            line,
            col,
            grouped,
            page,
            page_size,
            debug_raw,
        }) => (
            "find-referencing-symbols",
            json!({
                "file": file,
                "line": line,
                "col": col,
                "grouped": grouped,
                "page": page,
                "page_size": page_size,
                "_debug_raw": debug_raw,
            }),
        ),
        Some(Cmd::FindReferencingCodeSnippets {
            file,
            line,
            col,
            symbol,
            context_lines,
            max_results,
            debug_raw,
        }) => {
            // --symbol 直查：位置可省，supervisor 端解析符号名转坐标（O3）。
            let mut a = json!({
                "context_lines": context_lines,
                "max_results": max_results,
                "_debug_raw": debug_raw,
            });
            if let Some(name) = symbol {
                a["symbol"] = json!(name);
            }
            if let Some(f) = file {
                a["file"] = json!(f);
            }
            if let Some(l) = line {
                a["line"] = json!(l);
            }
            if let Some(c) = col {
                a["col"] = json!(c);
            }
            ("find-referencing-code-snippets", a)
        }
        Some(Cmd::SymbolBody { file, symbol, .. }) => (
            "symbol-body",
            json!({
                "file": file,
                "symbol": symbol.as_deref().expect("resolved by resolve_with_alias"),
            }),
        ),
        Some(Cmd::EditContext { file, symbol, .. }) => (
            "edit-context",
            json!({
                "file": file,
                "symbol": symbol.as_deref().expect("resolved by resolve_with_alias"),
            }),
        ),
        Some(Cmd::RepoMap { top_n }) => ("repo-map", json!({"top_n": top_n})),
        Some(Cmd::Warm {
            lang,
            timeout_secs,
            ..
        }) => ("warm", json!({"lang": lang, "timeout_secs": timeout_secs})),
        Some(Cmd::ReplaceBody {
            file,
            symbol,
            new_body,
        }) => (
            "replace-body",
            json!({"file": file, "symbol": symbol, "new_body": new_body}),
        ),
        Some(Cmd::ReplaceTextInSymbol {
            file,
            symbol,
            old_text,
            new_text,
        }) => (
            "replace-text-in-symbol",
            json!({"file": file, "symbol": symbol, "old_text": old_text, "new_text": new_text}),
        ),
        Some(Cmd::InsertTextBeforeSymbol { file, symbol, text, .. }) => (
            "insert-text-before-symbol",
            // --with 别名已由 resolve_with_alias 落回 text（cli_main 必经）。
            json!({"file": file, "symbol": symbol, "text": text.as_deref().expect("resolved by resolve_with_alias")}),
        ),
        Some(Cmd::InsertTextAfterSymbol { file, symbol, text, .. }) => (
            "insert-text-after-symbol",
            json!({"file": file, "symbol": symbol, "text": text.as_deref().expect("resolved by resolve_with_alias")}),
        ),
        Some(Cmd::DeleteTextInSymbol {
            file,
            symbol,
            start_line,
            end_line,
        }) => (
            "delete-text-in-symbol",
            json!({"file": file, "symbol": symbol, "start_line": start_line, "end_line": end_line}),
        ),
        Some(Cmd::SafeDeleteSymbol { file, symbol }) => (
            "safe-delete-symbol",
            json!({"file": file, "symbol": symbol}),
        ),
        Some(Cmd::InsertAtLine {
            file,
            line,
            text,
            expected_hash,
            ..
        }) => (
            "insert-at-line",
            json!({
                "file": file,
                "line": line,
                "content": text.as_deref().expect("resolved by resolve_with_alias"),
                "expected_hash": expected_hash,
            }),
        ),
        Some(Cmd::ReplaceLines {
            file,
            start_line,
            end_line,
            text,
            expected_hash,
            ..
        }) => (
            "replace-lines",
            json!({
                "file": file,
                "start_line": start_line,
                "end_line": end_line,
                "content": text.as_deref().expect("resolved by resolve_with_alias"),
                "expected_hash": expected_hash,
            }),
        ),
        Some(Cmd::DeleteLines {
            file,
            start_line,
            end_line,
            expected_hash,
        }) => (
            "delete-lines",
            json!({
                "file": file,
                "start_line": start_line,
                "end_line": end_line,
                "expected_hash": expected_hash,
            }),
        ),
        Some(Cmd::Completion {
            file,
            line,
            col,
            limit,
            trigger,
        }) => {
            // trigger=None 时按 file 后缀自动推断（C++=., Rust=::, TS/JS/Py=.）；
            // 显式传 "" 也视为 None（agent 端不需要感知 7 种扩展名）。
            let trigger = trigger
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .or_else(|| infer_trigger_char(file));
            (
                "completion",
                json!({
                    "file": file,
                    "line": line,
                    "col": col,
                    "limit": limit,
                    "trigger": trigger,
                }),
            )
        }
        Some(Cmd::ContainingSymbol { file, line, col }) => (
            "containing-symbol",
            json!({
                "file": file,
                "line": line,
                "col": col,
            }),
        ),
        Some(Cmd::DefiningSymbol { file, line, col }) => (
            "defining-symbol",
            json!({
                "file": file,
                "line": line,
                "col": col,
            }),
        ),

        Some(Cmd::SignatureHelp { file, line, col }) => (
            "signature-help",
            json!({
                "file": file,
                "line": line,
                "col": col,
            }),
        ),
        // ==== Phase 1 · 上游 wrapper 缺口（13 个）====
        Some(Cmd::CodeAction {
            file,
            line,
            col,
            kind,
        }) => (
            "code-action",
            json!({"file": file, "line": line, "col": col, "kind": kind}),
        ),
        Some(Cmd::Format {
            file,
            tab_size,
            insert_spaces,
        }) => (
            "format",
            json!({
                "file": file,
                "tab_size": tab_size,
                "insert_spaces": insert_spaces,
            }),
        ),
        Some(Cmd::FormatRange {
            file,
            start_line,
            start_col,
            end_line,
            end_col,
            tab_size,
            insert_spaces,
        }) => (
            "format-range",
            json!({
                "file": file,
                "start_line": start_line,
                "start_col": start_col,
                "end_line": end_line,
                "end_col": end_col,
                "tab_size": tab_size,
                "insert_spaces": insert_spaces,
            }),
        ),
        Some(Cmd::InlayHint {
            file,
            start_line,
            end_line,
        }) => (
            "inlay-hint",
            json!({
                "file": file,
                "start_line": start_line,
                "end_line": end_line,
            }),
        ),
        Some(Cmd::DocumentHighlight { file, line, col }) => (
            "document-highlight",
            json!({"file": file, "line": line, "col": col}),
        ),
        Some(Cmd::FoldingRange { file }) => ("folding-range", json!({"file": file})),
        Some(Cmd::SemanticTokens { file }) => ("semantic-tokens", json!({"file": file})),
        Some(Cmd::CodeLens { file }) => ("code-lens", json!({"file": file})),
        Some(Cmd::DocumentLink { file }) => ("document-link", json!({"file": file})),
        Some(Cmd::CallHierarchy {
            op,
            file,
            line,
            col,
            item,
        }) => {
            let mut args = json!({"op": op});
            if let (Some(f), Some(l), Some(c)) = (file, line, col) {
                args["file"] = json!(f);
                args["line"] = json!(l);
                args["col"] = json!(c);
            }
            if let Some(it) = item {
                // 解析 agent 传入的 JSON item 字符串；解析失败留原串（supervisor 端会拒）。
                args["item"] = serde_json::from_str(it)
                    .unwrap_or_else(|_| serde_json::Value::String(it.clone()));
            }
            ("call-hierarchy", args)
        }
        Some(Cmd::TypeHierarchy {
            op,
            file,
            line,
            col,
            item,
        }) => {
            let mut args = json!({"op": op});
            if let (Some(f), Some(l), Some(c)) = (file, line, col) {
                args["file"] = json!(f);
                args["line"] = json!(l);
                args["col"] = json!(c);
            }
            if let Some(it) = item {
                args["item"] = serde_json::from_str(it)
                    .unwrap_or_else(|_| serde_json::Value::String(it.clone()));
            }
            ("type-hierarchy", args)
        }
        Some(Cmd::Moniker { file, line, col }) => {
            ("moniker", json!({"file": file, "line": line, "col": col}))
        }
        Some(Cmd::WorkspaceDiagnostic) => ("workspace-diagnostic", json!({})),
        Some(Cmd::CreateTextFile { file, content, .. }) => (
            "create-text-file",
            json!({"file": file, "content": content.as_deref().expect("resolved by resolve_with_alias")}),
        ),
        Some(Cmd::Undo { steps, list }) => ("undo", json!({"steps": steps, "list": list})),
        Some(Cmd::Redo) => ("redo", json!({})),
        Some(Cmd::Test { file, name }) => {
            let mut a = json!({"target": file});
            if let Some(n) = name
                && !n.is_empty()
            {
                a["name"] = json!(n);
            }
            ("test", a)
        }
        Some(Cmd::Diff { txn_id, patch }) => {
            let mut a = json!({});
            if let Some(n) = txn_id {
                a["txn_id"] = json!(n);
            }
            if *patch {
                a["patch"] = json!(true);
            }
            ("diff", a)
        }
        Some(Cmd::FindTest { symbol, .. }) => (
            "find-test",
            json!({"symbol": symbol.as_deref().expect("resolved by resolve_with_alias")}),
        ),
        Some(Cmd::Recipe { name, args, new_body, to, as_name, target, tests_file, tests, run }) => {
            let mut a = json!({ "name": name, "pos": args });
            if let Some(v) = new_body { a["new_body"] = json!(v); }
            if let Some(v) = to { a["to"] = json!(v); }
            if let Some(v) = as_name { a["as"] = json!(v); }
            if let Some(v) = target { a["target"] = json!(v); }
            if let Some(v) = tests_file { a["tests_file"] = json!(v); }
            if let Some(v) = tests { a["tests"] = json!(v); }
            if *run { a["run"] = json!(true); }
            ("recipe", a)
        }
        Some(Cmd::Status { .. })
        | Some(Cmd::ProjectInfo { .. })
        | Some(Cmd::ChangeHistory { .. })
        | Some(Cmd::StopAll)
        | Some(Cmd::Install { .. })
        | Some(Cmd::Uninstall { .. })
        | Some(Cmd::LsUse { .. })
        | Some(Cmd::LsList { .. })
        | Some(Cmd::LsRemove { .. })
        | Some(Cmd::Shell)
        | Some(Cmd::Doctor { .. })
        | Some(Cmd::LintShell { .. })
        | Some(Cmd::WaitReady { .. })
        | None => return None,
    })
}

/// forward/--direct 共用：CLI 全局 flag → args 私有字段。supervisor 消费：
/// `_timeout_ms`/`_index_timeout_ms` 三层合并（Task 22b）、`_delta` 编排（§11-J）、
/// `_compact` envelope（§10-H）、`_max_tokens`/`_compress` 末尾后处理（§10-G）。
/// 两条路径同一注入 = 同一输出契约（kns）。
fn inject_private_args(args: &mut serde_json::Value, cli: &Cli) {
    inject_timeout_args(args, cli.request_timeout, cli.index_timeout, cli.warmup_timeout);
    if cmd_requests_delta(&cli.cmd)
        && let Some(obj) = args.as_object_mut()
    {
        obj.insert("_delta".into(), serde_json::json!(true));
    }
    inject_compact_arg(args, cli.json);
    if let Some(obj) = args.as_object_mut() {
        if let Some(n) = cli.max_tokens {
            obj.insert("_max_tokens".into(), serde_json::json!(n));
        }
        if cli.compress {
            obj.insert("_compress".into(), serde_json::json!(true));
        }
        if cli.dry_run {
            obj.insert("dry_run".into(), serde_json::json!(true));
        }
    }
}

/// unknown tool 错误的 daemon/CLI 版本错位 hint（bd serena-rust-eog5，第 3 次同坑）：
/// 新 CLI 连旧 exe lazy-spawn 的 daemon 时，新子命令报 BAD_ARGS unknown tool ——
/// 表象是功能缺失，实是 daemon 版本落后。
fn unknown_tool_hint(err: &serde_json::Value) -> Option<&'static str> {
    let code = err.get("code").and_then(|c| c.as_str())?;
    let msg = err.get("message").and_then(|m| m.as_str())?;
    (code == "BAD_ARGS" && msg.starts_with("unknown tool")).then_some(
        "daemon may have been started by an older binary; run `serena-cli stop-all` and retry",
    )
}

/// bd serena-rust-c6pb + p2zp：daemon 报 LS_NOT_INSTALLED 时客户端同层复算
/// （probe_launch：T2 launch_info / ensure_launch，与 daemon 冷启动同一判定层）
/// ——客户端可拉起而 daemon 说不 installed = daemon spawn 环境（PATH 快照 /
/// 旧二进制）与当前 shell 错位，不是"本机没有"；此时给重启指引而不是放任
/// install/ls-use 死循环（NitpickEdge F6 两轮实锤：pyright 在客户端 PATH，
/// daemon 看不见）。
///
/// p2zp 强化：
/// 1) 无论 probe_launch 成败都给 hint——老版本只覆盖"客户端能拉起"分支，"客户
///    端也拉不起"时无指引，AI 不知道下一步是 install 还是 stop-all；
/// 2) hint 用 [hint][ACTION] 前缀（伪彩色 ANSI 大写），与 daemon 启动进度行
///    等行宽内区分，stderr 末尾定位更稳；
/// 3) 两套分支给不同行动指引——错位（stop-all 重试）vs 真没装（install / 生态命令）。
async fn ls_env_mismatch_hint(lang: Option<&str>, root: &Path) {
    let Some(lang) = lang else {
        return;
    };
    match ls_registry::probe_launch(lang, root).await {
        Ok(()) => eprintln!(
            "[hint][DAEMON_STALE] `{lang}` LS is launchable from this shell, but the daemon reports LS_NOT_INSTALLED — daemon was spawned with a different PATH (or by an older binary). \
             Fix: run `serena-cli stop-all` and retry your command."
        ),
        Err(_) => eprintln!(
            "[hint][LS_MISSING] `{lang}` LS is not installed (this shell can't launch it either). \
             Fix: install it via `serena-cli install {lang}` or `serena-cli doctor --fix`, \
             then retry your command. If you already have the LS binary, register it with \
             `serena-cli ls-use {lang} <path>` so the daemon picks it up after restart."
        ),
    }
}

/// 按子命令转发 HTTP。
/// Ok(0) = 工具成功；Ok(n) = 工具失败（wire 码 → exit 码，消息已打 stderr）；
/// Err = 传输层失败（bd serena-rust-cwt：退出码经返回值传递，不用 process::exit）。
async fn forward(
    cli: &Cli,
    base: &str,
    token: &mut String,
    lock_path: &Path,
    lang: Option<&str>,
) -> Result<u8, ForwardFailure> {
    let client = http_client();
    let (tool, mut args) = tool_request(&cli.cmd)
        .expect("handled earlier: local commands returned before forward");
    let project_root = resolve_project_root(cli.project.clone());
    inject_private_args(&mut args, cli);
    // d3a：编排 envelope——invocation_id 来源 --invocation-id 覆写 > 自动生成
    // （UUID v4，std 熵）。header + body envelope 双通道携带；daemon 按
    // invocation_id 记重放日志（成功失败都记）。
    let invocation_id = cli
        .invocation_id
        .clone()
        .unwrap_or_else(daemon::http::new_invocation_id);
    let envelope = daemon::dto::InvocationEnvelope {
        invocation_id: invocation_id.clone(),
        server_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        protocol_version: Some(daemon::dto::WIRE_PROTOCOL_VERSION.to_string()),
        compat_evidence: Some(daemon::dto::CompatEvidence {
            env: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            protocol_version: daemon::dto::WIRE_PROTOCOL_VERSION.to_string(),
            capabilities: vec![],
        }),
    };
    let body = json!({
        "project_root": project_root.to_string_lossy(),
        "args": args,
        "lang": lang,
        "envelope": envelope,
    });

    let url = format!("{base}/tools/{tool}");
    // 批2-F：recipe 长步的 daemon→CLI stderr 中继。daemon 模式下 supervisor
    // （daemon 进程）的步进度行落 daemon stderr（默认 NULL）——CLI 看不到，长步
    // 哑语。转发期间轮询边带文件增量中继到本进程 stderr（--direct 进程内直打，
    // 不经此路径）。非 recipe 工具零开销（不 spawn）。
    let relay = if tool == "recipe" {
        Some(RecipeProgressRelay::start())
    } else {
        None
    };
    let send_once = |token: &String| {
        client
            .post(&url)
            .header("X-Serena-Token", token)
            .header("X-Invocation-Id", &invocation_id)
            .json(&body)
            .timeout(FORWARD_TIMEOUT)
            .send()
    };
    let mut resp = send_with_connect_retry(
        || send_once(&token.clone()),
        // 与压测观察一致：error sending request 覆盖 connect 与 request 两类瞬断形态。
        |e: &reqwest::Error| e.is_connect() || e.is_request(),
    )
    .await
    .map_err(|e| format!("forward {tool}: {e}"))?;
    if resp.status() == reqwest::StatusCode::FORBIDDEN
        && let Some(fresh) = refresh_token_if_stale(lock_path, token).await
    {
        // daemon 换代后缓存 token 过期：已刷新，用新 token 重发一次。
        tracing::info!("403 with stale token; refreshed from lock, retrying once");
        *token = fresh;
        resp = send_once(token)
            .await
            .map_err(|e| format!("forward {tool}: {e}"))?;
    }

    let status = resp.status();
    let payload: serde_json::Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    // 批2-F：响应已回，缓冲收 daemon 侧最后几行（step done）再停中继。
    if let Some(r) = relay {
        r.finish().await;
    }

    if !status.is_success() {
        // 403/503 等传输层错；503 DAEMON_DRAINING 单独分类供上层自愈重试（g0m）。
        if is_daemon_draining(status, &payload) {
            return Err(ForwardFailure::Draining { status, payload });
        }
        return Err(format!("daemon transport error {status}: {payload}").into());
    }
    match payload.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => {
            let data = payload.get("data").unwrap_or(&serde_json::Value::Null);
            // bd serena-rust-bxd O2：人类可读模式不再吞 supervisor warning ——
            // 统一打 stderr（--json 模式 warning 字段本就随 data 透传，不动）。
            if let Some(w) = data.get("warning").and_then(|v| v.as_str()) {
                eprintln!("[warn] {w}");
            }
            // bd serena-rust-c6pb：多语言聚合信封（find-symbol 等）把 per-lang
            // NotInstalled 折进 warning 而非 error——warning 同判补环境错位 hint
            // （语言不可定时无从复算，静默跳过）。
            if data
                .get("warning")
                .and_then(|v| v.as_str())
                .is_some_and(|w| w.contains("not installed"))
            {
                ls_env_mismatch_hint(lang, &project_root).await;
            }
            // 空结果 + warning = 「没符号」可能是「没就绪」（we0/暖机窗口）→
            // 误导性最强的形态，额外给固定 hint；正常空（无 warning）不打，不误报。
            // bd serena-rust-y3c1：hint 附降级指引——documentSymbol 层工具常已可用。
            // bd serena-rust-mfht F6：hint 仅在 warning 暗示语义未就绪时打，
            // 项目切换等无关 warning 不再误导 agent「重试/等就绪」。
            if let Some(w) = data.get("warning").and_then(|v| v.as_str())
                && payload_is_empty(data)
                && warning_suggests_index_warming(w)
            {
                eprintln!(
                    "[hint] index warming: semantic layer not ready, empty result may be false negative (rerun or use wait-ready --stage def); documentSymbol-layer tools (find-referencing-code-snippets / overview / symbol-body) may already work"
                );
            }
            // bd e1f4：undo 遇 sha 冲突 discard 后立即停止（时间线乱，禁
            // fall-through）——载荷仍是成功形态（undone/skipped 可解析），但
            // 部分完成非完全成功 → rc=2。其余工具成功恒 rc=0。
            let exit = if tool == "undo"
                && data.get("stopped_early").is_some_and(|v| !v.is_null())
            {
                2u8
            } else {
                0u8
            };
            print_json(data).map_err(|e| e.to_string())?;
            Ok(exit)
        }
        _ => {
            let err = payload.get("error").cloned().unwrap_or(payload);
            // bd serena-rust-p2zp：LS_NOT_INSTALLED hint 提到 tool error 之前——
            // 错误详情对 AI 操作者是噪音（已知码），行动指引才是有效信息；hint 先
            // 浮顶 = agent 看到 hint 可立即停手决策（stop-all 重试 vs install），
            // 错误全文仅供人肉诊断。
            if err.get("code").and_then(|c| c.as_str()) == Some("LS_NOT_INSTALLED") {
                ls_env_mismatch_hint(lang, &project_root).await;
            }
            // bd fdmj-F10：错误统一裸 JSON 走 stdout（与成功载荷/clap 用法错同流
            // 同形，agent 不再分形态解析）；原 "tool error: " 人读前缀挪 stderr
            // [error] 行——两流职责分明（stdout=JSON、stderr=人读诊断）。
            eprintln!("[error] {err}");
            println!("{err}");
            if let Some(hint) = unknown_tool_hint(&err) {
                eprintln!("[hint] {hint}");
            }
            // Δ 43ae021：exit 码按 wire code 取（ARCH §6.3 / dto::wire_error_code_to_exit），
            // 不再一律 1 —— Internal→3、BadArgs→2，agent 据此免重试确定性失败。
            // bd serena-rust-cwt：不再 std::process::exit —— 那会跳 Drop，
            // tracing subscriber / reqwest 缓冲来不及 flush（错误链零线索）；
            // 改为把退出码作为 Ok 值回传，正常走 main 的收尾路径。
            let code = err
                .get("code")
                .and_then(|c| serde_json::from_value::<daemon::dto::WireErrorCode>(c.clone()).ok());
            Ok(code.map_or(1u8, daemon::dto::wire_error_code_to_exit))
        }
    }
}

/// 批2-F：recipe 步进度的 daemon→CLI stderr 中继。daemon 模式下 supervisor
/// （daemon 进程）的进度行 eprintln 落 daemon stderr（默认 NULL）——CLI 看不见。
/// 转发 recipe 请求期间轮询边带文件（temp/`RECIPE_PROGRESS_FILE`，supervisor
/// 步级 append），把含 `[recipe]` 的增量行打到本进程 stderr。单机单 daemon
///（:7860 lock 仲裁）→ temp 单文件无归属歧义。Drop 兜底停止（错误路径不泄漏）。
struct RecipeProgressRelay {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: tokio::task::JoinHandle<()>,
}

impl RecipeProgressRelay {
    fn start() -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let path = std::env::temp_dir().join(supervisor::RECIPE_PROGRESS_FILE);
        let _ = std::fs::write(&path, b""); // 清上一轮残留，本轮 offset 从 0 起
        let stop2 = stop.clone();
        let handle = tokio::spawn(async move {
            let mut offset = 0u64;
            while !stop2.load(Ordering::Relaxed) {
                offset = relay_progress_lines(&path, offset);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        });
        Self { stop, handle }
    }

    /// 正常收尾：缓冲 300ms 让 daemon 的结束行落盘，再停轮询（循环 100ms 内
    /// 自停；Drop 兜底 abort，无需 join——CLI 进程存活远长于 100ms）。
    async fn finish(&self) {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        self.stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for RecipeProgressRelay {
    fn drop(&mut self) {
        self.stop
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self.handle.abort();
    }
}

/// 读 path 自 offset 起的新增字节，打印含 `[recipe]` 的行；返回新 offset。
/// 打不开/没新增/读失败都原样返回（中继是 best-effort 人读反馈，不参与成败）。
fn relay_progress_lines(path: &Path, offset: u64) -> u64 {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return offset;
    };
    let len = match f.metadata().map(|m| m.len()) {
        Ok(l) => l,
        Err(_) => return offset,
    };
    if len <= offset {
        return offset;
    }
    if f.seek(SeekFrom::Start(offset)).is_err() {
        return offset;
    }
    let mut buf = vec![0u8; (len - offset) as usize];
    if f.read_exact(&mut buf).is_err() {
        return offset;
    }
    for line in String::from_utf8_lossy(&buf).lines() {
        if line.contains("[recipe]") {
            eprintln!("{line}");
        }
    }
    len
}

/// `install` 子命令（Task 21）：配置驱动 LS 安装（幂等——已装即返回路径）。
/// `source` 字段标注条目来源（external-servers.toml / 内置 servers.toml）。
fn cmd_install(lang: &str) -> ExitCode {
    match ls_registry::config::ensure_launch(lang, None, true, false) {
        Ok((exe, args)) => {
            // args = expand_exec 完整 argv（首元素即 exe）。
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "lang": lang,
                    "source": ls_registry::config::spec_source(lang).unwrap_or("builtin"),
                    "exe": exe.display().to_string(),
                    "cmd": args,
                }))
                .unwrap_or_default()
            );
            ExitCode::SUCCESS
        }
        Err(msg) => {
            eprintln!("install failed: {msg}");
            ExitCode::from(3)
        }
    }
}

/// `install --all`：遍历 servers.toml 全部条目（内置 + external-servers.toml）逐个
/// ensure_launch。幂等——已装返 Ready（无下载），未装走 ensure_launch 完整下载路径。
/// 累计统计 ok / skipped / failed，最后输出 JSON。
fn cmd_install_all() -> ExitCode {
    let ids: Vec<&str> = ls_registry::config::all_server_ids().collect();
    let mut ok = 0usize;
    let mut failed: Vec<(String, String)> = Vec::new();
    for id in ids {
        match ls_registry::config::ensure_launch(id, None, true, false) {
            Ok((exe, _args)) => {
                ok += 1;
                let src = ls_registry::config::spec_source(id).unwrap_or("builtin");
                eprintln!("[OK]    {id:<28} -> {}/{}", exe.display(), src);
            }
            Err(msg) => {
                failed.push((id.to_string(), msg.clone()));
                eprintln!("[FAIL]  {id:<28} {msg}");
            }
        }
    }
    let total = ok + failed.len();
    let payload = json!({
        "ok": failed.is_empty(),
        "total": total,
        "installed": ok,
        "failed": failed.iter().map(|(id, m)| json!({"id": id, "msg": m})).collect::<Vec<_>>(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).unwrap_or_default()
    );
    if failed.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// `uninstall <lang>`：删除 serena 托管 LS 缓存（`{cache_root}/{id}/` 全版本/变体）。
/// 安全铁律：canonicalize 后目标必须仍在缓存根内（`ensure_within_cache_root`）——
/// 防 servers.toml 条目 id 被改成 `../..` 形态 / 缓存目录被软链出根后误删任意路径。
/// path_only（PATH 探测）/ uvx（uv 自管缓存）型 serena 不落缓存，报非托管不删。
fn cmd_uninstall(lang: &str, json: bool) -> ExitCode {
    let Some((id, spec)) = ls_registry::config::spec_for(lang) else {
        eprintln!("uninstall failed: no servers.toml entry for `{lang}`");
        return ExitCode::from(3);
    };
    if matches!(
        spec.kind_table(),
        Some(ls_registry::spec::KindRef::PathOnly(_)) | Some(ls_registry::spec::KindRef::Uvx(_))
    ) {
        eprintln!("uninstall: `{id}` is not serena-managed (no cache dir); nothing removed");
        return ExitCode::from(1);
    }
    let cache_root = ls_registry::config::dirs_cache_root();
    let dir = cache_root.join(id);
    if !dir.is_dir() {
        eprintln!(
            "uninstall failed: language server `{lang}` not installed; run `serena-cli install {id}`"
        );
        return ExitCode::from(3);
    }
    let dir = match ls_registry::config::ensure_within_cache_root(&cache_root, &dir) {
        Ok(d) => d,
        Err(m) => {
            eprintln!("uninstall refused: {m}");
            return ExitCode::from(1);
        }
    };
    let bytes_freed = dir_size(&dir);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        eprintln!("uninstall failed: remove `{}`: {e}", dir.display());
        return ExitCode::from(3);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "lang": id,
                "removed_dirs": [dir.display().to_string()],
                "bytes_freed": bytes_freed,
            }))
            .unwrap_or_default()
        );
    } else {
        println!(
            "uninstalled `{id}` -> {} ({bytes_freed} bytes freed)",
            dir.display()
        );
    }
    ExitCode::SUCCESS
}

/// 目录递归字节数（uninstall `bytes_freed` 用）；symlink 不跟进防环。
fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0u64;
    for entry in entries.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
            continue;
        };
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

// ===== ls-use / ls-list / ls-remove（bd serena-rust-4ux）=====

/// 本地管理命令的标准错误 JSON（与 daemon 9 错误码 wire 契约同形；retryable 恒
/// false——本地 fs/配置错误重试无意义）。
fn local_err_json(code: &str, msg: &str) {
    println!(
        "{}",
        json!({"ok": false, "error": {"code": code, "message": msg, "retryable": false}})
    );
}

/// 用户二进制的匹配键（小写 stem，去扩展）——`ls-use` 按二进制名智能匹配内置
/// server（oxw0）。无文件名段时退空串（validate_user_binary 已挡，不会命中任何键）。
fn binary_stem(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// 内置条目暴露的启动二进制名键（小写）：id + path_only/npm/uvx/dotnet/gem/
/// download 各子表的启动名。download 的 bin_path 取末段剥 .exe（marksman.exe →
/// marksman；平台互异路径全收）。
fn builtin_binary_keys(id: &str, spec: &ls_registry::spec::ServerSpec) -> Vec<String> {
    let mut keys = vec![id.to_ascii_lowercase()];
    if let Some(po) = &spec.path_only {
        keys.push(po.binary_name.to_ascii_lowercase());
    }
    if let Some(n) = &spec.npm {
        keys.push(n.package.to_ascii_lowercase());
        keys.push(n.bin_rel.to_ascii_lowercase());
    }
    if let Some(u) = &spec.uvx {
        keys.push(u.package.to_ascii_lowercase());
        keys.push(u.entrypoint.to_ascii_lowercase());
    }
    if let Some(d) = &spec.dotnet {
        keys.push(d.tool.to_ascii_lowercase());
    }
    if let Some(g) = &spec.gem {
        keys.push(g.gem.to_ascii_lowercase());
    }
    if let Some(dl) = &spec.download {
        let mut push_bin = |bp: &str| {
            let name = bp
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(bp)
                .to_ascii_lowercase();
            keys.push(name.strip_suffix(".exe").unwrap_or(&name).to_string());
        };
        push_bin(&dl.bin_path);
        for bp in dl.bin_path_per_platform.values() {
            push_bin(bp);
        }
    }
    keys.sort_unstable();
    keys.dedup();
    keys
}

/// 按二进制 stem 在内置表里找 server：恰好一个 → Ok((id, spec))；零/多个 →
/// Err(命中 id 清单)（零命中 = 空清单）。
fn builtin_id_for_binary(
    stem: &str,
) -> Result<(&'static str, &'static ls_registry::spec::ServerSpec), Vec<&'static str>> {
    let hits: Vec<_> = ls_registry::config::builtin_entries()
        .into_iter()
        .filter(|(id, spec)| builtin_binary_keys(id, spec).iter().any(|k| k == stem))
        .collect();
    if hits.len() == 1 {
        let (id, spec) = hits[0];
        Ok((id, spec))
    } else {
        Err(hits.iter().map(|(id, _)| *id).collect())
    }
}

/// ls-use <LANG_OR_ID> <PATH>：注册/覆盖；--list；--remove <ID>；--lang/--ext
/// 注册全新语言。已知 server id → 整条继承内置条目；仅语言名命中 → 按二进制名
/// 智能匹配内置 server（oxw0：语言默认条目的启动参数对别的二进制必然错配），
/// 唯一命中自动选，零/多命中拒改并列候选。
fn cmd_ls_use(
    lang_or_id: &str,
    path: Option<String>,
    list: bool,
    new_lang: Option<String>,
    new_ext: Option<String>,
    remove: Option<String>,
) -> ExitCode {
    let Some(cfg_path) = ls_registry::config::external_servers_path() else {
        local_err_json(
            "INTERNAL",
            "cannot resolve external-servers.toml path (no APPDATA/HOME)",
        );
        return ExitCode::from(3);
    };
    let cfg_str = cfg_path.display().to_string();

    if list {
        return cmd_ls_use_list(&cfg_path);
    }
    if let Some(id) = remove {
        if !ls_registry::config::valid_block_id(&id) {
            local_err_json(
                "BAD_ARGS",
                &format!("invalid server id `{id}` (expected [A-Za-z0-9_-])"),
            );
            return ExitCode::from(2);
        }
        return match ls_registry::config::external_block_remove(&cfg_path, &id) {
            Ok(true) => {
                println!(
                    "{}",
                    json!({"ok": true, "server": id, "file": cfg_str, "removed": true})
                );
                ExitCode::SUCCESS
            }
            Ok(false) => {
                local_err_json(
                    "BAD_ARGS",
                    &format!("`{id}` is not registered in {cfg_str}"),
                );
                ExitCode::from(2)
            }
            Err(e) => {
                local_err_json("INTERNAL", &format!("write {cfg_str}: {e}"));
                ExitCode::from(3)
            }
        };
    }

    let Some(bin_raw) = path else {
        eprintln!(
            "usage: serena-cli ls-use <LANG_OR_ID> <PATH_TO_BINARY> | --list | --remove <ID>"
        );
        return ExitCode::from(2);
    };
    if lang_or_id.is_empty() {
        local_err_json("BAD_ARGS", "language name or server id required");
        return ExitCode::from(2);
    }
    let bin = match ls_registry::config::validate_user_binary(Path::new(&bin_raw)) {
        Ok(b) => b,
        Err(m) => {
            local_err_json("BAD_ARGS", &m);
            return ExitCode::from(2);
        }
    };
    // bd serena-rust-oxw0：lang_or_id 显式命中内置 server id = 用户点名条目，整条
    // 继承（尊重显式选择）；仅语言名命中 → 按二进制 stem 智能匹配内置 server，
    // 唯一命中自动选（回显最终生效 id），零/多命中拒改并列候选。
    let explicit_id = ls_registry::config::builtin_entries()
        .iter()
        .any(|(bid, _)| bid.eq_ignore_ascii_case(lang_or_id));
    let mut matched_by: Option<String> = None;
    let (id, languages, extensions, exec, is_override) = match ls_registry::config::builtin_spec_for(lang_or_id) {
        Some((id, spec)) if explicit_id => (
            id.to_string(),
            spec.languages.clone(),
            spec.extensions.clone(),
            ls_registry::config::inherited_exec(spec),
            true,
        ),
        Some((lang_id, _)) => {
            let stem = binary_stem(&bin);
            match builtin_id_for_binary(&stem) {
                // 唯一命中语言默认条目 → 常规整条继承。
                Ok((hit, hspec)) if hit == lang_id => (
                    hit.to_string(),
                    hspec.languages.clone(),
                    hspec.extensions.clone(),
                    ls_registry::config::inherited_exec(hspec),
                    true,
                ),
                // 唯一命中别的条目 → 重定向：用命中条目的启动形态；languages 并上
                // 请求语言（external 同语言并列胜内置，merge_pick §3——python 路由
                // 随注册切到命中条目）。
                Ok((hit, hspec)) => {
                    matched_by = Some(format!(
                        "binary `{stem}` matched built-in server `{hit}` (requested language `{lang_or_id}`)"
                    ));
                    let mut langs = vec![lang_or_id.to_string()];
                    langs.extend(
                        hspec.languages.iter().filter(|l| !l.eq_ignore_ascii_case(lang_or_id)).cloned(),
                    );
                    (
                        hit.to_string(),
                        langs,
                        hspec.extensions.clone(),
                        ls_registry::config::inherited_exec(hspec),
                        true,
                    )
                }
                // 零命中 → 拒改 + 该语言的内置 server 候选（要求显式点名 server id）。
                Err(hits) if hits.is_empty() => {
                    let servers: Vec<&str> = ls_registry::config::builtin_entries()
                        .into_iter()
                        .filter(|(_, s)| {
                            s.languages.iter().any(|l| l.eq_ignore_ascii_case(lang_or_id))
                        })
                        .map(|(bid, _)| bid)
                        .collect();
                    let list = if servers.is_empty() {
                        "(none — browse `serena-cli ls-list`)".to_string()
                    } else {
                        servers.join(", ")
                    };
                    local_err_json(
                        "BAD_ARGS",
                        &format!(
                            "binary `{stem}` matches no built-in server; built-in servers for \
                             language `{lang_or_id}`: {list}; register explicitly with a server \
                             id: serena-cli ls-use <server-id> <path>"
                        ),
                    );
                    return ExitCode::from(2);
                }
                // 多命中 → 拒改 + 命中清单（同名二进制归属歧义）。
                Err(hits) => {
                    local_err_json(
                        "BAD_ARGS",
                        &format!(
                            "binary `{stem}` matches multiple built-in servers ({}); register \
                             explicitly with a server id: serena-cli ls-use <server-id> <path>",
                            hits.join(", ")
                        ),
                    );
                    return ExitCode::from(2);
                }
            }
        }
        None => match (new_lang, new_ext) {
            (Some(l), Some(e)) => {
                if !ls_registry::config::valid_block_id(lang_or_id) {
                    local_err_json(
                        "BAD_ARGS",
                        &format!("invalid server id `{lang_or_id}` (expected [A-Za-z0-9_-])"),
                    );
                    return ExitCode::from(2);
                }
                let ext = if e.starts_with('.') {
                    e
                } else {
                    format!(".{e}")
                };
                (
                    lang_or_id.to_string(),
                    vec![l],
                    vec![ext],
                    Vec::new(),
                    false,
                )
            }
            (got_lang, got_ext) => {
                let mut msg = format!("unknown language/server id `{lang_or_id}`");
                let cands = ls_registry::config::similar_server_ids(lang_or_id);
                if !cands.is_empty() {
                    let near: Vec<&str> = cands.iter().take(10).copied().collect();
                    msg.push_str(&format!("; similar: {}", near.join(", ")));
                }
                match (got_lang, got_ext) {
                        (None, Some(_)) => msg.push_str("; --lang is required together with --ext"),
                        (Some(_), None) => msg.push_str("; --ext is required together with --lang"),
                        _ => msg.push_str(&format!(
                            "; to register a brand-new language: serena-cli ls-use {lang_or_id} <path> --lang <LANG> --ext .<ext>"
                        )),
                    }
                local_err_json("BAD_ARGS", &msg);
                return ExitCode::from(2);
            }
        },
    };
    let block =
        ls_registry::config::ls_use_block(&id, &languages, &extensions, &exec, &bin, is_override);
    if let Err(e) = ls_registry::config::external_block_upsert(&cfg_path, &id, &block) {
        local_err_json("INTERNAL", &format!("write {cfg_str}: {e}"));
        return ExitCode::from(3);
    }
    // 启动命令形态回显：{bin} 展开为注册的二进制——参数与二进制错配一眼可见
    // （oxw0：此前回显只有 server 名，jedi 参数塞给 pyright 不可见）。
    let launch: Vec<String> = if exec.is_empty() {
        vec![bin.display().to_string()]
    } else {
        exec.iter()
            .map(|a| {
                if a == "{bin}" {
                    bin.display().to_string()
                } else {
                    a.clone()
                }
            })
            .collect()
    };
    println!(
        "{}",
        json!({
            "ok": true,
            "server": id,
            "languages": languages,
            "launch": launch,
            "file": cfg_str,
            "mode": if is_override { "override" } else { "new" },
            "matched_by": matched_by,
            "note": "takes effect after daemon restart (serena-cli stop-all or idle timeout)",
        })
    );
    ExitCode::SUCCESS
}

/// ls-use --list：external 条目 + 每语言生效来源（builtin / external-override /
/// external-new；负 priority 且撞内置 → 实际生效源是 builtin）。
fn cmd_ls_use_list(cfg_path: &Path) -> ExitCode {
    let mut entries = ls_registry::config::external_entries(cfg_path);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let servers: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|(id, spec)| {
            let per_lang: Vec<serde_json::Value> = spec
                .languages
                .iter()
                .map(|l| {
                    let conflicts = ls_registry::config::builtin_spec_for(&id).is_some()
                        || ls_registry::config::builtin_spec_for(l).is_some();
                    let source = if conflicts {
                        if spec.priority < 0 {
                            "builtin"
                        } else {
                            "external-override"
                        }
                    } else {
                        "external-new"
                    };
                    json!({"language": l, "source": source})
                })
                .collect();
            json!({
                "id": id,
                "languages": spec.languages,
                "priority": spec.priority,
                "binary_name": spec.path_only.as_ref().map(|p| p.binary_name.clone()),
                "language_source": per_lang,
            })
        })
        .collect();
    println!(
        "{}",
        json!({
            "ok": true,
            "file": cfg_path.display().to_string(),
            "servers": servers,
            "unregister": "serena-cli ls-use --remove <ID>",
        })
    );
    ExitCode::SUCCESS
}

/// cache_root/<id>/ 下的版本目录（目录名 = version/latest），各记递归字节数。
fn cache_versions(id_dir: &Path) -> Vec<(String, PathBuf, u64)> {
    let Ok(entries) = std::fs::read_dir(id_dir) else {
        return Vec::new();
    };
    let mut v: Vec<(String, PathBuf, u64)> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let bytes = dir_size(&e.path());
            (
                e.file_name().to_string_lossy().into_owned(),
                e.path(),
                bytes,
            )
        })
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// ls-list：内置表全条目 × 实装状态（installed / installed-unroutable（v5 P1-1：
/// 包在盘但无请求路由）/ external-override / not-installed）+ 每条 routable 标志
/// + external 新语言条目 + 总计（installed 数 / 可释放字节）。
fn cmd_ls_list(table: bool) -> ExitCode {
    let cache_root = ls_registry::config::dirs_cache_root();
    let external: std::collections::BTreeMap<String, ls_registry::spec::ServerSpec> =
        ls_registry::config::external_servers_path()
            .map(|p| ls_registry::config::external_entries(&p))
            .unwrap_or_default()
            .into_iter()
            .collect();
    let builtin: Vec<_> = ls_registry::config::builtin_entries();
    let builtin_ids: std::collections::BTreeSet<&str> = builtin.iter().map(|(id, _)| *id).collect();

    let mut servers = Vec::new();
    let mut installed_count = 0usize;
    let mut reclaimable = 0u64;
    for (id, spec) in &builtin {
        let versions = cache_versions(&cache_root.join(id));
        let bytes: u64 = versions.iter().map(|(_, _, b)| *b).sum();
        let overridden = external.get(*id).is_some_and(|s| s.priority >= 0);
        // bd serena-rust-9z0x：override 行显示 external 条目自身 languages——
        // external 是完整条目替换（merge_pick §3），生效路由语言以它为准。此前
        // 显示内置 spec.languages（如注册 [servers.pyright] languages=[python,
        // pyright] 却显示 ["pyright"]），注册时并上的请求语言在清单里蒸发
        // （NitpickAI F1 step4，与运行时行为相悖）。
        let languages: Vec<String> = if overridden {
            external.get(*id).map(|s| s.languages.clone()).unwrap_or_else(|| spec.languages.clone())
        } else {
            spec.languages.clone()
        };
        // blindtest v5 P1-1：installed 只代表缓存包在盘；routable = 请求路径存在
        // （adapter / EXT_TABLE / external 注册 / 避撞变体的家族语言），判据源
        // ls_registry::entry_routable。
        let routable = overridden || ls_registry::entry_routable(id, &languages);
        let state = if overridden {
            "external-override"
        } else if !versions.is_empty() && !routable {
            // 诚实态：包在盘但无路由（.rb/.m 类）——避免「installed 实际不可用」
            // 的 13/40 谎报面。
            "installed-unroutable"
        } else if !versions.is_empty() {
            "installed"
        } else {
            "not-installed"
        };
        let mut entry = json!({
            "id": id,
            "languages": languages,
            "state": state,
            "routable": routable,
        });
        if overridden {
            entry["binary_name"] = external[*id]
                .path_only
                .as_ref()
                .map(|p| json!(p.binary_name.clone()))
                .unwrap_or(serde_json::Value::Null);
        }
        if !versions.is_empty() {
            installed_count += 1;
            reclaimable += bytes;
            entry["versions"] = json!(
                versions
                    .iter()
                    .map(|(v, p, b)| json!({
                        "version": v,
                        "path": p.display().to_string(),
                        "bytes": b,
                    }))
                    .collect::<Vec<_>>()
            );
        }
        if state == "not-installed" {
            entry["install_hint"] = json!(format!("serena-cli install {id}"));
        }
        if state == "installed-unroutable" {
            entry["hint"] = json!(
                "package installed but no adapter routes this language; requests will fail — \
                 use ls-use to register an external server"
            );
        }
        servers.push(entry);
    }
    // external 新语言条目（id 不在内置表）——与 ls-use --list 条目同形。
    let external_new: Vec<serde_json::Value> = external
        .iter()
        .filter(|(id, _)| !builtin_ids.contains(id.as_str()))
        .map(|(id, s)| {
            json!({
                "id": id,
                "languages": s.languages,
                "binary_name": s.path_only.as_ref().map(|p| p.binary_name.clone()),
                // ls-use 注册即生效路由（external extensions 参与扩展名解析）。
                "routable": true,
            })
        })
        .collect();

    if table {
        println!("{:<24}{:<18}{:<20}DETAIL", "ID", "LANGUAGES", "STATE");
        for e in &servers {
            let id = e["id"].as_str().unwrap_or("");
            let langs = e["languages"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let state = e["state"].as_str().unwrap_or("");
            let detail = match state {
                "installed" | "installed-unroutable" => {
                    let versions = e["versions"]
                        .as_array()
                        .map(|vs| {
                            vs.iter()
                                .map(|v| {
                                    format!(
                                        "{} ({} bytes)",
                                        v["version"].as_str().unwrap_or("?"),
                                        v["bytes"].as_u64().unwrap_or(0)
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default();
                    if state == "installed-unroutable" {
                        format!("{versions} [no adapter; requests will fail]")
                    } else {
                        versions
                    }
                }
                "external-override" => e["binary_name"].as_str().unwrap_or("").to_string(),
                _ => e["install_hint"].as_str().unwrap_or("").to_string(),
            };
            println!("{:<24}{:<18}{:<20}{}", id, langs, state, detail);
        }
        for e in &external_new {
            let id = e["id"].as_str().unwrap_or("");
            let langs = e["languages"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let bin = e["binary_name"].as_str().unwrap_or("");
            println!("{:<24}{:<18}{:<20}{}", id, langs, "external-new", bin);
        }
        println!(
            "\ninstalled {}/{}; reclaimable {reclaimable} bytes; external-new: {}",
            installed_count,
            servers.len(),
            external_new.len()
        );
        return ExitCode::SUCCESS;
    }
    println!(
        "{}",
        json!({
            "ok": true,
            "cache_root": cache_root.display().to_string(),
            "total": servers.len(),
            "installed": installed_count,
            "reclaimable_bytes": reclaimable,
            "servers": servers,
            "external_new": external_new,
        })
    );
    ExitCode::SUCCESS
}

/// ls-remove 失败语义（cmd 层映射 9 错误码 + exit code）。
#[derive(Debug)]
enum LsRemoveFail {
    BadArgs(String),
    NotFound(String),
    Internal(String),
}

/// 只删 cache_root/<id>/：id 字符集闸（防 `..`/分隔符）+ canonicalize 后必须仍在
/// cache root 下（防软链/符号链接越界，复用 ensure_within_cache_root）。
/// 返回 (删除路径, 释放字节)。
fn ls_remove_dir(root: &Path, id: &str) -> Result<(PathBuf, u64), LsRemoveFail> {
    let valid = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        return Err(LsRemoveFail::BadArgs(format!(
            "invalid server id `{id}` (expected [A-Za-z0-9_-])"
        )));
    }
    let dir = root.join(id);
    if !dir.is_dir() {
        let installed = installed_ids(root).join(", ");
        return Err(LsRemoveFail::NotFound(format!(
            "`{id}` not found under cache root {}; installed: [{installed}]",
            root.display()
        )));
    }
    let dir = ls_registry::config::ensure_within_cache_root(root, &dir)
        .map_err(LsRemoveFail::Internal)?;
    let bytes = dir_size(&dir);
    std::fs::remove_dir_all(&dir)
        .map_err(|e| LsRemoveFail::Internal(format!("remove `{}`: {e}", dir.display())))?;
    Ok((ls_registry::config::strip_unc(dir), bytes))
}

/// cache root 下的已装 id 清单（目录名，排序）。`undo` 是事务栈目录非 LS 安装。
fn installed_ids(root: &Path) -> Vec<String> {
    const NON_SERVER_DIRS: [&str; 1] = ["undo"];
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut v: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !NON_SERVER_DIRS.iter().any(|x| x == n))
        .collect();
    v.sort();
    v
}

fn cmd_ls_remove(id: &str) -> ExitCode {
    let cache_root = ls_registry::config::dirs_cache_root();
    let ext_registered = ls_registry::config::external_servers_path()
        .map(|p| {
            ls_registry::config::external_entries(&p)
                .iter()
                .any(|(eid, _)| eid == id)
        })
        .unwrap_or(false);
    match ls_remove_dir(&cache_root, id) {
        Ok((path, bytes)) => {
            if ext_registered {
                eprintln!(
                    "note: `{id}` is also registered in external-servers.toml; unregister with `serena-cli ls-use --remove {id}`"
                );
            }
            println!(
                "{}",
                json!({"ok": true, "removed": id, "path": path.display().to_string(), "bytes_freed": bytes})
            );
            ExitCode::SUCCESS
        }
        Err(LsRemoveFail::BadArgs(m)) => {
            local_err_json("BAD_ARGS", &m);
            ExitCode::from(2)
        }
        Err(LsRemoveFail::NotFound(m)) => {
            if ext_registered {
                local_err_json(
                    "BAD_ARGS",
                    &format!(
                        "`{id}` has no serena-managed cache dir; it is registered in external-servers.toml — unregister with `serena-cli ls-use --remove {id}`"
                    ),
                );
            } else {
                local_err_json("BAD_ARGS", &m);
            }
            ExitCode::from(2)
        }
        Err(LsRemoveFail::Internal(m)) => {
            local_err_json("INTERNAL", &m);
            ExitCode::from(3)
        }
    }
}

/// cargo metadata 健康检查（bd xzb-doctor 的 doctor 侧）：项目目录落在别的
/// workspace 内时 `cargo metadata` 失败（"current package believes it's in a
/// workspace"），RA FetchWorkspaceError 令 def/refs/hover 等语义工具静默返空
/// ——在此提前暴露根因。检查目标 = `--project` 或 cwd。
/// bd serena-rust-0x0：非 cargo 项目（npm/pyproject 等）按其清单单独判读，
/// 不再误跑 cargo metadata 报"cargo 不可执行"噪音。
fn check_cargo_metadata(project_root: &Path) -> supervisor::doctor::Check {
    let mk = |status: supervisor::doctor::Status, detail: String, hint: Option<String>| {
        supervisor::doctor::Check {
            category: "workspace",
            id: "cargo_metadata",
            label: "cargo metadata",
            status,
            detail,
            hint,
        }
    };
    // 非 cargo 清单探测（根直接子级）：命中 → 报清单类型并跳过 cargo 分支。
    const NON_CARGO: [(&str, &str); 4] = [
        ("package.json", "npm/node"),
        ("pyproject.toml", "python (PEP 621)"),
        ("requirements.txt", "python (pip)"),
        ("go.mod", "go modules"),
    ];
    if !project_root.join("Cargo.toml").is_file() {
        if let Some((manifest, kind)) = NON_CARGO
            .iter()
            .find(|(f, _)| project_root.join(f).is_file())
        {
            return mk(
                supervisor::doctor::Status::Ok,
                format!("非 cargo 项目（{kind}，{manifest}）；跳过 cargo metadata"),
                None,
            );
        }
        // Cargo.toml 缺席且无已知清单：多语言混装项目里 marker 可能只是不在此层
        // —— Warn 提示而非 Miss，避免对目录式项目误报。
        return mk(
            supervisor::doctor::Status::Warn,
            "no Cargo.toml / package.json / pyproject.toml at project root".into(),
            Some(
                "Rust 项目请确认 Cargo.toml 在 --project 指向的目录；\
                 其他语言项目可忽略（LS 语义可用性以实际工具调用为准）"
                    .into(),
            ),
        );
    }
    // ponytail: 无超时——std Command 无内建超时；--no-deps 冷缓存秒级，与网络探活同级可接受。
    match std::process::Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(project_root)
        .output()
    {
        Ok(o) if o.status.success() => mk(
            supervisor::doctor::Status::Ok,
            format!("manifest 解析正常（{}）", project_root.display()),
            None,
        ),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let first = stderr.lines().next().unwrap_or_default().to_string();
            mk(
                supervisor::doctor::Status::Miss,
                if first.is_empty() {
                    "exit != 0".into()
                } else {
                    first
                },
                Some(
                    "workspace 归属冲突会使 LSP 语义工具静默返空：把项目移出外部 \
                     workspace 目录，或在其 Cargo.toml 追加空 [workspace] 表"
                        .into(),
                ),
            )
        }
        Err(e) => mk(
            supervisor::doctor::Status::Warn,
            format!("cargo 不可执行: {e}"),
            Some("非 Rust 项目可忽略此项；Rust 项目请确认 cargo 在 PATH".into()),
        ),
    }
}

/// bd serena-rust-9z0x：external-servers.toml 注册感知的 doctor 后处理——
/// external 是完整条目替换，被覆盖条目（priority ≥ 0）的「not on PATH」MISS
/// 判据失真：改看注册二进制是否在盘（ls-use 恒写 canonical 绝对路径）。在盘 →
/// OK 标注注册来源；缺盘 → 保持 MISS 但 detail 指向注册残链（可手修）。
fn apply_external_registrations(report: &mut supervisor::doctor::DoctorReport) {
    let Some(cfg) = ls_registry::config::external_servers_path() else {
        return;
    };
    for (id, spec) in ls_registry::config::external_entries(&cfg) {
        if spec.priority < 0 {
            continue; // 负 priority = 显式让位内置，doctor 判定维持内置
        }
        let Some(po) = spec.path_only.as_ref() else {
            continue; // 非 path_only 注册走原装态判据（缓存/PATH 探测已覆盖）
        };
        let on_disk = Path::new(&po.binary_name).is_file();
        for c in report
            .checks
            .iter_mut()
            .filter(|c| c.category == "ls" && c.id == id)
        {
            if c.status != supervisor::doctor::Status::Miss {
                continue;
            }
            if on_disk {
                c.status = supervisor::doctor::Status::Ok;
                c.detail =
                    format!("registered via external-servers.toml: {}", po.binary_name);
                c.hint = None;
            } else {
                c.detail = format!(
                    "{} (registered via external-servers.toml but binary missing on disk)",
                    c.detail
                );
            }
        }
    }
}

/// blindtest v5 P1-1：installed-unroutable 解释行——包已装但无请求路由的条目
/// （.rb/.m 类），doctor 不解释则「installed」与运行时 BAD_ARGS 三方相悖
/// （serena-rust-9z0x 同款三方一致性问题）。doctor 的 ls 检查 id 是二进制名/
/// 规格 id 精选集，非全目录逐条——聚合单行覆盖全部 unroutable 条目。
fn annotate_unroutable_installed(report: &mut supervisor::doctor::DoctorReport) {
    let cache_root = ls_registry::config::dirs_cache_root();
    let unroutable: Vec<String> = ls_registry::config::builtin_entries()
        .into_iter()
        .filter(|(id, spec)| {
            let has_cache = !cache_versions(&cache_root.join(id)).is_empty();
            has_cache && !ls_registry::entry_routable(id, &spec.languages)
        })
        .map(|(id, _)| id.to_string())
        .collect();
    if unroutable.is_empty() {
        return;
    }
    report.checks.push(supervisor::doctor::Check {
        category: "ls",
        id: "routability",
        label: "installed entries without adapter route",
        status: supervisor::doctor::Status::Warn,
        detail: format!(
            "{}: package installed but no adapter routes these languages — file requests \
             fail with \"registered but no adapter\"; use `serena-cli ls-use` to register \
             an external server if you need them",
            unroutable.join(", ")
        ),
        hint: None,
    });
}

/// `doctor` 子命令：6 类体检 + 可选 --fix 自动装 MISS 的 LS。
async fn cmd_doctor(json: bool, fix: bool, lock_path: &Path, project_root: &Path) -> ExitCode {
    let mut report = supervisor::doctor::run_all(lock_path);
    // workspace 类在 CLI 侧追加：检查目标（--project/cwd）是 CLI 会话概念，
    // supervisor::doctor 不感知（分层：doctor 库只做环境探测，ARCH §1）。
    report.checks.push(check_cargo_metadata(project_root));
    // bd serena-rust-9z0x：external 注册感知（NitpickAI F1 step5——doctor 无视
    // 已注册条目，`[MISS] pyright not on PATH` 与 ls-list/运行时三方相悖）。
    apply_external_registrations(&mut report);
    // blindtest v5 P1-1：installed-unroutable 解释行。
    annotate_unroutable_installed(&mut report);
    // 可选：--fix 尝试装 MISS 的 server 类别条目
    if fix {
        for c in &report.checks {
            if c.status == supervisor::doctor::Status::Miss && c.category == "ls" {
                // bd serena-rust-9z0x：已 external 注册的条目不自动安装——注册
                // 二进制才是生效源，装内置条目等于悄悄改写用户注册意图。
                if ls_registry::config::spec_source(c.id) == Some("external") {
                    continue;
                }
                // `id` 是 server name（如 rust-analyzer）—— 不一定在 servers.toml
                // （如 csharp-ls 是 dotnet tool）；只对 spec_for 能命中的跑 ensure_launch。
                if ls_registry::config::spec_for(c.id).is_some() {
                    let _ = ls_registry::config::ensure_launch(c.id, None, true, false);
                }
            }
        }
    }
    if json {
        match serde_json::to_string_pretty(&report) {
            Ok(s) => {
                println!("{s}");
            }
            Err(e) => {
                eprintln!("json serialize failed: {e}");
                return ExitCode::from(3);
            }
        }
    } else {
        print!("{}", supervisor::doctor::format_text(&report));
        // bd serena-rust-0z9 / xwh：用户可写配置路径就地可见（与 README
        // "Config file locations" 同源；--json 模式不加行，保持可解析）。
        if let Some(p) = ls_registry::config::external_servers_path() {
            println!("config: external servers (ls-use registry) — {}", p.display());
        }
        if let Some(p) = ls_registry::config::user_config_path() {
            println!("config: user overrides (config.toml) — {}", p.display());
        }
    }
    ExitCode::from(report.exit_code())
}

/// bd serena-rust-0x0：doctor workspace 检查的非 cargo 清单分支（不拉 cargo，
/// 纯 fs 探测路径可单测）。
#[cfg(test)]
mod doctor_workspace_tests {
    use super::*;

    #[test]
    fn workspace_check_reports_non_cargo_manifests_without_running_cargo() {
        let tmp =
            std::env::temp_dir().join(format!("serena-doctor-ws-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("package.json"), "{}").unwrap();
        let c = check_cargo_metadata(&tmp);
        assert_eq!(c.status, supervisor::doctor::Status::Ok);
        assert!(c.detail.contains("npm/node"), "{}", c.detail);

        let py = std::env::temp_dir()
            .join(format!("serena-doctor-ws-py-{}", std::process::id()));
        std::fs::create_dir_all(&py).unwrap();
        std::fs::write(py.join("pyproject.toml"), "[project]").unwrap();
        let c = check_cargo_metadata(&py);
        assert_eq!(c.status, supervisor::doctor::Status::Ok);
        assert!(c.detail.contains("python"), "{}", c.detail);

        std::fs::remove_dir_all(&tmp).ok();
        std::fs::remove_dir_all(&py).ok();
    }

    #[test]
    fn workspace_check_warns_on_manifest_less_root() {
        let tmp =
            std::env::temp_dir().join(format!("serena-doctor-ws-empty-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let c = check_cargo_metadata(&tmp);
        assert_eq!(c.status, supervisor::doctor::Status::Warn);
        assert!(c.detail.contains("no Cargo.toml"), "{}", c.detail);
        std::fs::remove_dir_all(&tmp).ok();
    }
}
/// `status` 子命令。
///
/// bd 30m：纯探测语义——只读 lock + TCP 探活 + GET /status，**永不 lazy-spawn**
/// （哨兵/无残留基线依赖 status 无副作用：daemon 不在时报 not-running 而非拉起；
/// 回归测试 `status_tests::status_absent_daemon_never_spawns`）。
/// bd b09i：daemon 侧已结构化 loaded_ls（{lang, sessions}），CLI 直透不再折叠。
async fn cmd_status(lock_path: &Path) -> ExitCode {
    let entry = match daemon::lockfile::read(lock_path) {
        Ok(Some(e)) if probe(e.port) => {
            // bd fakewait：drain 窗口撞上 status——照实报告是正在自杀的旧
            // daemon（旧 uptime）。等退净后接管或拉新，与 wait-ready 同链。
            if alive_but_draining(&e).await {
                match wait_drain_outcome(lock_path, e.port, DRAIN_TAKEOVER_WAIT).await {
                    Ok(DrainOutcome::Attach(e2)) => e2,
                    Ok(DrainOutcome::ReadyToSpawn) => match ensure_daemon(lock_path).await {
                        Ok(_) => match daemon::lockfile::read(lock_path) {
                            Ok(Some(e2)) => e2,
                            other => {
                                eprintln!("status: daemon did not come up after drain takeover: {other:?}");
                                return ExitCode::from(3);
                            }
                        },
                        Err(e) => {
                            eprintln!("status: {e}");
                            return ExitCode::from(3);
                        }
                    },
                    Err(e) => {
                        eprintln!("status: {e}");
                        return ExitCode::from(3);
                    }
                }
            } else {
                e
            }
        }
        _ => {
            println!("daemon: not running");
            return ExitCode::from(1);
        }
    };
    let client = http_client();
    match client
        .get(format!("http://127.0.0.1:{}/status", entry.port))
        .header("X-Serena-Token", &entry.token)
        .timeout(MGMT_TIMEOUT)
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.unwrap_or(json!(null));
            print_json(&body).expect("print status");
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("status probe failed: {other:?}");
            ExitCode::from(3)
        }
    }
}

/// `project-info` 子命令（bd v3yv）：git 风格项目元信息 + daemon/LS 加载状态。
///
/// 纯探测语义（同 `status`）：读 lock + TCP 探活 + GET /status + 本地 .git 解析，
/// **永不 lazy-spawn**。git meta 只读 `.git/HEAD`（分支 + 短 sha；worktree 链接与
/// packed-refs 兜底解析），不跑 git 子进程。
async fn cmd_project_info(lock_path: &Path, project: Option<PathBuf>) -> ExitCode {
    let daemon_entry = daemon::lockfile::read(lock_path).unwrap_or(None);
    let daemon_online = daemon_entry
        .as_ref()
        .map(|e| probe(e.port))
        .unwrap_or(false);

    // status 拉取（loaded_ls / active_project / daemon_version / pid）。
    let mut status: Option<serde_json::Value> = None;
    if let (Some(e), true) = (&daemon_entry, daemon_online) {
        let client = http_client();
        if let Ok(resp) = client
            .get(format!("http://127.0.0.1:{}/status", e.port))
            .header("X-Serena-Token", &e.token)
            .timeout(MGMT_TIMEOUT)
            .send()
            .await
            && resp.status().is_success()
            && let Ok(body) = resp.json::<serde_json::Value>().await
        {
            status = Some(body);
        }
    }

    // root 解析顺序：--project > daemon active_project > 当前目录。
    let root = project
        .or_else(|| {
            status
                .as_ref()
                .and_then(|s| s["active_project"].as_str())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let mut out = json!({
        "project_root": root.display().to_string(),
        "daemon": {
            "running": daemon_online,
            "version": status.as_ref().and_then(|s| s["daemon_version"].as_str()),
            "pid": status.as_ref().and_then(|s| s["pid"].as_u64()),
        },
        // loaded = daemon 在线且该 root 有至少一门 LS 会话（加载状态）。
        "ls_loaded": status
            .as_ref()
            .map(|s| s["loaded_ls"].as_array().is_some_and(|a| !a.is_empty()))
            .unwrap_or(false),
        "loaded_ls": status
            .as_ref()
            .and_then(|s| s["loaded_ls"].as_array())
            .cloned()
            .unwrap_or_default(),
    });

    // git 风格 meta：分支 + HEAD 短 sha（.git 目录直读，不跑子进程）。
    let dotgit = root.join(".git");
    let head_text = if dotgit.is_dir() {
        std::fs::read_to_string(dotgit.join("HEAD")).ok()
    } else {
        None
    };
    if let Some(head) = head_text {
        let branch = head
            .trim()
            .strip_prefix("ref: refs/heads/")
            .unwrap_or("(detached)")
            .to_string();
        let sha = resolve_git_head_sha(&dotgit, head.trim());
        let mut git = json!({ "branch": branch });
        if let Some(sha) = sha {
            git["head"] = json!(sha.chars().take(9).collect::<String>());
        }
        out["git"] = git;
    }

    print_json(&out).expect("print project-info");
    ExitCode::SUCCESS
}

/// `change-history` 子命令（bd zyrg）：文件/符号级变更历史。
///
/// `git log` 包装：默认 `--follow`（跨 rename 追踪），`--symbol` 走 `-L :sym:file`
/// 符号级历史。记录头用 `\x01<H>\t<ct>\t<s>` 切分（subject 可含任意字符，同
/// supervisor ct_recent_activity 的 porcelain 形态）。git 缺失 / 非 repo /
/// 超时 → exit 3 + stderr 原因，不静默。
fn cmd_change_history(root: &Path, file: &str, symbol: Option<&str>, max: usize) -> ExitCode {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C").arg(root).arg("log").arg("-n").arg(max.to_string());
    match symbol {
        Some(sym) => {
            // arg 分开传："-L :sym:file" 合成单 argv 会让 git 把值解析成
            // " :sym:file"（前导空格），:funcname:file 匹配直接 fatal。
            cmd.arg("-L").arg(format!(":{sym}:{file}"));
        }
        None => {
            cmd.arg("--follow");
        }
    }
    // --pretty 必须在 `--`（pathspec 分隔）之前，否则被吃成 pathspec。
    cmd.arg("--pretty=format:%x01%H%x09%ct%x09%s");
    if symbol.is_none() {
        cmd.arg("--").arg(file);
    }
    let out = match cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            eprintln!("git log spawn failed: {e} (git not on PATH?)");
            return ExitCode::from(3);
        }
    };
    if !out.status.success() {
        eprintln!("git log failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        return ExitCode::from(3);
    }
    // -L 模式 format 行后附 patch 体；\x01 切分天然只取记录头，patch 忽略。
    let commits: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .split('\x01')
        .filter_map(|rec| {
            let head = rec.lines().next()?;
            let mut parts = head.splitn(3, '\t');
            let sha = parts.next()?.trim();
            if sha.is_empty() {
                return None;
            }
            let ts = parts.next()?.parse::<u64>().unwrap_or(0);
            Some(json!({ "sha": sha, "committed_at": ts, "subject": parts.next().unwrap_or_default() }))
        })
        .collect();
    let mut out_json = json!({ "file": file, "commits": commits });
    if let Some(sym) = symbol {
        out_json["symbol"] = json!(sym);
    }
    print_json(&out_json).expect("print change-history");
    ExitCode::SUCCESS
}

/// 解析 HEAD 指向的 commit sha：直接 ref 文件 → packed-refs 兜底 → detached sha。
fn resolve_git_head_sha(dotgit: &Path, head: &str) -> Option<String> {
    if let Some(sha) = head.strip_prefix("ref: ") {
        let ref_file = dotgit.join(sha);
        if let Ok(s) = std::fs::read_to_string(ref_file) {
            return Some(s.trim().to_string());
        }
        // packed-refs：`<sha> <refname>` 行匹配。
        let packed = std::fs::read_to_string(dotgit.join("packed-refs")).ok()?;
        let needle = format!(" {sha}");
        packed
            .lines()
            .find(|l| l.ends_with(&needle))
            .and_then(|l| l.split_whitespace().next())
            .map(str::to_string)
    } else {
        // detached HEAD：HEAD 本身就是 sha。
        Some(head.to_string())
    }
}

/// `stop-all` 子命令：POST /shutdown + 删 lock；锁缺失时按端口探活兜底清残留（bd 3ab）。
async fn cmd_stop_all(lock_path: &Path) -> ExitCode {
    let Some(entry) = daemon::lockfile::read(lock_path).unwrap_or(None) else {
        println!("daemon: not running");
        // bd 3ab：锁已删但残留 daemon 仍占 7860 时，这里是管辖真空——探活兜底。
        reap_residual_listener(7860);
        return ExitCode::SUCCESS;
    };
    let client = http_client();
    let res = client
        .post(format!("http://127.0.0.1:{}/shutdown", entry.port))
        .header("X-Serena-Token", &entry.token)
        .timeout(MGMT_TIMEOUT)
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => {
            println!(
                "daemon draining (pid {}); lock will be removed by reaper",
                entry.pid
            );
            ExitCode::SUCCESS
        }
        Ok(r) => {
            // daemon 还在（draining 收尾 / lock 易主）：lock 留给仲裁路径接管清理。
            // CLI 无归属凭据，无条件删会制造孤儿（bd y2y）。
            eprintln!(
                "shutdown probe failed: HTTP {}; lock left for lazy-spawn arbitration",
                r.status()
            );
            ExitCode::from(3)
        }
        Err(e) if e.is_connect() => {
            // 锁在但端口连不上 = daemon 已死：对齐 status 的人话（bd serena-rust-6i2l）；
            // 「无 daemon 在跑」已达成 → 0，lock 留给 lazy-spawn 仲裁删。
            println!("daemon: not running (stale lock left for lazy-spawn arbitration)");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("shutdown probe failed: {e}; lock left for lazy-spawn arbitration");
            ExitCode::from(3)
        }
    }
}

/// bd 3ab：锁不在但端口仍有 listener = 残留 daemon（lazy-spawn 被 kill 脱管/超时
/// 截断的产物），后续 lazy-spawn bind 失败或请求打到老进程。按端口反查 PID：
/// 是自家映像才终止并复测确认；反查失败/非自家进程只告警不动手（防误杀）。
fn reap_residual_listener(port: u16) {
    if !port_has_listener(port) {
        return;
    }
    eprintln!("warning: lock absent but 127.0.0.1:{port} still has a listener (residual daemon?)");
    let Some(pid) = listener_pid_on_port(port) else {
        eprintln!("{}", residual_warn_msg(None, port));
        return;
    };
    match pid_process_name(pid).as_deref() {
        Some(name) if is_serena_daemon_image(name) => {
            if kill_process(pid) {
                // listen socket 释放非原子，短暂等待后复测。
                std::thread::sleep(REAP_RECHECK_DELAY);
                if port_has_listener(port) {
                    eprintln!("{}", residual_warn_msg(Some(pid), port));
                } else {
                    eprintln!("residual daemon pid={pid} on port {port} terminated");
                }
            } else {
                eprintln!("{}", residual_warn_msg(Some(pid), port));
            }
        }
        Some(other) => {
            eprintln!(
                "warning: port {port} held by non-daemon process {other} (pid {pid}); not killing"
            )
        }
        None => eprintln!("{}", residual_warn_msg(Some(pid), port)),
    }
}

/// loopback 端口是否有 listener（TCP connect 探测，500ms 封顶）。
fn port_has_listener(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, RESIDUAL_PROBE_TIMEOUT).is_ok()
}

/// 按端口反查 LISTENING 进程 PID；反查工具缺失/无命中 → None。
fn listener_pid_on_port(port: u16) -> Option<u32> {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("netstat")
            .args(["-ano", "-p", "TCP"])
            .output()
            .ok()?;
        parse_netstat_listeners(&String::from_utf8_lossy(&out.stdout), port)
    }
    #[cfg(not(windows))]
    {
        let out = std::process::Command::new("lsof")
            .args(["-t", "-nP", "-sTCP:LISTEN", &format!("-iTCP:{port}")])
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()?
            .trim()
            .parse()
            .ok()
    }
}

/// `netstat -ano -p TCP` 输出按端口反查 LISTENING PID（纯函数，跨平台单测锚）。
/// 行形态：`  TCP    127.0.0.1:7860    0.0.0.0:0    LISTENING    4092`。
fn parse_netstat_listeners(output: &str, port: u16) -> Option<u32> {
    let suffix = format!(":{port}");
    output.lines().find_map(|line| {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() >= 5
            && cols[0].eq_ignore_ascii_case("tcp")
            && cols[3].eq_ignore_ascii_case("LISTENING")
            && cols[1].ends_with(&suffix)
        {
            cols[4].parse().ok()
        } else {
            None
        }
    })
}

/// PID 对应进程映像名（tasklist/ps）；查询失败或无匹配 → None。
#[cfg(windows)]
fn pid_process_name(pid: u32) -> Option<String> {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().next()?;
    // 无匹配时 tasklist 输出本地化提示语，非 CSV 引号行。
    if !line.starts_with('"') {
        return None;
    }
    line.trim_matches('"')
        .split("\",\"")
        .next()
        .map(str::to_string)
}

#[cfg(not(windows))]
fn pid_process_name(pid: u32) -> Option<String> {
    let out = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()?
        .trim()
        .to_string();
    (!name.is_empty()).then_some(name)
}

/// 只对自家映像动手：现名 serena-cli（兼容更名前 cli.exe 旧残留）。
fn is_serena_daemon_image(name: &str) -> bool {
    ["serena-cli.exe", "serena-cli", "cli.exe", "cli"]
        .iter()
        .any(|n| name.eq_ignore_ascii_case(n))
}

/// 终止进程：Windows taskkill /F；Unix kill -9。成功与否看 exit code。
#[cfg(windows)]
fn kill_process(pid: u32) -> bool {
    std::process::Command::new("taskkill")
        .args(["/F", "/PID", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn kill_process(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 无法自动清理时的 stderr 告警（pid 反查失败走手动 netstat 分支）。
fn residual_warn_msg(pid: Option<u32>, port: u16) -> String {
    match pid {
        Some(p) => format!("残留 daemon pid={p} 仍占端口 {port}，请手动 taskkill /F /PID {p}"),
        None => format!(
            "端口 {port} 仍有 listener 但反查 PID 失败，请手动 netstat -ano 查占并 taskkill"
        ),
    }
}

/// wire 成功 data 是否为空结果：`items:[]` envelope 或裸 `[]`。
/// 无 items 且非数组的形态（hover 对象等）视为非空——O2 hint 只管集合型工具。
fn payload_is_empty(v: &serde_json::Value) -> bool {
    match v.get("items") {
        Some(items) => items.as_array().is_some_and(|a| a.is_empty()),
        None => v.as_array().is_some_and(|a| a.is_empty()),
    }
}

/// warning 文本是否暗示"语义层未就绪"（与 [`hover_ready`]/`def_ready` 的
/// we0 判据同源）。`may not be ready` / `type analysis` / `index warming`
/// / `results may be partial` —— 仅这四类语义就绪关键词才应触发 `[hint] index
/// warming` 提示，避免项目切换等无关 warning 误导 agent「重试/等就绪」。
fn warning_suggests_index_warming(w: &str) -> bool {
    let l = w.to_lowercase();
    l.contains("may not be ready")
        || l.contains("type analysis")
        || l.contains("index warming")
        || l.contains("results may be partial")
}

/// Print JSON pretty; map serde_json errors to ToolError::Serialize (uniform exit 3 path).
fn print_json(v: &serde_json::Value) -> Result<(), ToolError> {
    println!(
        "{}",
        serde_json::to_string_pretty(v).map_err(|e| ToolError::Serialize(e.into()))?
    );
    Ok(())
}
// ============== shell 模式 (Task 18) ==============

/// 长连接 shell：stdin/stdout JSONL。
///
/// 输入（一行 JSON）：
///   `{"id":<n>,"cmd":"<tool>","args":{...}}`  —— 调用 LSP 工具
///   `{"id":<n>,"cmd":"status"}`                —— daemon 状态
///   `{"id":<n>,"cmd":"exit"}`                  —— 退出 shell
///
/// 输出（一行 JSON）：
///   `{"id":<n>,"ok":true,"data":<v>}`
///   `{"id":<n>,"ok":false,"error":<msg>}`
///   EOF / `exit` 后退出 0。
async fn cmd_shell(cli: &Cli) -> ExitCode {
    let lock_path = daemon::serve::default_lock_path();
    let mut base_token = match ensure_daemon(&lock_path).await {
        Ok(b) => b,
        Err(e) => {
            // shell 启动失败也要在 stdout 留 JSON，便于 agent 解析。
            println!(r#"{{"id":null,"ok":false,"error":"{}"}}"#, json_escape(&e));
            return ExitCode::from(3);
        }
    };

    let client = http_client();
    use tokio::io::{AsyncBufReadExt, BufReader};
    let project_root = resolve_project_root(cli.project.clone());
    let mut lines = BufReader::new(tokio::io::stdin()).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            // 空输入回 usage 提示（bd djo）：协议通道内只出合法 JSON 帧。
            println!(
                "{}",
                json!({"id": null, "ok": false, "error": "empty input; usage: {\"id\":<n>,\"cmd\":\"<tool|status|exit>\",\"args\":{...}}; see: serena-cli shell --help"})
            );
            continue;
        }

        // 解析 input。
        let input: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                println!(
                    r#"{{"id":null,"ok":false,"error":"bad json: {}"}}"#,
                    json_escape(&e.to_string())
                );
                continue;
            }
        };

        let id = input.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let cmd = input.get("cmd").and_then(|v| v.as_str()).unwrap_or("");

        // exit: 退出。
        if cmd == "exit" {
            println!(
                r#"{{"id":{},"ok":true,"data":null,"bye":true}}"#,
                serde_json::to_string(&id).unwrap_or("null".into())
            );
            break;
        }

        // 处理单条命令。
        let resp = dispatch_shell_cmd(
            &client,
            &mut base_token,
            &lock_path,
            &project_root,
            cmd,
            input.get("args").cloned().unwrap_or(json!({})),
        )
        .await;
        println!("{}", resp_with_id(&id, resp));
    }

    ExitCode::SUCCESS
}

/// lazy-spawn daemon 子进程并等就绪，返回 base url。ensure_daemon 与
/// forward_or_spawn 共用的 spawn 分支（不删 lock——daemon 子进程的 lock 仲裁
/// 会带宽限接管，CLI 无归属凭据先删会误伤启动中/易主 lock）。
async fn spawn_and_adopt(lock_path: &Path) -> Result<String, String> {
    let (port, child_pid) = spawn_daemon_child()?;
    wait_ready(port, SPAWN_WAIT).await?;
    // bd dbx1：3 并发 lazy-spawn 时 OS bind 排他只能 1 赢；败家子进程
    // bind 失败立即退但都连到胜家 :7860 listener 拿到 200。日志归属必须
    // 等到 lock.pid 反查——胜家 = 自己的子进程 PID 才算"spawn 成功"，
    // 败家走 attach 路径、stderr 不打 "lazy-spawned" 字样。
    if own_child_won(lock_path, child_pid) {
        tracing::info!(port, child_pid, "lazy-spawned daemon child; daemon ready");
    } else {
        tracing::info!(
            port,
            child_pid,
            "attached to peer-spawned daemon (lost bind race)"
        );
    }
    Ok(format!("http://127.0.0.1:{port}"))
}

/// 探活命中后的语义复查：GET /status，body `draining:true` = daemon 正在
/// ShutdownDraining（bd fakewait：drain 窗口内 listener 仍 accept，新工具请求
/// 必 503）。拿不到明确 draining:true 的形态（非 200 / 非 JSON / 请求失败）一律
/// false 保守判活——保持既有探活语义，宁可走 503 老路不误杀健康 daemon。
async fn alive_but_draining(entry: &daemon::lockfile::LockEntry) -> bool {
    let client = http_client();
    let Ok(resp) = client
        .get(format!("http://127.0.0.1:{}/status", entry.port))
        .header("X-Serena-Token", &entry.token)
        .timeout(Duration::from_secs(1))
        .send()
        .await
    else {
        return false;
    };
    if !resp.status().is_success() {
        return false;
    }
    resp.json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|b| b.get("draining").and_then(|d| d.as_bool()))
        .unwrap_or(false)
}

/// drain 接管等待的出口。
#[derive(Debug)]
enum DrainOutcome {
    /// lock 存在、daemon 活着且不在 draining（等待期被并发 CLI 接管，直接用）。
    Attach(daemon::lockfile::LockEntry),
    /// lock 消失且端口无 listener——可以 spawn 接管。
    ReadyToSpawn,
}

/// 等 draining 老 daemon 退净（finish_shutdown = 删 lock → process::exit，
/// 两事件毫秒级先后；端口的黑洞窗口只在 drain_window 内）。`wait` 上限必须
/// ≥ daemon 侧 drain_window（serve.rs 15s，常量不跨 crate 暴露）。
async fn wait_drain_outcome(
    lock_path: &Path,
    port: u16,
    wait: Duration,
) -> Result<DrainOutcome, String> {
    let deadline = Instant::now() + wait;
    loop {
        match daemon::lockfile::read(lock_path) {
            Ok(Some(e)) => {
                if daemon::lockfile::is_alive_graceful(e.port) && !alive_but_draining(&e).await {
                    return Ok(DrainOutcome::Attach(e));
                }
            }
            Ok(None) => {
                if !port_has_listener(port) {
                    return Ok(DrainOutcome::ReadyToSpawn);
                }
            }
            Err(e) => return Err(format!("read lock: {e}")),
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "old daemon on :{port} still draining after {wait:?}; \
                 retry, or check for a residual listener"
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// 探活 + lazy-spawn，返回 (base_url, token)。
async fn ensure_daemon(lock_path: &Path) -> Result<(String, String), String> {
    let entry = daemon::lockfile::read(lock_path).map_err(|e| format!("read lock: {e}"))?;
    let base = match entry {
        Some(e) if daemon::lockfile::is_alive_graceful(e.port) => {
            // bd fakewait：drain 窗口内老 daemon TCP 探活必中但请求必 503——
            // 半死 daemon 视同将死，等退净后接管，不把请求打进必 503 的 listener。
            if alive_but_draining(&e).await {
                match wait_drain_outcome(lock_path, e.port, DRAIN_TAKEOVER_WAIT).await? {
                    DrainOutcome::Attach(e2) => format!("http://127.0.0.1:{}", e2.port),
                    DrainOutcome::ReadyToSpawn => spawn_and_adopt(lock_path).await?,
                }
            } else {
                format!("http://127.0.0.1:{}", e.port)
            }
        }
        _ => spawn_and_adopt(lock_path).await?,
    };
    let token = read_token_with_retry(lock_path).await?;
    Ok((base, token))
}

/// 单条 shell 命令：HTTP 转发到 daemon。
async fn dispatch_shell_cmd(
    client: &reqwest::Client,
    base_token: &mut (String, String),
    lock_path: &Path,
    project_root: &Path,
    cmd: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, String> {
    dispatch_shell_cmd_with(
        client,
        base_token,
        lock_path,
        project_root,
        cmd,
        args,
        || ensure_daemon(lock_path),
    )
    .await
}

/// 同 dispatch_shell_cmd；`reensure` 为断流自愈探活的注入点（单测 mock connect
/// 失败自愈，BD serena-rust-3bu），生产传 `|| ensure_daemon(lock_path)`。
/// 无参形态避开 `FnOnce(&Path) -> Fut` 的 HRTB 推断陷阱。
async fn dispatch_shell_cmd_with<F, Fut>(
    client: &reqwest::Client,
    base_token: &mut (String, String),
    lock_path: &Path,
    project_root: &Path,
    cmd: &str,
    args: serde_json::Value,
    reensure: F,
) -> Result<serde_json::Value, String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<(String, String), String>>,
{
    // 管理命令。
    if cmd == "status" {
        let resp = client
            .get(format!("{}/status", base_token.0))
            .header("X-Serena-Token", &base_token.1)
            .timeout(MGMT_TIMEOUT)
            .send()
            .await
            .map_err(|e| format!("status: {e}"))?;
        let status = resp.status();
        let data: serde_json::Value = resp.json().await.unwrap_or(json!(null));
        if !status.is_success() {
            return Err(format!("daemon transport {status}: {data}"));
        }
        return Ok(data);
    }

    // LSP 工具：透传到 /tools/{name}。
    let tool = match cmd {
        "overview"
        | "symbol-tree"
        | "hover"
        | "diagnostics"
        | "def"
        | "refs"
        | "containing-symbol"
        | "defining-symbol"
        | "signature-help"
        | "symbol-body"
        | "replace-body"
        | "completion"
        | "search"
        | "find-symbol"
        | "find-implementations"
        | "rename-symbol"
        | "code-action"
        | "format"
        | "format-range"
        | "inlay-hint"
        | "document-highlight"
        | "folding-range"
        | "semantic-tokens"
        | "code-lens"
        | "document-link"
        | "call-hierarchy"
        | "type-hierarchy"
        | "moniker"
        | "workspace-diagnostic"
        | "read-file"
        | "list-dir"
        | "find-file"
        | "find-referencing-symbols"
        | "find-referencing-code-snippets"
        | "replace-text-in-symbol"
        | "insert-text-after-symbol"
        | "insert-text-before-symbol"
        | "delete-text-in-symbol"
        | "safe-delete-symbol"
        | "insert-at-line"
        | "replace-lines"
        | "delete-lines"
        // IDE undo/redo（事务版快照栈）+ 新建文件 —— supervisor 侧 tool 层实现。
        | "create-text-file"
        | "undo"
        | "redo"
        // recipe 批1 地基三原子命令。
        | "test"
        | "diff"
        | "find-test"
        // recipe 批4 编排层单入口。
        | "recipe" => cmd,
        other => return Err(format!("unknown cmd: {other}")),
    };
    let body = json!({
        "project_root": project_root.to_string_lossy(),
        "args": args,
    });
    let send_tool = |base: &str, token: &str| {
        client
            .post(format!("{base}/tools/{tool}"))
            .header("X-Serena-Token", token)
            .json(&body)
            .timeout(FORWARD_TIMEOUT)
            .send()
    };
    let mut resp = match send_with_connect_retry(
        || send_tool(&base_token.0, &base_token.1.clone()),
        // 与压测观察一致：error sending request 覆盖 connect 与 request 两类瞬断形态。
        |e: &reqwest::Error| e.is_connect() || e.is_request(),
    )
    .await
    {
        Ok(resp) => resp,
        // BD serena-rust-3bu：shell 长会话里 daemon 可能已 15min 空闲自退——
        // connect 类错误（退避耗尽仍连不上）重跑 ensure_daemon lazy-spawn 新
        // daemon，换新 (base, token) 重发一次；与转发模式 draining 自愈（g0m）同源。
        Err(e) if e.is_connect() || e.is_request() => {
            let (base, token) = reensure().await.map_err(|he| {
                format!("forward {tool}: {e}; reconnect self-heal failed: {he}")
            })?;
            *base_token = (base, token);
            send_tool(&base_token.0, &base_token.1)
                .await
                .map_err(|e2| format!("forward {tool}: {e2}"))?
        }
        Err(e) => return Err(format!("forward {tool}: {e}")),
    };
    if resp.status() == reqwest::StatusCode::FORBIDDEN
        && let Some(fresh) = refresh_token_if_stale(lock_path, &base_token.1).await
    {
        // daemon 换代后旧 token 过期：已刷新，用新 token 重发一次。
        base_token.1 = fresh;
        resp = send_tool(&base_token.0, &base_token.1)
            .await
            .map_err(|e| format!("forward {tool}: {e}"))?;
    }
    let status = resp.status();
    let payload: serde_json::Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    if !status.is_success() {
        return Err(format!("daemon transport {status}: {payload}"));
    }
    match payload.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => Ok(payload
            .get("data")
            .cloned()
            .unwrap_or(serde_json::Value::Null)),
        _ => Err(payload.get("error").cloned().unwrap_or(payload).to_string()),
    }
}

/// 把 (id, result) 序列化成一行 JSON 输出。
fn resp_with_id(id: &serde_json::Value, r: Result<serde_json::Value, String>) -> String {
    let id_str = serde_json::to_string(id).unwrap_or_else(|_| "null".into());
    match r {
        Ok(data) => format!(r#"{{"id":{},"ok":true,"data":{}}}"#, id_str, data),
        Err(e) => format!(
            r#"{{"id":{},"ok":false,"error":"{}"}}"#,
            id_str,
            json_escape(&e)
        ),
    }
}

/// JSON string 转义（仅控制 + 引号 + 反斜杠 —— 不全但够错误消息用）。
/// 解析 --project：相对 → CWD 拼接 → canonicalize。失败回退原值。
fn resolve_project_root(raw: Option<PathBuf>) -> PathBuf {
    let p = raw.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    dunce::canonicalize(&p).unwrap_or(p)
}

/// bd serena-rust-74b3：项目清单 → 主语言（warm 缺省 LANG 探测）。
/// 只看项目根直接子级；命中序 = 表序（多清单项目取先者）。package.json 单独
/// 出现也报 typescript —— ts/js 同走 tsserver（ls-registry LanguageId 归并）。
fn detect_project_lang(root: &Path) -> Option<&'static str> {
    const MANIFESTS: &[(&str, &str)] = &[
        ("Cargo.toml", "rust"),
        ("pyproject.toml", "python"),
        ("tsconfig.json", "typescript"),
        ("package.json", "typescript"),
    ];
    MANIFESTS
        .iter()
        .find_map(|(f, lang)| root.join(f).is_file().then_some(*lang))
}

#[cfg(test)]
mod warm_lang_detect_tests {
    use super::*;

    #[test]
    fn detect_project_lang_prefers_manifest_order_and_none_without() {
        let tmp = std::env::temp_dir().join(format!("serena-warm-detect-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        assert_eq!(detect_project_lang(&tmp), None, "空目录探测不到");

        std::fs::write(tmp.join("pyproject.toml"), "[project]").unwrap();
        assert_eq!(detect_project_lang(&tmp), Some("python"));

        std::fs::write(tmp.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(
            detect_project_lang(&tmp),
            Some("rust"),
            "表序 = Cargo.toml 先于 pyproject.toml"
        );

        // 子目录里的清单不算（防误探 monorepo 子包）。
        std::fs::create_dir_all(tmp.join("sub")).unwrap();
        std::fs::write(tmp.join("sub").join("package.json"), "{}").unwrap();
        assert_eq!(detect_project_lang(&tmp), Some("rust"));

        std::fs::remove_dir_all(&tmp).ok();
    }
}

/// 批3 可发现性（ADR serena-rust-ai-experience-9.5 批3）：--new-body/--with 互通、
/// warm 位置参双形态、help 首屏速查——三票回归。
#[cfg(test)]
mod discoverability_batch3_tests {
    use super::*;

    // ==== E：--new-body / --with 互通（replace-body ↔ recipe fix-bug）====

    #[test]
    fn replace_body_accepts_new_body_alias_and_canonical_with() {
        use clap::Parser as _;
        for flag in ["--new-body", "--with"] {
            let cli = Cli::try_parse_from([
                "serena-cli",
                "replace-body",
                "a.py",
                "sym",
                flag,
                "NEW",
            ])
            .unwrap_or_else(|e| panic!("{flag}: {e}"));
            let Some(Cmd::ReplaceBody { new_body, .. }) = cli.cmd else {
                panic!("expected replace-body ({flag})");
            };
            assert_eq!(new_body, "NEW", "{flag} 必须归一到 new_body 字段");
        }
    }

    #[test]
    fn recipe_fix_bug_accepts_with_alias_and_canonical_new_body() {
        use clap::Parser as _;
        for flag in ["--with", "--new-body"] {
            let cli = Cli::try_parse_from([
                "serena-cli",
                "recipe",
                "fix-bug",
                "a.py",
                "sym",
                flag,
                "NEW",
            ])
            .unwrap_or_else(|e| panic!("{flag}: {e}"));
            let Some(Cmd::Recipe { new_body, .. }) = cli.cmd else {
                panic!("expected recipe ({flag})");
            };
            assert_eq!(new_body.as_deref(), Some("NEW"), "{flag} 必须落 new_body");
        }
    }

    // ==== warm 位置参双形态（`warm <LANG>` ≡ `warm --lang <LANG>`）====

    #[test]
    fn warm_lang_forms_merges_flag_into_positional() {
        let mut pos = None;
        let mut flag = Some("rust".into());
        warm_lang_forms(&mut pos, &mut flag, None).unwrap();
        assert_eq!(pos.as_deref(), Some("rust"));
        assert!(flag.is_none(), "flag 归一后清空");

        // 前置全局 --lang（`--lang X warm`，global flag 不传播进子命令）第三来源。
        let mut pos = None;
        warm_lang_forms(&mut pos, &mut None, Some("rust")).unwrap();
        assert_eq!(pos.as_deref(), Some("rust"));
    }

    #[test]
    fn warm_lang_forms_keeps_positional_when_others_absent() {
        let mut pos = Some("python".into());
        warm_lang_forms(&mut pos, &mut None, None).unwrap();
        assert_eq!(pos.as_deref(), Some("python"));
    }

    #[test]
    fn warm_lang_forms_rejects_multiple_forms() {
        let mut pos = Some("python".into());
        let mut flag = Some("rust".into());
        let err = warm_lang_forms(&mut pos, &mut flag, None).unwrap_err();
        assert!(err.contains("not multiple forms"), "{err}");

        let mut pos = None;
        let mut flag = Some("rust".into());
        let err = warm_lang_forms(&mut pos, &mut flag, Some("go")).unwrap_err();
        assert!(err.contains("not multiple forms"), "{err}");
    }

    #[test]
    fn warm_flag_form_parses_via_subcommand_lang() {
        use clap::Parser as _;
        // flag 形态：--lang 落子命令字段，位置参保持空（归一由 cli_main 接线完成）。
        let cli = Cli::try_parse_from(["serena-cli", "warm", "--lang", "rust"]).unwrap();
        let Some(Cmd::Warm { lang, lang_flag, .. }) = cli.cmd else {
            panic!("expected warm");
        };
        assert!(lang.is_none(), "flag 形态不得占位置参");
        assert_eq!(lang_flag.as_deref(), Some("rust"));
        // 位置参形态（既有行为不破）。
        let cli = Cli::try_parse_from(["serena-cli", "warm", "rust"]).unwrap();
        let Some(Cmd::Warm { lang, lang_flag, .. }) = cli.cmd else {
            panic!("expected warm");
        };
        assert_eq!(lang.as_deref(), Some("rust"));
        assert!(lang_flag.is_none());
        // 前置全局形态（既有行为不破，第三来源）。
        let cli = Cli::try_parse_from(["serena-cli", "--lang", "python", "warm"]).unwrap();
        assert!(matches!(cli.cmd, Some(Cmd::Warm { .. })));
        assert_eq!(cli.lang.as_deref(), Some("python"));
    }

    // ==== help 首屏高频 5 命令速查 ====

    #[test]
    fn help_first_screen_carries_top5_quick_reference() {
        use clap::Parser as _;
        let err = Cli::try_parse_from(["serena-cli", "--help"]).expect_err("--help = DisplayHelp");
        let rendered = err.render().to_string();
        assert!(rendered.contains("高频 5 命令速查"), "首屏缺速查标题");
        for line in [
            "warm <LANG>",
            "find-symbol <QUERY>",
            "symbol-body <FILE> <SYMBOL>",
            "replace-body <FILE> <SYMBOL> --with <NEW>",
            "undo",
        ] {
            assert!(rendered.contains(line), "速查缺: {line}");
        }
    }
}

/// bd serena-rust-8cx5：`--with` 别名归一（合并/冲突/缺失三态）+ clap 端到端解析。
#[cfg(test)]
mod with_alias_tests {
    use super::*;

    #[test]
    fn resolve_with_alias_merges_alias_into_positional() {
        let mut cmd = Cmd::InsertTextAfterSymbol {
            file: "a.rs".into(),
            symbol: "f".into(),
            text: None,
            with: Some("body".into()),
        };
        resolve_with_alias(&mut cmd).unwrap();
        let Cmd::InsertTextAfterSymbol { text, .. } = &cmd else {
            panic!("variant changed")
        };
        assert_eq!(text.as_deref(), Some("body"));
    }

    #[test]
    fn resolve_with_alias_keeps_positional_shape() {
        let mut cmd = Cmd::InsertAtLine {
            file: "a.rs".into(),
            line: 1,
            text: Some("t".into()),
            with: None,
            expected_hash: None,
        };
        resolve_with_alias(&mut cmd).unwrap();
        let Cmd::InsertAtLine { text, .. } = &cmd else {
            panic!("variant changed")
        };
        assert_eq!(text.as_deref(), Some("t"), "旧形状（裸位置参数）不破");
    }

    #[test]
    fn resolve_with_alias_rejects_both_and_neither() {
        let mut both = Cmd::ReplaceLines {
            file: "a.rs".into(),
            start_line: 1,
            end_line: 2,
            text: Some("a".into()),
            with: Some("b".into()),
            expected_hash: None,
        };
        assert!(resolve_with_alias(&mut both).is_err(), "双给必拒");

        let mut neither = Cmd::CreateTextFile {
            file: "a.rs".into(),
            content: None,
            with: None,
            stdin: false,
            content_file: None,
        };
        assert!(resolve_with_alias(&mut neither).is_err(), "全缺必拒");
    }

    #[test]
    fn clap_parses_with_flag_on_insert_text_after_symbol() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "serena-cli",
            "--project",
            ".",
            "insert-text-after-symbol",
            "a.rs",
            "f",
            "--with",
            "x",
        ])
        .unwrap();
        let Some(Cmd::InsertTextAfterSymbol { text, with, .. }) = cli.cmd else {
            panic!("expected insert-text-after-symbol");
        };
        assert!(text.is_none() && with.as_deref() == Some("x"));
    }

    #[test]
    fn create_content_file_source_resolves() {
        // bd 4nqk：--content-file 读文件落 content（多行内容 bash 引号退路）。
        let p = std::env::temp_dir().join(format!(
            "serena-a3a-cf-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, "line1\nline2\n").unwrap();
        let mut cmd = Cmd::CreateTextFile {
            file: "a.rs".into(),
            content: None,
            with: None,
            stdin: false,
            content_file: Some(p.to_string_lossy().to_string()),
        };
        resolve_with_alias(&mut cmd).unwrap();
        let Cmd::CreateTextFile { content, .. } = &cmd else {
            panic!("variant changed")
        };
        assert_eq!(content.as_deref(), Some("line1\nline2\n"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn create_rejects_two_sources() {
        let mut both = Cmd::CreateTextFile {
            file: "a.rs".into(),
            content: Some("x".into()),
            with: None,
            stdin: true,
            content_file: None,
        };
        assert!(resolve_with_alias(&mut both).is_err(), "双来源必拒");
    }

    #[test]
    fn clap_parses_sweep_a3a_flags() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from(["serena-cli", "find-symbol", "x", "--format", "brief"])
            .unwrap();
        let Some(Cmd::FindSymbol { format, .. }) = cli.cmd else {
            panic!("expected find-symbol");
        };
        assert_eq!(format, OutFormat::Brief);

        let cli = Cli::try_parse_from(["serena-cli", "search", "p", "--distinct-symbols"]).unwrap();
        let Some(Cmd::Search {
            distinct_symbols,
            format,
            ..
        }) = cli.cmd
        else {
            panic!("expected search");
        };
        assert!(distinct_symbols);
        assert_eq!(format, OutFormat::Full, "默认 full 行为不变");

        let cli = Cli::try_parse_from([
            "serena-cli",
            "symbol-tree",
            ".",
            "--grep",
            "foo",
            "--max-depth",
            "2",
            "--files-only",
        ])
        .unwrap();
        let Some(Cmd::SymbolTree {
            grep,
            max_depth,
            files_only,
            ..
        }) = cli.cmd
        else {
            panic!("expected symbol-tree");
        };
        assert_eq!(grep.as_deref(), Some("foo"));
        assert_eq!(max_depth, Some(2));
        assert!(files_only);

        let cli = Cli::try_parse_from([
            "serena-cli",
            "find-referencing-symbols",
            "a.rs",
            "1",
            "1",
            "--debug-raw",
        ])
        .unwrap();
        let Some(Cmd::FindReferencingSymbols { debug_raw, .. }) = cli.cmd else {
            panic!("expected find-referencing-symbols");
        };
        assert!(debug_raw);
    }
}

/// bd serena-rust-kdye（symbol-body / edit-context `--symbol` 旗标）+ oxw0（ls-use
/// 二进制名智能匹配纯函数）锁。
#[cfg(test)]
mod blindfix_c_tests {
    use super::*;

    #[test]
    fn clap_parses_symbol_flag_and_positional_on_both_commands() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "serena-cli",
            "--project",
            ".",
            "symbol-body",
            "calc.py",
            "--symbol",
            "divide",
        ])
        .unwrap();
        let Some(Cmd::SymbolBody {
            file,
            symbol,
            symbol_flag,
        }) = cli.cmd
        else {
            panic!("expected symbol-body");
        };
        assert_eq!(file, "calc.py");
        assert!(symbol.is_none() && symbol_flag.as_deref() == Some("divide"));

        // 位置第二参兼容形状不变。
        let cli = Cli::try_parse_from([
            "serena-cli",
            "--project",
            ".",
            "edit-context",
            "calc.py",
            "divide",
        ])
        .unwrap();
        let Some(Cmd::EditContext {
            symbol,
            symbol_flag,
            ..
        }) = cli.cmd
        else {
            panic!("expected edit-context");
        };
        assert!(symbol.as_deref() == Some("divide") && symbol_flag.is_none());
    }

    /// 杠精 cqns：find-test --symbol 旗与位置参数等价（merge 进同一 wire 键）。
    #[test]
    fn resolve_with_alias_merges_find_test_symbol_flag() {
        let mut cmd = Cmd::FindTest {
            symbol: None,
            symbol_flag: Some("alpha".into()),
        };
        resolve_with_alias(&mut cmd).unwrap();
        let Cmd::FindTest { symbol, .. } = &cmd else {
            panic!("variant changed")
        };
        assert_eq!(symbol.as_deref(), Some("alpha"));

        // 位置参数优先形态照常通过。
        let mut cmd = Cmd::FindTest {
            symbol: Some("beta".into()),
            symbol_flag: None,
        };
        resolve_with_alias(&mut cmd).unwrap();

        // 双给必拒；全缺必拒。
        let mut both = Cmd::FindTest {
            symbol: Some("a".into()),
            symbol_flag: Some("b".into()),
        };
        assert!(resolve_with_alias(&mut both).is_err(), "双给必拒");
        let mut neither = Cmd::FindTest {
            symbol: None,
            symbol_flag: None,
        };
        assert!(resolve_with_alias(&mut neither).is_err(), "全缺必拒");
    }

    /// 杠精 cqns：read-file --start/--end 短别名归一进 --start-line/--end-line。
    #[test]
    fn resolve_with_alias_merges_read_file_line_aliases() {
        let mut cmd = Cmd::ReadFile {
            file: "lib.rs".into(),
            start_line: None,
            end_line: None,
            start: Some(3),
            end: Some(7),
            max_tokens: None,
        };
        resolve_with_alias(&mut cmd).unwrap();
        let Cmd::ReadFile {
            start_line,
            end_line,
            ..
        } = &cmd
        else {
            panic!("variant changed")
        };
        assert_eq!((*start_line, *end_line), (Some(3), Some(7)));

        // 长名与别名混给同名对 = 拒；不同名对 = 各自归一。
        let mut mixed = Cmd::ReadFile {
            file: "lib.rs".into(),
            start_line: Some(1),
            end_line: None,
            start: None,
            end: Some(9),
            max_tokens: None,
        };
        resolve_with_alias(&mut mixed).unwrap();
        let Cmd::ReadFile {
            start_line,
            end_line,
            ..
        } = &mixed
        else {
            panic!("variant changed")
        };
        assert_eq!((*start_line, *end_line), (Some(1), Some(9)));

        let mut both = Cmd::ReadFile {
            file: "lib.rs".into(),
            start_line: Some(1),
            end_line: None,
            start: Some(2),
            end: None,
            max_tokens: None,
        };
        assert!(resolve_with_alias(&mut both).is_err(), "双给必拒");
    }

    #[test]
    fn resolve_with_alias_merges_and_rejects_symbol_forms() {
        let mut flag_only = Cmd::SymbolBody {
            file: "calc.py".into(),
            symbol: None,
            symbol_flag: Some("divide".into()),
        };
        resolve_with_alias(&mut flag_only).unwrap();
        let Cmd::SymbolBody { symbol, .. } = &flag_only else {
            panic!("variant changed");
        };
        assert_eq!(symbol.as_deref(), Some("divide"));

        let mut both = Cmd::EditContext {
            file: "calc.py".into(),
            symbol: Some("a".into()),
            symbol_flag: Some("b".into()),
        };
        assert!(resolve_with_alias(&mut both).is_err(), "双给必拒");

        let mut neither = Cmd::EditContext {
            file: "calc.py".into(),
            symbol: None,
            symbol_flag: None,
        };
        assert!(resolve_with_alias(&mut neither).is_err(), "全缺必拒");
    }

    #[test]
    fn builtin_id_for_binary_matches_langserver_entrypoints() {
        // oxw0 主形态：pyright-langserver → pyright（不再落 jedi 条目）。
        assert_eq!(
            builtin_id_for_binary("pyright-langserver").map(|(id, _)| id),
            Ok("pyright")
        );
        assert_eq!(
            builtin_id_for_binary("jedi-language-server").map(|(id, _)| id),
            Ok("jedi")
        );
        assert!(
            builtin_id_for_binary("no-such-binary-xyz")
                .unwrap_err()
                .is_empty(),
            "零命中 = 空候选清单"
        );
    }

    #[test]
    fn builtin_binary_keys_cover_download_bin_paths() {
        let entries = ls_registry::config::builtin_entries();
        let (_, spec) = entries
            .iter()
            .find(|(id, _)| *id == "marksman")
            .expect("marksman in builtin table");
        let keys = builtin_binary_keys("marksman", spec);
        assert!(keys.contains(&"marksman".to_string()), "keys: {keys:?}");
    }

    #[test]
    fn def_ready_criterion_items_nonempty_without_warning() {
        // bd serena-rust-y3c1：def 同层就绪判据——items 非空且无 we0 warning。
        assert!(def_ready(&json!({"items": [{"uri": "file:///x.py"}]})));
        assert!(!def_ready(&json!({"items": []})), "空 items = 未就绪");
        assert!(
            !def_ready(&json!({"items": [{"uri": "x"}], "warning": "may not be ready"})),
            "we0 warning 在场即未就绪（空窗形态）"
        );
        assert!(!def_ready(&json!({})), "无 items 键 = 未就绪");
    }

    #[test]
    fn clap_parses_wait_ready_stage_def() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "serena-cli",
            "--project",
            ".",
            "wait-ready",
            "--stage",
            "def",
        ])
        .unwrap();
        assert!(matches!(
            cli.cmd,
            Some(Cmd::WaitReady {
                stage: WaitStage::Def,
                ..
            })
        ));
    }

    /// bd 0vj1：`--stage indexing` 可解析（ValueEnum 命名 = 小写变体名，rc=2 前科防御）。
    #[test]
    fn clap_parses_wait_ready_stage_indexing() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "serena-cli",
            "--project",
            ".",
            "wait-ready",
            "--stage",
            "indexing",
        ])
        .unwrap();
        assert!(matches!(
            cli.cmd,
            Some(Cmd::WaitReady {
                stage: WaitStage::Indexing,
                ..
            })
        ));
    }
}

#[cfg(test)]
mod shorthand_tests {
    use super::rewrite_shorthand_argv;

    #[test]
    fn query_form_maps_to_find_symbol() {
        let out = rewrite_shorthand_argv(vec!["?clamp".into()]).unwrap();
        assert_eq!(out, vec!["find-symbol".to_string(), "clamp".to_string()]);
    }

    #[test]
    fn cmd_suffix_form_strips_question() {
        let out = rewrite_shorthand_argv(vec!["list-dir?".into(), "crates".into()]).unwrap();
        assert_eq!(out, vec!["list-dir".to_string(), "crates".to_string()]);
    }

    #[test]
    fn global_value_flag_value_is_skipped() {
        let out = rewrite_shorthand_argv(vec![
            "--project".into(),
            ".".into(),
            "?x".into(),
        ])
        .unwrap();
        assert_eq!(
            out,
            vec![
                "--project".to_string(),
                ".".to_string(),
                "find-symbol".to_string(),
                "x".to_string()
            ]
        );
    }

    #[test]
    fn bool_flag_then_shorthand_rewrites() {
        let out = rewrite_shorthand_argv(vec!["--json".into(), "?q".into()]).unwrap();
        assert_eq!(
            out,
            vec!["--json".to_string(), "find-symbol".to_string(), "q".to_string()]
        );
    }

    #[test]
    fn plain_subcommand_and_bare_question_untouched() {
        assert!(rewrite_shorthand_argv(vec!["status".into()]).is_none());
        assert!(rewrite_shorthand_argv(vec!["find-symbol".into(), "x".into()]).is_none());
        // 裸 `?`（len=1）不做糖，原样交 clap 报错。
        assert!(rewrite_shorthand_argv(vec!["?".into()]).is_none());
    }

    #[test]
    fn terminator_disables_sugar() {
        assert!(rewrite_shorthand_argv(vec!["--".into(), "?x".into()]).is_none());
    }
}

/// 按 file 后缀推断 LSP `textDocument/completion` 的 triggerCharacter。
/// 仅当 agent 显式不传 trigger 时启用（C++ / Rust / TS / JS / Py 共 5 系）。
/// ponytail: 这是文件后缀到 trigger 字符的固定映射表，新加 lang 时补一行即可，
///          不必上配置。
fn infer_trigger_char(file: &str) -> Option<String> {
    let ext = file.rsplit('.').next()?.to_ascii_lowercase();
    let ch: &'static str = match ext.as_str() {
        "cpp" | "c" | "cc" | "cxx" | "h" | "hpp" | "hxx" => ".",
        "rs" => "::",
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "py" => ".",
        _ => return None,
    };
    Some(ch.to_owned())
}
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!(r"\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

// ============== 行号契约（bd serena-rust-7xv）==============
//
// CLI 的 position 型参数契约统一为 1-based（AI 用户与编辑器行号习惯）；LSP
// Position 为 0-based。转换只发生在 CLI 层：`cli_main` 解析后经
// `normalize_positions` 一次性就地 -1，`--direct` 直调与 HTTP 转发两条路径共用
// 转换结果，supervisor / lsp-core 不感知。行级编辑工具（read-file /
// insert-at-line / replace-lines / delete-lines / delete-text-in-symbol）的行
// 参数本来就是 1-based 且不映射 LSP Position，不在转换之列。shell JSONL 的
// args 透传模式不在本契约内（另行约定）。

/// bd serena-rust-8cx5：单文本写工具 `--with` 别名归一。位置参数与 `--with`
/// 二选一，解析后统一落回原字段——wire args 键名不变，旧形状（裸位置参数）
/// 不破；replace-body 本用 `--with`，其余写工具自此同形。位置参数在 clap 里
/// 已成 Option，未解析就到 tool_request = 编程错误（cli_main 必先跑本函数）。
fn resolve_with_alias(cmd: &mut Cmd) -> Result<(), String> {
    fn merge(
        pos: &mut Option<String>,
        alias: &mut Option<String>,
        label: &str,
        flag: &str,
    ) -> Result<(), String> {
        match (pos.take(), alias.take()) {
            (Some(t), None) | (None, Some(t)) => {
                *pos = Some(t);
                Ok(())
            }
            (Some(_), Some(_)) => {
                Err(format!("provide the {label} positionally or via --{flag}, not both"))
            }
            (None, None) => Err(format!("missing {label}: pass it positionally or via --{flag}")),
        }
    }
    // 杠精 cqns：read-file 行别名（u32 形态的 merge；别名 = 同语义旗标对，二选一）。
    fn merge_line(
        long: &mut Option<u32>,
        alias: &mut Option<u32>,
        long_flag: &str,
        alias_flag: &str,
    ) -> Result<(), String> {
        match (long.take(), alias.take()) {
            (Some(t), None) | (None, Some(t)) => {
                *long = Some(t);
                Ok(())
            }
            (Some(_), Some(_)) => Err(format!("provide only one of --{long_flag} / --{alias_flag}")),
            (None, None) => Ok(()),
        }
    }
    match cmd {
        Cmd::InsertTextBeforeSymbol { text, with, .. }
        | Cmd::InsertAtLine { text, with, .. }
        | Cmd::ReplaceLines { text, with, .. } => merge(text, with, "text", "with"),
        // bd serena-rust-kdye：symbol-body / edit-context 符号名旗标归一——与位置
        // 第二参等价二选一，wire args 键名不变。
        Cmd::SymbolBody {
            symbol,
            symbol_flag,
            ..
        }
        | Cmd::EditContext {
            symbol,
            symbol_flag,
            ..
        } => merge(symbol, symbol_flag, "symbol", "symbol"),
        // 杠精 cqns：find-test 补 --symbol 旗（与 symbol-body 形状对齐）。
        Cmd::FindTest {
            symbol,
            symbol_flag,
            ..
        } => merge(symbol, symbol_flag, "symbol", "symbol"),
        // 杠精 cqns：read-file --start/--end 短别名归一进 --start-line/--end-line
        // （wire 键名不变，别名只存在于 CLI 面）。
        Cmd::ReadFile {
            start_line,
            start,
            end_line,
            end,
            ..
        } => {
            merge_line(start_line, start, "start-line", "start")?;
            merge_line(end_line, end, "end-line", "end")
        }
        // bd 4nqk：内容四来源（位置 / --with / --stdin / --content-file）恰好一个。
        Cmd::CreateTextFile {
            content,
            with,
            stdin,
            content_file,
            ..
        } => {
            let from_file = match content_file.take() {
                Some(p) => Some(
                    std::fs::read_to_string(&p)
                        .map_err(|e| format!("--content-file {p}: {e}"))?,
                ),
                None => None,
            };
            let from_stdin = if *stdin {
                Some(
                    std::io::read_to_string(std::io::stdin())
                        .map_err(|e| format!("read stdin: {e}"))?,
                )
            } else {
                None
            };
            let sources: [Option<String>; 4] =
                [content.take(), with.take(), from_stdin, from_file];
            let picked: Vec<String> = sources.into_iter().flatten().collect();
            match picked.len() {
                1 => {
                    *content = picked.into_iter().next();
                    Ok(())
                }
                0 => Err(
                    "missing content: pass it positionally, --with, --stdin or --content-file"
                        .into(),
                ),
                _ => Err(
                    "provide the content via only one of positional / --with / --stdin / --content-file"
                        .into(),
                ),
            }
        }
        Cmd::InsertTextAfterSymbol { text, with, .. } => merge(text, with, "text", "with"),
        _ => Ok(()),
    }
}

/// 批3 可发现性：warm 双形态归一——位置参 LANG 与子命令级 `--lang` 二选一，
/// 统一落回位置参字段（wire args 键名不变，8cx5 同款契约）。global 是前置全局
/// `--lang`（clap global flag 不传播进子命令，仅 `--lang X warm` 形态可达），
/// 第三来源同判二选一。
fn warm_lang_forms(
    pos: &mut Option<String>,
    flag: &mut Option<String>,
    global: Option<&str>,
) -> Result<(), String> {
    let given = usize::from(pos.is_some()) + usize::from(flag.is_some()) + usize::from(global.is_some());
    if given > 1 {
        return Err(
            "provide the warm LANG positionally or via --lang (once), not multiple forms".into(),
        );
    }
    if pos.is_none() {
        *pos = flag.take().or(global.map(str::to_string));
    }
    Ok(())
}

/// 1-based (line, col) → LSP 0-based Position；0 为用法错误。
fn to_lsp_pos(line: u32, col: u32) -> Result<(u32, u32), String> {
    if line == 0 {
        return Err(format!("line is 1-based (got line={line})"));
    }
    if col == 0 {
        return Err(format!("col is 1-based (got col={col})"));
    }
    Ok((line - 1, col - 1))
}

/// 1-based line（无 col 的行参数，如 inlay-hint 的 Range 行）→ 0-based。
fn to_lsp_line(line: u32) -> Result<u32, String> {
    if line == 0 {
        return Err(format!("line is 1-based (got line={line})"));
    }
    Ok(line - 1)
}

/// 杠精 07u5-1：客户端校验失败与 daemon 同形——打 wire JSON error 对象
/// （{"code","message","retryable"}），rc=2。纯文本形态对 JSON 解析方不可消费。
/// bd fdmj-F3：JSON 统一走 **stdout**（成功载荷同流，agent 单流解析）；
/// clap 渲染的 usage/人读文本留 stderr（clap_exit_to_json 里 err.print()）。
fn bad_args_exit(detail: &str) -> ExitCode {
    println!(
        "{}",
        serde_json::json!({"code": "BAD_ARGS", "message": detail, "retryable": false})
    );
    ExitCode::from(2)
}

/// bd serena-rust-p2zp：clap 原生 parse 错（missing subcommand / invalid value /
/// unknown arg 等）从裸文本 + `e.exit()`（std::process::exit，Drop 来不及 flush）
/// 改为：人读文本（clap 渲染保留 usage 提示）→ wire JSON error 对象 → rc=2。
/// help / version（`use_stderr()=false`）保持原生路径——这些是用户主动请求的
/// 输出，不是错误。Clap `Error::to_string()` 已含「error: ...」前缀 + 「Usage:」
/// 段；JSON message 字段塞全文让 agent 既能识别 code 又能定位原 bad token。
fn clap_exit_to_json(err: clap::Error) -> ExitCode {
    if err.use_stderr() {
        // 真错误：先打 clap 渲染文本（含 usage + 红字/粗体风格，纯终端好看），
        // 再打 wire JSON error 对象（agent 解析路径），最后 rc=2。
        // `e.print()` 走 clap 内部 stream 选择（stderr），不会与后续 eprintln 互踩。
        let _ = err.print();
        return bad_args_exit(&err.to_string());
    }
    // help / version：原生渲染（stdout）+ 0。
    let _ = err.print();
    ExitCode::SUCCESS
}

/// 解析后统一转换：把 position 型子命令的 line/col 就地 -1 成 LSP 0-based。
/// 新增 position 型子命令时必须在此登记——漏登记 = 该命令 raw 透传（即 bd 7xv
/// 的原始 bug 形态）；行级工具误登记 = 双重 -1（单测锁定）。
fn normalize_positions(cmd: &mut Cmd) -> Result<(), String> {
    match cmd {
        // O3：--symbol 直查时位置可省；给了位置才做 1-based → 0-based 归一，
        // 缺位置交由 supervisor required_position 报 BAD_ARGS rc=2。
        Cmd::FindReferencingCodeSnippets {
            line: Some(l),
            col: Some(c),
            symbol,
            ..
        } if symbol.is_none() => {
            let (nl, nc) = to_lsp_pos(*l, *c)?;
            *l = nl;
            *c = nc;
            Ok(())
        }
        Cmd::Def { line, col, .. }
        | Cmd::Refs { line, col, .. }
        | Cmd::Hover { line, col, .. }
        | Cmd::FindImplementations { line, col, .. }
        | Cmd::RenameSymbol { line, col, .. }
        | Cmd::FindReferencingSymbols { line, col, .. }
        | Cmd::Completion { line, col, .. }
        | Cmd::ContainingSymbol { line, col, .. }
        | Cmd::DefiningSymbol { line, col, .. }
        | Cmd::SignatureHelp { line, col, .. }
        | Cmd::CodeAction { line, col, .. }
        | Cmd::DocumentHighlight { line, col, .. }
        | Cmd::Moniker { line, col, .. } => {
            let (l, c) = to_lsp_pos(*line, *col)?;
            *line = l;
            *col = c;
            Ok(())
        }
        Cmd::FormatRange {
            start_line,
            start_col,
            end_line,
            end_col,
            ..
        } => {
            let (sl, sc) = to_lsp_pos(*start_line, *start_col)?;
            let (el, ec) = to_lsp_pos(*end_line, *end_col)?;
            *start_line = sl;
            *start_col = sc;
            *end_line = el;
            *end_col = ec;
            Ok(())
        }
        Cmd::InlayHint {
            start_line,
            end_line,
            ..
        } => {
            *start_line = to_lsp_line(*start_line)?;
            *end_line = to_lsp_line(*end_line)?;
            Ok(())
        }
        // 仅 prepare 消费 file/line/col；incoming/outgoing 的 line/col 不使用、不动。
        Cmd::CallHierarchy { op, line, col, .. } | Cmd::TypeHierarchy { op, line, col, .. }
            if op == "prepare" =>
        {
            if let (Some(l), Some(c)) = (line, col) {
                let (l2, c2) = to_lsp_pos(*l, *c)?;
                *l = l2;
                *c = c2;
            }
            Ok(())
        }
        // 行级工具（已 1-based）/ 管理命令 / shell / daemon：无 position 参数。
        _ => Ok(()),
    }
}

/// J（§11-J）：4 个集合型位置工具是否带 `--delta`（其余子命令无该 flag）。
fn cmd_requests_delta(cmd: &Option<Cmd>) -> bool {
    matches!(
        cmd,
        Some(Cmd::Overview { delta: true, .. })
            | Some(Cmd::Refs { delta: true, .. })
            | Some(Cmd::FindSymbol { delta: true, .. })
            | Some(Cmd::FindImplementations { delta: true, .. })
    )
}

/// H（§10-H）：`--json` → `args._compact=false`（supervisor 位置工具
/// envelope 消费；sanitize 不清，与 `_delta` 同套私有约定）。非 object
/// 形态静默跳过（同 `inject_timeout_args`）。`json_flag=false` 时不动 args
/// —— 默认紧凑 wire 零变化。
fn inject_compact_arg(args: &mut serde_json::Value, json_flag: bool) {
    if !json_flag {
        return;
    }
    if let Some(obj) = args.as_object_mut() {
        obj.insert("_compact".into(), serde_json::json!(false));
    }
}

/// Phase 4 基建 Task 22b：把 CLI flag `--request-timeout` / `--index-timeout` /
/// `--warmup-timeout`（批2-A）注入 `args._timeout_ms` / `args._index_timeout_ms` /
/// `args._warmup_ms` 私有字段（supervisor `execute_tool` 入口 `sanitize_timeout_args`
/// 会清掉）。`args` 必须是 object 形态；其他形态（少见，子命令可能返 null/array）
/// 静默跳过。
fn inject_timeout_args(
    args: &mut serde_json::Value,
    req_ms: Option<u32>,
    idx_ms: Option<u32>,
    warm_ms: Option<u32>,
) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    if let Some(ms) = req_ms {
        obj.insert("_timeout_ms".into(), serde_json::json!(ms));
    }
    if let Some(ms) = idx_ms {
        obj.insert("_index_timeout_ms".into(), serde_json::json!(ms));
    }
    if let Some(ms) = warm_ms {
        obj.insert("_warmup_ms".into(), serde_json::json!(ms));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// serena-rust-nodd：跨 run tempdir 残留清扫——每次套件启动删 >2h 的
    /// serena 前缀残留（age 阈值避开并行 run 在用目录）。
    #[test]
    fn sweep_stale_serena_tempdirs() {
        let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 3600);
        let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) else {
            return;
        };
        for ent in rd.flatten() {
            let name = ent.file_name().to_string_lossy().to_string();
            // serena-powershell 是 powershell LS 的运行时日志目录（非测试产物），不碰。
            if name == "serena-powershell" || !name.starts_with("serena") {
                continue;
            }
            let stale = ent
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| t < cutoff)
                .unwrap_or(false);
            if stale {
                let _ = if ent.path().is_dir() {
                    std::fs::remove_dir_all(ent.path())
                } else {
                    std::fs::remove_file(ent.path())
                };
            }
        }
    }

    /// --json：args 注入 `_compact=false`（supervisor envelope 走原始 LSP 形态）。
    #[test]
    fn json_flag_injects_compact_false() {
        let mut args = json!({"file": "a.rs", "line": 1, "col": 2});
        inject_compact_arg(&mut args, true);
        assert_eq!(args["_compact"], serde_json::Value::Bool(false));
        assert_eq!(args["file"], "a.rs"); // 原有 args 不丢
    }

    /// 无 --json：args 一字不动 → 默认紧凑 wire 零变化。
    #[test]
    fn no_json_flag_leaves_args_untouched() {
        let mut args = json!({"file": "a.rs"});
        inject_compact_arg(&mut args, false);
        assert!(args.get("_compact").is_none());
        assert_eq!(args, json!({"file": "a.rs"}));
    }

    // ---- 行号契约（bd serena-rust-7xv）----

    #[test]
    fn to_lsp_pos_converts_1based_to_0based() {
        assert_eq!(to_lsp_pos(1, 1), Ok((0, 0)));
        assert_eq!(to_lsp_pos(2, 8), Ok((1, 7)));
        assert_eq!(to_lsp_pos(u32::MAX, 1), Ok((u32::MAX - 1, 0)));
    }

    #[test]
    fn to_lsp_pos_rejects_zero() {
        assert!(to_lsp_pos(0, 7).is_err());
        assert!(to_lsp_pos(1, 0).is_err());
        assert!(to_lsp_pos(0, 0).is_err());
    }

    #[test]
    fn to_lsp_line_converts_and_rejects_zero() {
        assert_eq!(to_lsp_line(3), Ok(2));
        assert!(to_lsp_line(0).is_err());
    }

    #[test]
    fn normalize_converts_position_subcommands() {
        let mut cmd = Cmd::RenameSymbol {
            file: "lib.rs".into(),
            line: 1,
            col: 8,
            new_name: "X".into(),
        };
        normalize_positions(&mut cmd).unwrap();
        let Cmd::RenameSymbol { line, col, .. } = &cmd else {
            panic!("variant changed")
        };
        assert_eq!((*line, *col), (0, 7));

        let mut cmd = Cmd::Hover {
            file: "lib.rs".into(),
            line: 2,
            col: 8,
        };
        normalize_positions(&mut cmd).unwrap();
        let Cmd::Hover { line, col, .. } = &cmd else {
            panic!("variant changed")
        };
        assert_eq!((*line, *col), (1, 7));
    }

    #[test]
    fn normalize_rejects_zero_line_and_col() {
        let mut cmd = Cmd::RenameSymbol {
            file: "lib.rs".into(),
            line: 0,
            col: 7,
            new_name: "X".into(),
        };
        assert!(normalize_positions(&mut cmd).is_err());

        let mut cmd = Cmd::Def {
            file: "lib.rs".into(),
            line: 1,
            col: 0,
        };
        assert!(normalize_positions(&mut cmd).is_err());
    }

    #[test]
    fn normalize_skips_line_based_tools() {
        // 行级工具的行参数本来就是 1-based，normalize 不得触碰（防双重 -1）。
        let mut cmd = Cmd::InsertAtLine {
            file: "lib.rs".into(),
            line: 1,
            text: Some("// foo".into()),
            with: None,
            expected_hash: None,
        };
        normalize_positions(&mut cmd).unwrap();
        let Cmd::InsertAtLine { line, .. } = &cmd else {
            panic!("variant changed")
        };
        assert_eq!(*line, 1);

        let mut cmd = Cmd::ReadFile {
            file: "lib.rs".into(),
            start_line: Some(1),
            end_line: Some(2),
            start: None,
            end: None,
            max_tokens: None,
        };
        normalize_positions(&mut cmd).unwrap();
        let Cmd::ReadFile {
            start_line,
            end_line,
            ..
        } = &cmd
        else {
            panic!("variant changed")
        };
        assert_eq!((*start_line, *end_line), (Some(1), Some(2)));
    }

    #[test]
    fn normalize_converts_range_subcommands() {
        let mut cmd = Cmd::FormatRange {
            file: "f.rs".into(),
            start_line: 1,
            start_col: 1,
            end_line: 2,
            end_col: 5,
            tab_size: None,
            insert_spaces: None,
        };
        normalize_positions(&mut cmd).unwrap();
        let Cmd::FormatRange {
            start_line,
            start_col,
            end_line,
            end_col,
            ..
        } = &cmd
        else {
            panic!("variant changed")
        };
        assert_eq!((*start_line, *start_col, *end_line, *end_col), (0, 0, 1, 4));

        let mut cmd = Cmd::InlayHint {
            file: "f.rs".into(),
            start_line: 1,
            end_line: 3,
        };
        normalize_positions(&mut cmd).unwrap();
        let Cmd::InlayHint {
            start_line,
            end_line,
            ..
        } = &cmd
        else {
            panic!("variant changed")
        };
        assert_eq!((*start_line, *end_line), (0, 2));
    }

    #[test]
    fn normalize_converts_hierarchy_prepare_only() {
        let mut cmd = Cmd::CallHierarchy {
            op: "prepare".into(),
            file: Some("f.rs".into()),
            line: Some(2),
            col: Some(8),
            item: None,
        };
        normalize_positions(&mut cmd).unwrap();
        let Cmd::CallHierarchy { line, col, .. } = &cmd else {
            panic!("variant changed")
        };
        assert_eq!((*line, *col), (Some(1), Some(7)));

        // incoming/outgoing 不消费 line/col：不转换也不报错。
        let mut cmd = Cmd::TypeHierarchy {
            op: "subtypes".into(),
            file: None,
            line: Some(0),
            col: Some(0),
            item: Some("{}".into()),
        };
        normalize_positions(&mut cmd).unwrap();
        let Cmd::TypeHierarchy { line, col, .. } = &cmd else {
            panic!("variant changed")
        };
        assert_eq!((*line, *col), (Some(0), Some(0)));
    }
}

#[cfg(test)]
mod net_retry_tests {
    use super::*;

    // 假错误类型：reqwest::Error 无公开构造器，transient 判定以 fn 注入即可测重试编排。
    #[derive(Debug, PartialEq)]
    enum FakeErr {
        Transient,
        Fatal,
    }

    fn transient(e: &FakeErr) -> bool {
        matches!(e, FakeErr::Transient)
    }

    #[tokio::test]
    async fn retries_transient_until_success() {
        let calls = std::cell::Cell::new(0usize);
        let r = send_with_connect_retry(
            || {
                let n = calls.get() + 1;
                calls.set(n);
                async move {
                    if n < 3 {
                        Err(FakeErr::Transient)
                    } else {
                        Ok::<_, FakeErr>(n)
                    }
                }
            },
            transient,
        )
        .await
        .unwrap();
        assert_eq!(r, 3);
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn non_transient_fails_without_retry() {
        let calls = std::cell::Cell::new(0usize);
        let r: Result<(), FakeErr> = send_with_connect_retry(
            || {
                calls.set(calls.get() + 1);
                async { Err(FakeErr::Fatal) }
            },
            transient,
        )
        .await;
        assert_eq!(r, Err(FakeErr::Fatal));
        assert_eq!(calls.get(), 1, "语义错误绝不重试");
    }

    #[tokio::test]
    async fn exhausted_transient_returns_last_err() {
        let calls = std::cell::Cell::new(0usize);
        let r: Result<(), FakeErr> = send_with_connect_retry(
            || {
                calls.set(calls.get() + 1);
                async { Err(FakeErr::Transient) }
            },
            transient,
        )
        .await;
        assert_eq!(r, Err(FakeErr::Transient));
        assert_eq!(
            calls.get(),
            1 + CONNECT_BACKOFF_MS.len(),
            "首发起发 + 每档退避各一发"
        );
    }

    /// 真连接失败走 reqwest::Error 判定：bind 后立即 drop listener → connect refused
    /// 应被判为可重试并退避耗尽（总耗时 ≥ 各档退避之和）。
    #[tokio::test]
    async fn real_connect_refused_is_transient_and_backs_off() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let client = http_client();
        let t0 = Instant::now();
        let r: Result<reqwest::Response, _> = send_with_connect_retry(
            || {
                let url = format!("http://127.0.0.1:{port}/");
                let c = client.clone();
                async move { c.get(&url).send().await }
            },
            |e: &reqwest::Error| e.is_connect() || e.is_request(),
        )
        .await;
        assert!(r.is_err());
        assert!(
            t0.elapsed() >= Duration::from_millis(700),
            "4 档退避之和 750ms，实测 {:?}",
            t0.elapsed()
        );
    }

    /// lazy-spawn 进程配置：DETACHED_PROCESS + CREATE_NEW_PROCESS_GROUP 必须都在
    /// （父退不带走 daemon；ctrl+C 不打穿 daemon）。行为层由 vjm e2e 锁。
    #[cfg(windows)]
    #[test]
    fn daemon_creation_flags_detached_and_new_group() {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        let f = super::daemon_creation_flags();
        assert_ne!(f & DETACHED_PROCESS, 0);
        assert_ne!(f & CREATE_NEW_PROCESS_GROUP, 0);
    }

    // ---- bd serena-rust-g0m：DAEMON_DRAINING 自愈 ----

    #[test]
    fn is_daemon_draining_matches_only_503_with_wire_code() {
        let draining = json!({"ok": false, "error": {"code": "DAEMON_DRAINING", "message": "x"}});
        let other_code = json!({"ok": false, "error": {"code": "INTERNAL", "message": "x"}});
        assert!(is_daemon_draining(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            &draining
        ));
        assert!(
            !is_daemon_draining(reqwest::StatusCode::SERVICE_UNAVAILABLE, &other_code),
            "非 DRAINING 503 不触发自愈"
        );
        assert!(
            !is_daemon_draining(reqwest::StatusCode::INTERNAL_SERVER_ERROR, &draining),
            "非 503 不触发"
        );
        assert!(!is_daemon_draining(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            &json!("boom")
        ));
    }

    #[tokio::test]
    async fn draining_is_retried_until_success() {
        let calls = std::cell::Cell::new(0usize);
        let payload = json!({"error": {"code": "DAEMON_DRAINING"}});
        let r = retry_on_draining(
            || {
                let n = calls.get() + 1;
                calls.set(n);
                let payload = payload.clone();
                async move {
                    if n < 3 {
                        Err(ForwardFailure::Draining {
                            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                            payload,
                        })
                    } else {
                        Ok::<_, ForwardFailure>(())
                    }
                }
            },
            Duration::from_secs(2),
            Duration::from_millis(10),
        )
        .await;
        assert!(r.is_ok());
        assert_eq!(calls.get(), 3, "窗口内退避重试到成功");
    }

    #[tokio::test]
    async fn draining_beyond_window_returns_original_error() {
        let calls = std::cell::Cell::new(0usize);
        let r: Result<(), String> = retry_on_draining(
            || {
                calls.set(calls.get() + 1);
                async {
                    Err(ForwardFailure::Draining {
                        status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
                        payload: json!({"error": {"code": "DAEMON_DRAINING"}}),
                    })
                }
            },
            Duration::from_millis(150),
            Duration::from_millis(50),
        )
        .await;
        let msg = r.unwrap_err();
        assert!(
            msg.contains("daemon transport error"),
            "还原既有错误文本: {msg}"
        );
        assert!(msg.contains("DAEMON_DRAINING"), "错误体保留 wire 码: {msg}");
        assert!(calls.get() >= 2, "窗口内至少重试过一轮: {}", calls.get());
    }

    #[tokio::test]
    async fn fatal_forward_failure_is_not_retried() {
        let calls = std::cell::Cell::new(0usize);
        let r: Result<(), String> = retry_on_draining(
            || {
                calls.set(calls.get() + 1);
                async { Err(ForwardFailure::Fatal("boom".into())) }
            },
            Duration::from_secs(5),
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(r.unwrap_err(), "boom");
        assert_eq!(calls.get(), 1, "非 draining 失败绝不重试");
    }

    // ---- bd serena-rust-55m：wait-ready ----

    #[test]
    fn hover_ready_rejects_we0_warning_and_empty_forms() {
        assert!(!hover_ready(&json!(
            {"items": [], "warning": "semantic layer returned empty; ... may not be ready yet"}
        )));
        assert!(!hover_ready(&serde_json::Value::Null));
        assert!(!hover_ready(&json!({"contents": ""})));
        assert!(!hover_ready(&json!({"contents": []})));
        assert!(!hover_ready(&json!({"contents": {"value": ""}})));
        assert!(!hover_ready(&json!({})));
    }

    #[test]
    fn hover_ready_accepts_nonempty_contents_forms() {
        assert!(hover_ready(&json!({"contents": {"value": "fn hello"}})));
        assert!(hover_ready(&json!({"contents": [{"value": "x"}]})));
        assert!(hover_ready(&json!({"contents": "plain text"})));
    }

    // ---- bd serena-rust-7m8：semantic 探针标识符偏移 ----

    #[test]
    fn probe_positions_prefers_selection_range() {
        // wire 带 selectionRange → 直接用其 start，不查文件文本。
        let wire = json!([
            {"name": "Foo", "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 16}},
             "selectionRange": {"start": {"line": 2, "character": 13}, "end": {"line": 2, "character": 16}}}
        ]);
        assert_eq!(
            hover_probe_positions(&wire, Some("public class Foo")),
            vec![(2, 13)]
        );
    }

    #[test]
    fn probe_positions_locate_symbol_name_in_range_line() {
        // 无 selectionRange → range.start 行内按 name 文本定位（UTF-16 列）。
        let wire = json!([
            {"name": "Greeter", "range": {"start": {"line": 2, "character": 0}, "end": {"line": 4, "character": 1}}}
        ]);
        let text = "namespace App;\n\npublic class Greeter\n{\n}\n";
        assert_eq!(
            hover_probe_positions(&wire, Some(text)),
            vec![(2, 13)],
            "hover 落在 Greeter 的 G（'public class ' = 13 列），非行首"
        );
        // UTF-16 偏移：非 ASCII 前缀按 code unit 计列，不是字节/char 混算。
        let wire_cjk = json!([
            {"name": "值", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 8}}}
        ]);
        assert_eq!(
            hover_probe_positions(&wire_cjk, Some("let 值 = 1;")),
            vec![(0, 4)]
        );
    }

    #[test]
    fn probe_positions_drops_symbols_without_locatable_name() {
        // bd serena-rust-b8sp：name 不在 range.start 行（模板/复合符号形态）或行越界
        // → 丢弃候选，绝不退 range.start——行首 stdlib/修饰 token 的 hover 恒空，
        // 会把已就绪永判 pending。全 null/空 hover 仍判 pending（等待语义不变）。
        let wire = json!([
            {"name": "p", "range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 9}}},
            {"name": "q", "range": {"start": {"line": 9, "character": 2}, "end": {"line": 9, "character": 12}}}
        ]);
        assert_eq!(
            hover_probe_positions(&wire, Some("<div>\n</div>")),
            Vec::<(u32, u32)>::new(),
            "两个候选都不可定位 → 空候选集（调用方给根因行），不打行首"
        );
        // name 可定位的用户标识符保留（同轮混排只留可定位者）。
        let wire_mixed = json!([
            {"name": "p", "range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 9}}},
            {"name": "main", "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 10}}}
        ]);
        assert_eq!(
            hover_probe_positions(&wire_mixed, Some("<div>\n</div>\nfn main() {}\n")),
            vec![(2, 3)]
        );
        // 全 null/空 hover → hover_ready 全 false → 循环判 pending（非假阳性）。
        assert!(!hover_ready(&serde_json::Value::Null));
        assert!(!hover_ready(&json!({"contents": null})));
    }

    // ---- bd serena-rust-nqjo rework：打点整词精化 + ≤3 行窗口 + kind 偏置 ----

    #[test]
    fn probe_positions_whole_word_skips_substring_hit() {
        // 子串命中 `running` 不是 `run` —— 整词精化后落在真正的 run 名字上。
        let wire = json!([
            {"name": "run", "kind": "Function", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 3, "character": 1}}}
        ]);
        let text = "def running(x):\n    return run(x)\n";
        // run 在 line 0 无整词命中（running 吃掉），窗口 +1 行命中 `run(x)` 列 11。
        assert_eq!(hover_probe_positions(&wire, Some(text)), vec![(1, 11)]);
    }

    #[test]
    fn probe_positions_cross_line_signature_hits_name_token() {
        // range.start 行是装饰器（pyright 对装饰器函数 range 从 @ 行起），
        // 名字在窗口 +1 行 —— 单行版 fallback 会丢候选导致 hover 恒 pending。
        let wire = json!([
            {"name": "cached_compute", "kind": "Function", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 2, "character": 1}}}
        ]);
        let text = "@cache\ndef cached_compute(x):\n    return x\n";
        assert_eq!(hover_probe_positions(&wire, Some(text)), vec![(1, 4)]);
    }

    #[test]
    fn probe_positions_prefers_semantic_kinds_over_variables() {
        // 冷窗口局部变量 hover 常空：Class/Function/Method 优先于 Other(变量)。
        let wire = json!([
            {"name": "alpha", "kind": {"Other": 13}, "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 9}}},
            {"name": "beta", "kind": {"Other": 13}, "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 8}}},
            {"name": "gamma", "kind": {"Other": 13}, "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 9}}},
            {"name": "Repo", "kind": "Class", "range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 12}}}
        ]);
        let text = "alpha = 1\nbeta = 2\ngamma = 3\nclass Repo:\n";
        let got = hover_probe_positions(&wire, Some(text));
        assert_eq!(got.len(), 3);
        assert_eq!(got[0], (3, 6), "Class 排到候选首位");
    }

    #[test]
    fn probe_positions_scans_all_symbols_when_no_semantic_kind() {
        // 全无语义 kind（import 别名/属性形态）→ 不截断，4 个全扫，3 个可定位。
        let wire = json!([
            {"name": "a", "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 4}}},
            {"name": "b", "range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 4}}},
            {"name": "zz", "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 5}}},
            {"name": "d", "range": {"start": {"line": 3, "character": 0}, "end": {"line": 3, "character": 4}}}
        ]);
        let text = "a = 1\nb = 2\nzz = 3\nd = 4\n";
        assert_eq!(hover_probe_positions(&wire, Some(text)).len(), 4);
    }

    #[test]
    fn probe_positions_pyright_fixture_shape_lands_on_name() {
        // PM 实景复刻：supervisor overview 组装丢弃 selectionRange（字段缺失），
        // range.start 落在 def/class 关键字位 → 必须打在名字 token 上。
        let wire = json!([
            {"name": "Calculator", "kind": "Class", "range": {"start": {"character": 0, "line": 3}, "end": {"character": 18, "line": 3}}},
            {"name": "__init__", "kind": "Method", "range": {"start": {"character": 4, "line": 4}, "end": {"character": 38, "line": 4}}},
            {"name": "base", "kind": {"Other": 13}, "range": {"start": {"character": 23, "line": 4}, "end": {"character": 36, "line": 4}}}
        ]);
        let text = "\"\"\"doc\"\"\"\n\n\nclass Calculator:\n    def __init__(self, base: int = 0):\n        self.base = base\n";
        assert_eq!(
            hover_probe_positions(&wire, Some(text)),
            vec![(3, 6), (4, 8), (4, 23)],
            "class 名 (3,6) / 方法名 (4,8) / 参数名 (4,23)，全在名字 token 内"
        );
    }

    #[test]
    fn probe_positions_cjk_fixture_without_selectionrange_lands_on_names() {
        // bd fq7f 实景：fb4k_fx/zh.py 全 CJK fixture——v0.2.0 旧探针 hover
        // range.start（def/class 关键字位），pyright 对关键字位 hover 恒 null
        // → 假超时 rc=4。候选必须落名字 token（本测试钉死坐标防回档）。
        let wire = json!([
            {"name": "calc", "kind": "Function", "range": {"start": {"character": 0, "line": 1}, "end": {"character": 16, "line": 4}}},
            {"name": "x", "kind": {"Other": 13}, "range": {"start": {"character": 9, "line": 1}, "end": {"character": 10, "line": 1}}},
            {"name": "s", "kind": {"Other": 13}, "range": {"start": {"character": 4, "line": 3}, "end": {"character": 5, "line": 3}}},
            {"name": "缓存", "kind": "Class", "range": {"start": {"character": 0, "line": 6}, "end": {"character": 16, "line": 8}}},
            {"name": "存", "kind": "Method", "range": {"start": {"character": 4, "line": 7}, "end": {"character": 16, "line": 8}}}
        ]);
        let text = "# 中文注释测试\ndef calc(x):\n    \"\"\"计算平方, 中文 docstring\"\"\"\n    s = \"中文字符串长度十二个字节以上测试\"\n    return x * x\n\nclass 缓存:\n    def 存(self, k, v):\n        return v\n";
        assert_eq!(
            hover_probe_positions(&wire, Some(text)),
            vec![(1, 4), (6, 6), (7, 8)],
            "calc (1,4) / 缓存 (6,6) / 存 (7,8)——语义 kind 优先且全在名字 token"
        );
    }

    #[test]
    fn probe_positions_cjk_prefix_name_uses_utf16_column() {
        // bd fq7f：名字前有 CJK 字符时列必须按 LSP UTF-16 单位计（CJK=2），
        // 不是 UTF-8 字节数（CJK=3）——按字节算列会越出名字 token（单字名
        // 直接落 ')'），pyright hover 恒 null，探针假 pending。
        let wire = json!([
            {"name": "问候", "kind": "Function", "range": {"start": {"character": 0, "line": 0}, "end": {"character": 26, "line": 0}}},
            {"name": "存", "kind": "Method", "range": {"start": {"character": 4, "line": 1}, "end": {"character": 21, "line": 1}}},
            {"name": "名", "kind": {"Other": 13}, "range": {"start": {"character": 16, "line": 1}, "end": {"character": 17, "line": 1}}}
        ]);
        let text = "def 问候(名字: str) -> str:\n    def 存(self, 名):\n        return 名\n";
        assert_eq!(
            hover_probe_positions(&wire, Some(text)),
            vec![(0, 4), (1, 8), (1, 16)],
            "问候 def 后 col 4；存 col 8；名 = 15 ASCII + 存(1 个 UTF-16 单位) = col 16；\
             按字节算会得 18，越出单字名 token"
        );
    }

    #[test]
    fn wait_ready_backoff_caps_at_2s() {
        assert_eq!(wait_ready_backoff(0), Duration::from_millis(500));
        assert_eq!(wait_ready_backoff(1), Duration::from_millis(1000));
        assert_eq!(wait_ready_backoff(2), Duration::from_millis(2000));
        assert_eq!(wait_ready_backoff(10), Duration::from_millis(2000), "封顶");
    }

    #[test]
    fn wait_ready_timeout_explicit_beats_env_and_invalid_env_falls_back() {
        // 显式 --timeout 永远优先。
        assert_eq!(wait_ready_timeout_secs(Some(5), Some("999")), 5);
        // 合法 env 覆盖默认。
        assert_eq!(wait_ready_timeout_secs(None, Some("300")), 300);
        // 非法 env（非数字 / 空白含非数字）→ warn + 默认。
        assert_eq!(
            wait_ready_timeout_secs(None, Some("abc")),
            WAIT_READY_DEFAULT_TIMEOUT_SECS
        );
        assert_eq!(
            wait_ready_timeout_secs(None, Some("")),
            WAIT_READY_DEFAULT_TIMEOUT_SECS
        );
        // 都没有 → 默认。
        assert_eq!(
            wait_ready_timeout_secs(None, None),
            WAIT_READY_DEFAULT_TIMEOUT_SECS
        );
    }

    #[test]
    fn wait_ready_stage_defaults_to_semantic_and_parses_symbol() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from(["serena-cli", "--project", ".", "wait-ready"]).unwrap();
        let Some(Cmd::WaitReady { stage, timeout, .. }) = cli.cmd else {
            panic!("expected wait-ready");
        };
        assert_eq!(stage, WaitStage::Semantic, "默认保持现行为 semantic");
        assert_eq!(timeout, None);
        let cli = Cli::try_parse_from([
            "serena-cli",
            "--project",
            ".",
            "wait-ready",
            "--stage",
            "symbol",
        ])
        .unwrap();
        assert!(matches!(
            cli.cmd,
            Some(Cmd::WaitReady {
                stage: WaitStage::Symbol,
                ..
            })
        ));
    }

    #[test]
    fn payload_is_empty_covers_envelope_and_bare_array_but_not_hover_objects() {
        assert!(payload_is_empty(&json!({ "items": [], "warning": "w" })));
        assert!(payload_is_empty(&json!([])));
        assert!(!payload_is_empty(&json!({ "items": [1] })));
        assert!(!payload_is_empty(
            &json!({ "compact": true, "items": ["a", "f:1:1"] })
        ));
        // hover 等无 items 的对象形态不算「空集合」——O2 hint 只管集合型工具。
        assert!(!payload_is_empty(&json!({ "contents": "" })));
    }

    // ---- bd serena-rust-mfht F6：warning_suggests_index_warming 语义收口 ----

    #[test]
    fn warning_suggests_index_warming_matches_we0_and_index_warming_keywords() {
        // we0 / 暖机相关关键词 → 提示语义未就绪。
        assert!(warning_suggests_index_warming(
            "semantic layer returned empty; type analysis may not be ready yet"
        ));
        assert!(warning_suggests_index_warming(
            "index warming: results may be partial"
        ));
        // 大小写不敏感（与 hover_ready 的 contains 一致）。
        assert!(warning_suggests_index_warming("Type Analysis NOT READY"));
        // 项目切换 / not installed 等无关 warning → 误报护栏。
        assert!(!warning_suggests_index_warming(
            "project switched: A -> B"
        ));
        assert!(!warning_suggests_index_warming(
            "python: language server for `python` not installed"
        ));
        assert!(!warning_suggests_index_warming(""));
    }

    #[test]
    fn find_first_source_file_skips_build_dirs_and_detects_by_ext() {
        let tmp =
            std::env::temp_dir().join(format!("serena-waitready-test-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("target")).unwrap();
        std::fs::write(tmp.join("target").join("aaa.rs"), "fn junk() {}").unwrap();
        std::fs::write(tmp.join("zmain.py"), "def main():\n    pass\n").unwrap();
        let hit = find_first_source_file(&tmp).unwrap();
        assert_eq!(hit, tmp.join("zmain.py"), "target/ 被跳过");
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// bd serena-rust-tjlm：清单类（Cargo.toml 排序先于 src/*.rs）不得当选默认
    /// 探针；只含清单的目录返 None（提示用户 --file）。
    #[test]
    fn find_first_source_file_skips_manifest_class_files() {
        let tmp = std::env::temp_dir().join(format!(
            "serena-waitready-manifest-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::write(tmp.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(tmp.join("package.json"), "{}").unwrap();
        std::fs::write(tmp.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        let hit = find_first_source_file(&tmp).unwrap();
        assert_eq!(
            hit,
            tmp.join("src").join("main.rs"),
            "Cargo.toml/package.json 必须让位给真源码"
        );
        std::fs::remove_dir_all(&tmp).ok();

        let only = std::env::temp_dir().join(format!(
            "serena-waitready-tomlonly-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&only).unwrap();
        std::fs::write(only.join("Cargo.toml"), "[package]\n").unwrap();
        assert!(
            find_first_source_file(&only).is_none(),
            "只有清单类文件 → None（等 --file 显式指定）"
        );
        std::fs::remove_dir_all(&only).ok();
    }
}

/// bd 3ab：残留 daemon 探活兜底的纯函数锚（netstat 解析 / 告警文案 / 映像白名单）。
#[cfg(test)]
mod residual_reap_tests {
    use super::*;

    /// 中文 Windows netstat 表头 + 同端口非 LISTENING 行必须跳过。
    #[test]
    fn netstat_parse_hits_listen_row_skips_established_and_headers() {
        let out = "\r\n 活动连接\r\n\r\n  Proto  本地地址          远程地址        状态           PID\r\n  TCP    127.0.0.1:7860    5.6.7.8:443           ESTABLISHED     999\r\n  TCP    127.0.0.1:7860    0.0.0.0:0              LISTENING       4092\r\n  TCP    [::]:7860         [::]:0                 LISTENING       111\r\n  UDP    127.0.0.1:7860    *:*                                    333\r\n";
        assert_eq!(parse_netstat_listeners(out, 7860), Some(4092));
    }

    #[test]
    fn netstat_parse_ignores_other_ports_and_port_number_suffixes() {
        // :17860 不得因 ends_with 误命中 :7860
        let out = "  TCP    0.0.0.0:17860     0.0.0.0:0              LISTENING       111\r\n  TCP    0.0.0.0:7861      0.0.0.0:0              LISTENING       222\r\n";
        assert_eq!(parse_netstat_listeners(out, 7860), None);
        assert_eq!(parse_netstat_listeners(out, 7861), Some(222));
    }

    #[test]
    fn residual_warn_msg_names_pid_and_port_and_manual_action() {
        let m = residual_warn_msg(Some(4092), 7860);
        assert!(
            m.contains("4092") && m.contains("7860") && m.contains("taskkill"),
            "{m}"
        );
        let m = residual_warn_msg(None, 7860);
        assert!(m.contains("7860") && m.contains("netstat"), "{m}");
    }

    #[test]
    fn serena_image_match_is_case_insensitive_and_excludes_foreign() {
        assert!(is_serena_daemon_image("serena-cli.exe"));
        assert!(is_serena_daemon_image("SERENA-CLI.EXE"));
        assert!(is_serena_daemon_image("cli.exe"));
        assert!(!is_serena_daemon_image("python.exe"));
        assert!(!is_serena_daemon_image("serena-cli-helper.exe"));
    }
}

/// ls-remove 的参数化单测（bd serena-rust-4ux）：临时 fake cache 三例——
/// 越界拒绝 / 正常删除 / 未知 id 列已装清单。
#[cfg(test)]
mod ls_remove_tests {
    use super::*;

    fn fake_cache(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "serena-ls-remove-test-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("marksman").join("0.10.0")).unwrap();
        std::fs::write(
            d.join("marksman").join("0.10.0").join("marksman.exe"),
            b"fake",
        )
        .unwrap();
        std::fs::create_dir_all(d.join("zls").join("latest")).unwrap();
        d
    }

    #[test]
    fn ls_remove_deletes_only_target_id_dir() {
        let root = fake_cache("ok");
        let (path, bytes) = ls_remove_dir(&root, "marksman").expect("删除应成功");
        let shown = path.display().to_string();
        assert!(!shown.contains(r"\\?\"), "返回路径不得带 UNC 前缀: {shown}");
        assert!(path.ends_with("marksman"));
        assert!(bytes > 0, "释放字节数必须 >0");
        assert!(!root.join("marksman").exists());
        assert!(root.join("zls").is_dir(), "其它 id 目录必须幸存");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ls_remove_rejects_path_traversal_and_separators() {
        let root = fake_cache("traversal");
        for bad in ["..", "a/b", "a\\b", "."] {
            let err = ls_remove_dir(&root, bad).expect_err("必须拒绝");
            assert!(matches!(err, LsRemoveFail::BadArgs(_)), "bad={bad}");
        }
        assert!(root.join("marksman").is_dir(), "拒绝时不得删任何目录");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn ls_remove_unknown_id_lists_installed() {
        let root = fake_cache("unknown");
        let err = ls_remove_dir(&root, "nosuchls").expect_err("未知 id 必须报错");
        match err {
            LsRemoveFail::NotFound(m) => {
                assert!(
                    m.contains("marksman") && m.contains("zls"),
                    "须列已装 id: {m}"
                );
                assert!(m.contains("nosuchls"));
            }
            other => panic!("want NotFound, got {other:?}"),
        }
        std::fs::remove_dir_all(&root).ok();
    }
}

/// BD serena-rust-3bu：shell 长会话断流自愈（connect 失败 → 重跑 ensure_daemon）。
#[cfg(test)]
mod shell_selfheal_tests {
    use super::*;

    /// 死端口 connect 失败（退避耗尽）→ 注入的 reensure 被调用 → 换新 base 重发
    /// 命中 mock daemon → 响应透传，base_token 更新为新 daemon。
    #[tokio::test]
    async fn shell_dispatch_selfheals_after_connect_failure() {
        // 死端口：bind 后立即 drop → connect refused。
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);

        // mock daemon：一次性 HTTP 200 JSON 响应（原生 TcpListener，免引入 server 依赖）。
        let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mock_port = srv.local_addr().unwrap().port();
        let payload = r#"{"ok":true,"data":42}"#;
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut sock, _) = srv.accept().expect("mock daemon accepts one conn");
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            sock.write_all(resp.as_bytes()).expect("mock daemon writes resp");
        });

        let client = http_client();
        let mut base_token = (format!("http://127.0.0.1:{dead_port}"), "tok".to_string());
        let healed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = healed.clone();
        let resp = dispatch_shell_cmd_with(
            &client,
            &mut base_token,
            std::path::Path::new("unused-lock"),
            std::path::Path::new("."),
            "read-file",
            json!({"file": "a.rs"}),
            move || {
                let flag = flag.clone();
                async move {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok((format!("http://127.0.0.1:{mock_port}"), "tok2".to_string()))
                }
            },
        )
        .await
        .expect("自愈后重发应成功");
        server.join().unwrap();

        assert!(
            healed.load(std::sync::atomic::Ordering::SeqCst),
            "connect 失败必须触发重探活"
        );
        assert_eq!(resp, json!(42), "mock daemon 响应应透传");
        assert_eq!(
            base_token.0,
            format!("http://127.0.0.1:{mock_port}"),
            "base 应更新为新 daemon 地址"
        );
        assert_eq!(base_token.1, "tok2");
    }

    /// 自愈探活本身失败：错误链必须同时携带原连接错误与 reensure 失败事实。
    #[tokio::test]
    async fn selfheal_failure_surfaces_both_errors() {
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);

        let client = http_client();
        let mut base_token = (format!("http://127.0.0.1:{dead_port}"), "tok".to_string());
        let err = dispatch_shell_cmd_with(
            &client,
            &mut base_token,
            std::path::Path::new("unused-lock"),
            std::path::Path::new("."),
            "read-file",
            json!({"file": "a.rs"}),
            || async { Err("spawn refused".to_string()) },
        )
        .await
        .expect_err("探活失败必须报错");

        assert!(err.contains("reconnect self-heal failed"), "{err}");
        assert!(err.contains("spawn refused"), "{err}");
    }
}

/// bd 30m：status 纯探测语义回归锁——daemon 不在时报 not-running 且零副作用
/// （lazy-spawn 出的 daemon 必然写 lock，lock 缺席即证未 spawn）。
#[cfg(test)]
mod status_tests {
    use super::*;

    #[tokio::test]
    async fn status_absent_daemon_never_spawns() {
        let lock = std::env::temp_dir().join(format!(
            "serena-status-nospawn-{}.lock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&lock);

        let code = cmd_status(&lock).await;

        assert_eq!(code, ExitCode::from(1), "daemon 不在 = not-running (exit 1)");
        assert!(!lock.exists(), "status 不得 lazy-spawn（lock 出现 = 有 daemon 被拉起）");
    }

    /// 死 lock（文件在、端口无 listener）同契约：不 spawn、报 not-running。
    #[tokio::test]
    async fn status_with_dead_lock_never_spawns() {
        let lock = std::env::temp_dir().join(format!(
            "serena-status-deadlock-{}.lock",
            std::process::id()
        ));
        std::fs::write(&lock, "{}").unwrap();

        let code = cmd_status(&lock).await;

        assert_eq!(code, ExitCode::from(1));
        assert!(lock.exists(), "status 不得动死 lock");
        let _ = std::fs::remove_file(&lock);
    }

    /// bd v3yv：project-info 对不在的 daemon 同样纯探测（不 spawn、成功返回 meta）。
    #[tokio::test]
    async fn project_info_absent_daemon_never_spawns() {
        let lock = std::env::temp_dir().join(format!(
            "serena-projinfo-nospawn-{}.lock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&lock);

        let code = cmd_project_info(&lock, None).await;

        assert_eq!(code, ExitCode::SUCCESS, "daemon 不在也输出 meta（git + 空加载态）");
        assert!(!lock.exists(), "project-info 不得 lazy-spawn");
    }

    /// bd zyrg：change-history 记录头解析——\x01 切分 + sha/epoch/subject 三段；
    /// 空 sha 记录与畸形行跳过；-L 模式的 patch 体不混入。
    #[test]
    fn change_history_head_parse_skips_malformed_records() {
        let raw = "\x01abc123\t1700000000\tfeat: first\n\x01\n\x01deadbeef\tnot-a-number\tbad ts\n\x1fe327fa5\t1700000001\tfix: second";
        let commits: Vec<serde_json::Value> = raw
            .split('\x01')
            .filter_map(|rec| {
                let head = rec.lines().next()?;
                let mut parts = head.splitn(3, '\t');
                let sha = parts.next()?.trim();
                if sha.is_empty() {
                    return None;
                }
                let ts = parts.next()?.parse::<u64>().unwrap_or(0);
                Some(json!({ "sha": sha, "committed_at": ts, "subject": parts.next().unwrap_or_default() }))
            })
            .collect();
        assert_eq!(commits.len(), 2, "空 sha 记录跳过: {commits:?}");
        assert_eq!(commits[0]["sha"], "abc123");
        assert_eq!(commits[0]["committed_at"], 1700000000);
        assert_eq!(commits[0]["subject"], "feat: first");
        // epoch 解析失败容忍为 0（不丢整条记录）。
        assert_eq!(commits[1]["sha"], "deadbeef");
        assert_eq!(commits[1]["committed_at"], 0);
    }

    /// bd v3yv：HEAD sha 解析三态——直接 ref 文件 / packed-refs 兜底 / detached。
    #[test]
    fn resolve_git_head_sha_covers_ref_packed_and_detached() {
        let dotgit = std::env::temp_dir().join(format!(
            "serena-git-sha-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dotgit.join("refs/heads")).unwrap();

        // ① 直接 ref 文件。
        std::fs::write(
            dotgit.join("refs/heads/main"),
            "36b0471abcdef0123456789abcdef0123456789\n",
        )
        .unwrap();
        std::fs::write(dotgit.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let sha = resolve_git_head_sha(&dotgit, "ref: refs/heads/main").expect("ref file sha");
        assert!(sha.starts_with("36b0471"), "got: {sha}");

        // ② packed-refs 兜底（ref 文件不存在）。
        std::fs::remove_file(dotgit.join("refs/heads/main")).unwrap();
        std::fs::write(
            dotgit.join("packed-refs"),
            "# pack-refs with: peeled \n31bf612000000000000000000000000000000000 refs/heads/main\n",
        )
        .unwrap();
        let sha = resolve_git_head_sha(&dotgit, "ref: refs/heads/main").expect("packed sha");
        assert!(sha.starts_with("31bf612"), "got: {sha}");

        // ③ detached HEAD。
        let sha =
            resolve_git_head_sha(&dotgit, "e327fa5000000000000000000000000000000000").expect("detached");
        assert!(sha.starts_with("e327fa5"), "got: {sha}");
        let _ = std::fs::remove_dir_all(&dotgit);
    }
}

/// 盲测波 Wave1-B：wait-ready LS_NOT_INSTALLED fail-fast（bd serena-rust-nqjo）+
/// unknown tool 版本错位 hint（bd serena-rust-eog5）的纯函数锁。
#[cfg(test)]
mod blindfix_b_tests {
    use super::*;

    #[test]
    fn not_installed_exit_failfasts_on_wire_code() {
        let e = r#"{"code":"LS_NOT_INSTALLED","message":"language server `pyright` not found in PATH; install_hint: npm install -g pyright","retryable":false}"#;
        let code = not_installed_exit(e, Some("python")).expect("确定性错误必须 fail-fast");
        assert_eq!(code, ExitCode::from(1));
    }

    #[test]
    fn not_installed_exit_keeps_waiting_on_transient_and_other_codes() {
        // transport 层（非 JSON）= 瞬态。
        assert!(not_installed_exit("hover: transport 503: {...}", Some("python")).is_none());
        // 其他 wire code ≠ 未装（如 BAD_ARGS）不该误判成 install 问题。
        let bad = r#"{"code":"BAD_ARGS","message":"invalid file"}"#;
        assert!(not_installed_exit(bad, None).is_none());
    }

    #[test]
    fn unknown_tool_hint_hits_only_bad_args_unknown_tool() {
        let hit = json!({"code":"BAD_ARGS","message":"unknown tool: recipe"});
        assert_eq!(
            unknown_tool_hint(&hit),
            Some("daemon may have been started by an older binary; run `serena-cli stop-all` and retry")
        );
        // 同文案但非 BAD_ARGS → 不 hint。
        let internal = json!({"code":"INTERNAL","message":"unknown tool: recipe"});
        assert!(unknown_tool_hint(&internal).is_none());
        // BAD_ARGS 但不是 unknown tool → 不 hint。
        let other = json!({"code":"BAD_ARGS","message":"invalid server id `x`"});
        assert!(unknown_tool_hint(&other).is_none());
        // 非 object 错误体 → 不 hint。
        assert!(unknown_tool_hint(&json!("unknown tool: recipe")).is_none());
    }

    #[test]
    fn wire_err_code_reads_code_field_only() {
        let e = r#"{"code":"LS_TIMEOUT","message":"x"}"#;
        assert_eq!(wire_err_code(e).as_deref(), Some("LS_TIMEOUT"));
        assert_eq!(wire_err_code("transport 500: internal"), None);
        assert_eq!(wire_err_code(r#"{"message":"no code"}"#), None);
    }
}

/// bd dbx1：3 并发 lazy-spawn 时所有 CLI 对同一 :7860 wait_ready 全 200，
/// 必须用 lock.pid 反查确认谁真 spawn 了 daemon。下面是 own_child_won 纯函数
/// 的契约测试（写 temp lockfile 模拟 OS bind 赢家/败家状态）。临时目录用
/// std::env::temp_dir + pid+test 名拼唯一路径（与 cli 现有测试一致；cli 不引
/// tempfile crate 作为 dev-dep，由 sweep_stale_serena_tempdirs 清扫老化残留）。
#[cfg(test)]
mod dbx1_concurrent_lazy_spawn_tests {
    use super::*;
    use daemon::lockfile::{LockEntry, write_final};
    use std::path::PathBuf;

    fn fresh_lock(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "serena-dbx1-{}-{}-{}.lock",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn write_lock_with_pid(path: &std::path::Path, pid: u32) {
        let entry = LockEntry {
            pid,
            port: 7860,
            boot_ms: 1,
            token: "deadbeef".repeat(4),
        };
        write_final(path, &entry).expect("write lock");
    }

    #[test]
    fn own_child_won_returns_true_when_lock_pid_matches_spawned() {
        let lock = fresh_lock("match");
        write_lock_with_pid(&lock, 4242);
        let r = own_child_won(&lock, 4242);
        let _ = std::fs::remove_file(&lock);
        assert!(r, "lock.pid == spawned PID → 胜家");
    }

    #[test]
    fn own_child_won_returns_false_when_lock_pid_is_peer() {
        let lock = fresh_lock("peer");
        write_lock_with_pid(&lock, 9999);
        let r = own_child_won(&lock, 4242);
        let _ = std::fs::remove_file(&lock);
        assert!(
            !r,
            "lock.pid != spawned PID → 败家走 attach"
        );
    }

    #[test]
    fn own_child_won_returns_false_when_lock_absent() {
        let lock = fresh_lock("absent");
        // 不写 lock —— 模拟 bind 赢家刚 spawn 还未来得及 write_final 的窗口。
        let r = own_child_won(&lock, 4242);
        assert!(
            !r,
            "无 lock → 保守判败家（attach 等读 lock 重试而非误报 spawn）"
        );
    }

    #[test]
    fn own_child_won_treats_corrupt_lock_as_lost() {
        let lock = fresh_lock("corrupt");
        std::fs::write(&lock, b"not valid json").unwrap();
        let r = own_child_won(&lock, 4242);
        let _ = std::fs::remove_file(&lock);
        assert!(!r, "lock 损坏 → 败家兜底");
    }
}

/// bd serena-rust-p2zp：clap 原生 parse 错（missing/invalid/unknown）必须走
/// wire JSON error 对象（{code,message,retryable}）+ rc=2，与 07u5 工具语义错
/// 路径同形。下面三单测锁住：(a) 真错 use_stderr=true → 退出码 2 + JSON 含
/// code BAD_ARGS；(b) help/version use_stderr=false → 退出码 0（原生渲染保留）。
/// 直接调用 `clap_exit_to_json` 而不走 cli_main 全链路——避免进程外断言。
#[cfg(test)]
mod clap_exit_json_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn missing_required_arg_returns_bad_args_json_and_rc2() {
        let err = Cli::try_parse_from(["serena-cli", "read-file"]).expect_err("missing arg");
        assert!(err.use_stderr(), "missing arg = 真错误 → use_stderr=true");
        let code = clap_exit_to_json(err);
        assert_eq!(code, ExitCode::from(2), "rc=2 与 07u5 工具语义错一致");
    }

    #[test]
    fn invalid_value_returns_bad_args_json_and_rc2() {
        let err = Cli::try_parse_from([
            "serena-cli",
            "find-symbol",
            "foo",
            "--format",
            "not_a_real_format",
        ])
        .expect_err("bad value");
        assert!(err.use_stderr());
        let code = clap_exit_to_json(err);
        assert_eq!(code, ExitCode::from(2));
    }

    #[test]
    fn unknown_subcommand_returns_bad_args_json_and_rc2() {
        let err =
            Cli::try_parse_from(["serena-cli", "totally-unknown-sub"]).expect_err("unknown cmd");
        assert!(err.use_stderr());
        let code = clap_exit_to_json(err);
        assert_eq!(code, ExitCode::from(2));
    }

    #[test]
    fn help_request_returns_success_without_json() {
        let err = Cli::try_parse_from(["serena-cli", "--help"]).expect_err("--help = DisplayHelp");
        assert!(
            !err.use_stderr(),
            "--help 用 use_stderr=false 走原生渲染 + 退出 0"
        );
        let code = clap_exit_to_json(err);
        assert_eq!(code, ExitCode::SUCCESS, "help/version 不走 JSON 路径");
    }

    #[test]
    fn version_request_returns_success_without_json() {
        let err = Cli::try_parse_from(["serena-cli", "--version"]).expect_err("--version = DisplayVersion");
        assert!(!err.use_stderr());
        let code = clap_exit_to_json(err);
        assert_eq!(code, ExitCode::SUCCESS);
    }

    // ---- bd serena-rust-mfht F7：status 接受 --project（与 project-info 形状对齐）----

    #[test]
    fn status_accepts_project_flag_with_or_without_subcommand_first() {
        // 子命令前 `--project X status`：Cli 全局 flag + 子命令后置位。
        let cli = Cli::try_parse_from(["serena-cli", "--project", "X", "status"]).unwrap();
        assert!(
            matches!(cli.cmd, Some(Cmd::Status { .. })),
            "--project X status 必须解析为 Status 子命令"
        );
        // 子命令后 `status --project X`：status 子命令内 `--project` 字段。
        let cli = Cli::try_parse_from(["serena-cli", "status", "--project", "X"]).unwrap();
        assert!(
            matches!(cli.cmd, Some(Cmd::Status { .. })),
            "status --project X 必须解析为 Status 子命令（不得报 unexpected）"
        );
    }
}

/// bd fdmj-F5：进度行按档消歧的形态锁定。
#[cfg(test)]
mod wait_ready_progress_tests {
    use super::*;

    #[test]
    fn semantic_stage_drops_symbol_ok_prefix() {
        let p = wait_ready_progress(WaitStage::Semantic, true, 3, "lib.rs");
        assert_eq!(p, "hover-pending");
        assert!(!p.contains("symbol-ok"), "symbol-ok 对 semantic 等待者是噪音");
    }

    #[test]
    fn semantic_stage_no_probeable_identifier_keeps_root_cause_note() {
        let p = wait_ready_progress(WaitStage::Semantic, true, 0, "lib.rs");
        assert_eq!(p, "hover-pending (no probeable identifier in lib.rs)");
    }

    #[test]
    fn def_stage_reports_def_pending_not_hover() {
        let p = wait_ready_progress(WaitStage::Def, true, 2, "lib.rs");
        assert_eq!(p, "def-pending");
        assert!(!p.contains("hover"), "def 档不得误打 hover 字样");
    }

    #[test]
    fn symbol_layer_down_reports_symbol_pending_for_all_stages() {
        for stage in [WaitStage::Symbol, WaitStage::Semantic, WaitStage::Def] {
            assert_eq!(
                wait_ready_progress(stage, false, 0, "lib.rs"),
                "symbol-pending",
                "符号层未就绪时 pending 字样与档位无关"
            );
        }
    }

    #[test]
    fn symbol_stage_keeps_symbol_ok_target_semantics() {
        assert_eq!(
            wait_ready_progress(WaitStage::Symbol, true, 1, "lib.rs"),
            "symbol-ok"
        );
    }
}

/// bd fakewait：drain 窗口自愈链的单元契约。mock daemon 用手写 HTTP 响应
/// （与 shell_selfheal_tests 同手法）；temp lock 用 temp_dir + 唯一名
/// （cli 不引 tempfile dev-dep，见 dbx1 测试注释）。
#[cfg(test)]
mod drain_takeover_tests {
    use super::*;
    use daemon::lockfile::{LockEntry, write_final};
    use std::path::PathBuf;

    fn fresh_lock(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "serena-fakewait-{}-{}-{}.lock",
            std::process::id(),
            name,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn test_entry(port: u16) -> LockEntry {
        LockEntry {
            pid: 4242,
            port,
            boot_ms: 1,
            token: "deadbeef".repeat(4),
        }
    }

    /// mock daemon：bind 随机端口，对 GET /status 回指定 status line + JSON body。
    /// std::thread + 先读后写（与 shell_selfheal_tests 同手法；tokio accept task
    /// 在 current_thread runtime 下会被 reqwest 连接先 RST）。accept 循环应对
    /// wait_ready 轮询的多请求；线程随测试进程退出回收。
    fn spawn_status_mock(status_line: &str, body: serde_json::Value) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().unwrap().port();
        let status_line = status_line.to_string();
        let payload = body.to_string();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { break };
                let status_line = status_line.clone();
                let payload = payload.clone();
                std::thread::spawn(move || {
                    use std::io::{Read, Write};
                    let mut buf = [0u8; 4096];
                    let _ = sock.read(&mut buf);
                    let resp = format!(
                        "{status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        payload.len(),
                        payload
                    );
                    let _ = sock.write_all(resp.as_bytes());
                });
            }
        });
        port
    }

    const OK_200: &str = "HTTP/1.1 200 OK";

    #[tokio::test]
    async fn alive_but_draining_true_only_on_explicit_flag() {
        let port = spawn_status_mock(OK_200, json!({"draining": true}));
        assert!(alive_but_draining(&test_entry(port)).await, "draining:true 必须识别");
    }

    #[tokio::test]
    async fn alive_but_draining_false_on_live_and_on_unreachable() {
        let port = spawn_status_mock(OK_200, json!({"draining": false}));
        assert!(!alive_but_draining(&test_entry(port)).await, "活 daemon 不误杀");
        // 连不上 = 拿不到明确标志 → 保守判活（保持既有探活语义）。
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        assert!(!alive_but_draining(&test_entry(dead_port)).await, "无响应保守判活");
    }

    #[tokio::test]
    async fn wait_ready_poll_ignores_draining_and_accepts_live() {
        // draining:true 的 200 不算就绪 → 短窗耗尽必 Err。
        let drain_port = spawn_status_mock(OK_200, json!({"draining": true}));
        assert!(
            wait_ready(drain_port, Duration::from_millis(1200)).await.is_err(),
            "draining /status 200 不得判就绪"
        );
        // draining:false → 首轮即 Ok。
        let live_port = spawn_status_mock(OK_200, json!({"draining": false}));
        assert!(
            wait_ready(live_port, Duration::from_secs(3)).await.is_ok(),
            "正常 /status 判就绪不倒退"
        );
    }

    #[tokio::test]
    async fn wait_drain_outcome_attaches_live_lock_and_spawns_when_gone() {
        // Attach：lock 活 + 不 draining → 直接给出 entry。
        let port = spawn_status_mock(OK_200, json!({"draining": false}));
        let lock = fresh_lock("attach");
        write_final(&lock, &test_entry(port)).expect("write lock");
        match wait_drain_outcome(&lock, port, Duration::from_secs(3)).await {
            Ok(DrainOutcome::Attach(e)) => assert_eq!(e.port, port),
            other => panic!("expected Attach, got {other:?}"),
        }
        let _ = std::fs::remove_file(&lock);
        // ReadyToSpawn：lock 消失 + 端口空 → 立即可 spawn。
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let gone = fresh_lock("gone");
        match wait_drain_outcome(&gone, dead_port, Duration::from_secs(3)).await {
            Ok(DrainOutcome::ReadyToSpawn) => {}
            other => panic!("expected ReadyToSpawn, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn wait_drain_outcome_times_out_while_peer_keeps_draining() {
        let port = spawn_status_mock(OK_200, json!({"draining": true}));
        let lock = fresh_lock("timeout");
        write_final(&lock, &test_entry(port)).expect("write lock");
        let r = wait_drain_outcome(&lock, port, Duration::from_millis(800)).await;
        assert!(r.is_err(), "peer 恒 draining 必须超时 Err 而非空转");
        assert!(r.unwrap_err().contains("still draining"));
        let _ = std::fs::remove_file(&lock);
    }
}
