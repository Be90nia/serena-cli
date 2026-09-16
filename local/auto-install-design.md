# serena-rust 自动下载机制设计 v0.4

> **修订史**：v0.1 → oracle 评审（1C+3I+7M）→ v0.2 → 自审 11 项 → v0.3 → momus 终审打回（1C+7I+5M，通过条件 5 条）→ **v0.4 全部落地**。
> 锚：oraios/serena@43ae0211；数据底盘 `local/upstream-ls-catalog.md`。
> **Task 编号映射（momus I-2）**：本设计 §7 中 Task 18-21 = PLAN.md 既有槽位的就地细化；PLAN.md Task 22-27（safe_delete / read_file / find_ref_snippets / 编辑四件套 / --record / --json）**维持原义、未执行、不受本设计影响**；执行史 commit d9de43c 所用「Task 19-22」标签作废（当时实为工具开发）；本设计新任务从 **Task 29** 起（28 留空防混淆）。

## 0. 目标 & 非目标

### 目标

1. `git clone serena-rust → cargo build → cargo install`，**仅依赖系统 runtime（Node/.NET/Ruby/JDK/Go 等）即用**。所有 LS 二进制由本工具负责下载与升级。
2. 73 个上游 LS 全覆盖（按 solidlsp 同等粒度）。
3. **双路径**：
   - **路径 A（默认）**：`auto_install = false`，仅 PATH 找 → 无则 `ToolError::NotInstalled` 含安装命令。**永不触网**（含 uvx 预热）。
   - **路径 B（opt-in）**：`auto_install = true` 或 `serena-cli install <ls>` 显式触发。
4. **优先级模型**：CLI flag > 用户全局 `config.toml` > `servers.toml` 默认。**项目级配置不参与 cmd 构造**（防恶意仓库注入二进制）。
5. **完整性 gate（按类别，§2.9）**：A 类 sha256 未知 → 拒绝 auto，仅 `install --allow-unsigned-sha` 越狱；B/C/D/E 类信任包管理器完整性（TLS + 官方源 + 版本钉死）。
6. **用户级 override**：CLI `--ls-path / --ls-base-cmd / --ls-args` 覆盖拉起方式；文件不存在 → `RuntimeError::MissingRuntime` 含 hint。

### 非目标

- 不做 LS 源码编译（nixd 归 PATH-only；macOS hlsl 走 PATH 提示）。
- 不做跨平台 fallback。
- 不做每日自动升级（仅 `install --update`）。
- HTTPS only；不做项目级 override；**不设任何环境变量 override**。
- 外部 servers.toml 覆盖（ARCH §1 提及的 %APPDATA% 外部文件）：**v1 不做**，Δ 记入 ARCH 修订备注；内置单表已够 73 条。

## 1. 安装方式分档（= catalog 8 类）

| 类 | 方式 | 数量 | LS |
|---|---|---|---|
| A | 单二进制下载（zip/tar.gz/tar.xz） | **27** | ada, al, bsl, csharp, clangd, clojure, cue, dart, eclipse_jdtls, elixir, haxe, hlsl, kotlin, lua, luau, marksman, matlab, nextflow, omnisharp, pascal, phpactor, phpantom, powershell, systemverilog, taplo, terraform, texlab |
| B | npm | **14** | angular, ansible, bash, elm, intelephense, json, solidity, some-sass, svelte, typescript, vscode_html, vts, vue, yaml |
| C | uvx（pip） | **5** | pyright, basedpyright, pyrefly, ty, fortls |
| D | dotnet tool | **1** | fsharp |
| E | gem | **2** | ruby-lsp, solargraph |
| F | 系统包/PATH only（不下载） | **19** | ccls, crystal, deno, erlang_ls, gleam, gopls, haskell-ls, jedi-language-server, lean4, nixd, ocaml-lsp, perl_ls, qmlls, r-languageserver, regal, rust-analyzer, sourcekit-lsp, wolfram-kernel, zls |
| G | 特殊 | **5** | gdscript(godot TCP), groovy(用户 JAR+JRE 自动), julia(系统 julia+用户自装 LSP), msl(vendor 脚本), scala(coursier) |

合计 27+14+5+1+2+19+5 = **73** ✓

**tar.xz 处理**：调系统 `xz`/`tar`（std::process），不引入 xz2 crate（守 ARCH §8）。缺工具 → `MissingRuntime{what:"xz"}`。

## 2. `servers.toml` schema

**server key 规则**：一律用**语言 id**（`typescript`、`clangd`、`gopls`），不用包名——用户 `[ls.<id>]` 配置键唯一。

**占位符注册表**（全文档唯一来源，出现在 `exec`/`check_cmd` 中）：

| 占位符 | 含义 |
|---|---|
| `{bin}` | 安装/探测到的 LS 可执行文件绝对路径 |
| `{jar}` | groovy 用户提供的 JAR 路径 |
| `{jre}` | 自动下载的 JRE 根目录 |
| `{script}` | msl 脚本物化后的绝对路径（§2.8） |
| `{py}` | Python 解释器（PATH `python` 探测） |
| `{julia}` | julia 解释器（PATH 探测） |
| `{version}` | 该条目 `version` 字段值（防 exec 硬拷漂移） |
| `{cache_root}` | §5 缓存根目录 |

### 2.1 通用字段

```toml
[servers.clangd]
languages = ["cpp"]
extensions = [".c", ".cpp", ".h", ".hpp"]
priority = 10
required_root_patterns = []
install = "download"
```

### 2.2 A 类 download

```toml
[servers.clangd.download]
version = "18.1.5"
# kind 枚举钉死："zip" | "tar.gz" | "tar.xz" | "vsix"（vsix = zip 变体，按 zip 解）
archive = { kind = "tar.xz", strip_components = 1 }
exec = ["{bin}"]
allowed_hosts = ["github.com", "objects.githubusercontent.com"]

# bin_path 支持 per-platform map（Windows 产物常带 .exe，Unix 不带）
[servers.clangd.download.bin_path_per_platform]
"windows-x86_64" = "bin/clangd.exe"
"linux-x86_64" = "bin/clangd"
"macos-x86_64" = "bin/clangd"
"macos-aarch64" = "bin/clangd"

[servers.clangd.download.url_per_platform]
"windows-x86_64" = "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-x86_64-pc-windows-msvc.tar.xz"
"linux-x86_64" = "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-x86_64-linux-gnu-ubuntu-22.04.tar.xz"
"macos-x86_64" = "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-x86_64-apple-darwin.tar.xz"
"macos-aarch64" = "https://github.com/llvm/llvm-project/releases/download/llvmorg-18.1.5/clang+llvm-18.1.5-aarch64-apple-darwin.tar.xz"

[servers.clangd.download.sha256_per_platform]
"windows-x86_64" = "<64hex>"
"linux-x86_64" = "<64hex>"
"macos-x86_64" = "<64hex>"
"macos-aarch64" = "<64hex>"
```

**any 平台单资产形态**（bsl JAR / haxe VSIX / phpactor PHAR / powershell zip 等 6/27 无平台维度）：

```toml
[servers.phpactor.download]
version = "<ver>"
archive = { kind = "zip", strip_components = 0 }
exec = ["{bin}"]
url = "https://github.com/phpactor/phpactor/releases/download/{version}/phpactor.phar"   # 单 URL，{version} 展开
sha256 = "<64hex>"
bin_path = "phpactor.phar"
```

多平台 VSIX（al/matlab）用 `url_per_platform` + `bin_path_per_platform` 常规形态。
多产物 LS（jdtls/omnisharp）加 `[[servers.<id>.download.secondary_assets]]` 数组表（id/kind/url|url_per_platform/sha256/bin_path）。

### 2.3 B 类 npm

```toml
[servers.typescript.npm]
package = "typescript-language-server"
version = "5.1.3"
bin_name = "typescript-language-server"
exec = ["{bin}", "--stdio"]
[[servers.typescript.npm.secondary_packages]]
package = "typescript"
version = "5.9.3"
```

### 2.4 C 类 uvx

```toml
[servers.pyright.uvx]
package = "pyright"
version = "1.1.396"
entrypoint = "pyright-langserver"
exec = ["uvx", "-p", "3.13", "--from", "pyright=={version}", "{entrypoint}", "--stdio"]
```

### 2.5 D 类 dotnet / 2.6 E 类 gem

```toml
[servers.fsharp.dotnet]
tool = "fsautocomplete"
version = "<ver>"
exec = ["dotnet", "tool", "run", "fsautocomplete"]

[servers.ruby-lsp.gem]
gem = "ruby-lsp"
version = "<ver>"
bin_name = "ruby-lsp"
exec = ["{bin}"]
```

### 2.7 F 类 path_only

```toml
[servers.gopls.path_only]
binary_name = "gopls"
exec = ["{bin}"]
install_hint = "go install golang.org/x/tools/gopls@latest"
```

`install_hint` 由 spec 数据生成（非硬编码）。

### 2.8 G 类特殊

```toml
[servers.gdscript.tcp_external]
host = "127.0.0.1"
port = 6008          # 零安装；需 Godot 编辑器在跑

[servers.groovy.groovy_jar]
ls_jar_path_required = true
[servers.groovy.groovy_jar.jre]          # 结构同 A 类 download
exec = ["{jre}/bin/java", "-jar", "{jar}"]

[servers.julia.julia_pkg]
check_cmd = ["{julia}", "-e", "using LanguageServer"]
exec = ["{julia}", "--project={cache_root}/julia-env", "-e", "using LanguageServer; runserver()"]

[servers.msl.bundled_script]
# include_str! 内嵌进二进制，首次使用物化到 {cache_root}/bundled/ —— cargo install 后不依赖仓库
exec = ["{py}", "{script}"]
required_runtime = "python"

[servers.scala.coursier]
version = "<ver>"
exec = ["cs", "bootstrap", "-o", "{bin}", "org.scalameta:metals_2.13:{version}"]
```

### 2.9 完整性模型（按类别）

| 类 | 完整性来源 | auto_install 门 |
|---|---|---|
| A（+G 的 jre） | `sha256_per_platform` / 单资产 `sha256` 显式值 | **sha 缺失/未知 → 拒绝**；仅 `install --allow-unsigned-sha` 越狱（人类显式，agent 永不） |
| B npm | npm registry integrity（TLS + 官方源 + 版本钉死） | 信任 npm |
| C uvx | TLS + PyPI 官方源 + 版本钉死（与 npm 同级；uv 对钉死版本默认不做额外 hash 校验，如实记录） | 信任 uv；**路径 A 永不预热** |
| D dotnet | NuGet 签名包 + 版本钉死 | 信任 dotnet |
| E gem | gem checksum + 版本钉死 | 信任 gem |
| F/G | 无下载（groovy JRE 走 A 类门） | — |

**transport**：`TransportKind` 在 `ls-runtime/src/process.rs:31`（现状仅 `Stdio`），新增变体在此文件：

```rust
pub enum TransportKind {
    Stdio,
    Tcp { host: String, port: u16 },   // gdscript；lsp-core transport 层加 Tcp 泵，client.rs 拓扑不变
}
```

## 3. ls-runtime::deps 扩展

**依赖方向（momus Critical-1 修复）**：`ServerSpec` 归 ls-registry（ARCH §1），而 Cargo 边为 ls-registry→ls-adapters→ls-runtime ⇒ **ls-runtime 禁止引用 ServerSpec**。本 crate 自足定义值子集 `InstallSpec`，由 ls-registry 的 ConfigAdapter 做 `ServerSpec → InstallSpec` 映射：

```rust
/// ls-runtime 自足类型（serde 反序列化亦可直接从 toml 子表来）
pub struct InstallSpec {
    pub id: String,                       // 语言 id
    pub kind: InstallKind,                // Download{..} / Npm{..} / Uvx{..} / Dotnet{..} / Gem{..} / PathOnly{..} / TcpExternal{..} / ...
    pub exec: Vec<String>,                // 含占位符模板
}

pub struct InstallCtx {
    pub os: Os,
    pub arch: Arch,
    pub auto_install: bool,
    pub allow_unsigned_sha: bool,
    pub cache_root: PathBuf,
    pub http: reqwest::Client,     // 调用方注入共享客户端
}

pub enum Launch {
    /// 本地进程产物
    Process { exe: PathBuf, args: Vec<String> },
    /// 外部服务（godot TCP），零本地产物
    External { host: String, port: u16 },
}

pub enum InstallOutcome {
    Ready(Launch),
    NotInstalled { hint: String, install_cmd: Option<String> },
    UnsignedRefused { ls_id: String, hint: String },   // wire 映射 = LS_NOT_INSTALLED + hint
}

pub trait DependencySource: Send + Sync {
    /// 同步签名：daemon(tokio) 调用方须 spawn_blocking 包裹（下载分钟级 < ARCH §2 工具超时 300s，
    /// 首装接近上限时 CLI 侧提前提示）
    fn install(&self, ctx: &InstallCtx, spec: &InstallSpec) -> InstallOutcome;
    fn probe(&self, ctx: &InstallCtx, spec: &InstallSpec) -> Option<Launch>;
    fn install_hint(&self, spec: &InstallSpec) -> String;
}
```

**错误归 RuntimeError**（ARCH §6.1 为唯一权威表；本设计 **Δ ARCH §6.1 增补** `InvalidSpec` / `AllowedHostsDenied`，实现时回写 ARCH 并记 ADR）：

```rust
pub enum RuntimeError {
    Spawn { cmd: String, cause: io::Error },
    Download { url: String, expected_sha: Option<String>, actual_sha: Option<String>, cause: String },
    MissingRuntime { what: String, install_hint: String },
    InvalidSpec { ls_id: String, reason: String },          // Δ 增补
    AllowedHostsDenied { url: String, allowed: Vec<String> }, // Δ 增补
}
```

## 4. override 优先级（ls-registry ConfigAdapter 落地）

```
1. CLI flag：--ls-path / --ls-base-cmd / --ls-args（clap global=true，单次生效）
2. 用户全局 config.toml：Windows %APPDATA%\serena\config.toml；Unix ~/.config/serena/config.toml
   [ls.<id>]
   ls_path = "/custom/clangd"     # 不存在 → MissingRuntime + hint
   ls_base_cmd = [...]
   ls_args = [...]
3. servers.toml 默认（include_str!，内置单表；外部覆盖 v1 不做）
```

**禁止**：项目目录任何文件参与 cmd 构造；环境变量 override；servers.toml 之外的配置文件。

## 5. 下载通道安全三件套（momus I-4/5/6）

1. **重定向逐跳校验**：`reqwest::redirect::Policy::custom`，每一跳 host 必须命中 `allowed_hosts`（首跳+重定向链），违者 `AllowedHostsDenied`——`--allow-unsigned-sha` 人工通道同样受此保护。
2. **zip-slip 消毒**：解压逐条目检查目标路径（canonicalize 后必须落在解压根内），拒绝含 `..`、绝对路径、盘符（`X:`）的条目 → `Download{cause:"unsafe archive entry"}`。trust boundary，不可省。
3. **锁原子获取**：对照 ARCH §2 daemon.lock 先例——陈锁判定（pid 死或 age>10min；pid 复用由 age 门兜底）后**不直接删**，走「唯一 tmp 名 `create_new` → 同卷原子 rename 到 `install.lock`」重试环；拿不到锁则等待/放弃，消除 TOCTOU 互删窗口。

## 6. CLI 子命令（新增；已核对 main.rs Cmd 枚举 23 变体无撞名）

```
serena-cli install <ls_id> [--update] [--allow-unsigned-sha] [--global-auto-install]
serena-cli install --list          # 所有 LS + 状态（installed / path / unavailable / unsigned-blocked）
serena-cli doctor                  # runtime 检测 + LS 状态 + 修复建议
serena-cli --ls-path <p> <cmd>     # 单次 override
```

持久化语义：**只有 `install --global-auto-install` 落盘改 config.toml**；`--auto-install` 类 flag 一律单次。

## 7. 任务与验收（映射声明见文首）

| Task | 内容 | code-level | end-to-end |
|---|---|---|---|
| **18**（PLAN 细化） | deps.rs 修：verify_sha256 真校验字节 + reqwest 下载流（含 §5 三件套）+ InstallSpec/InstallCtx/InstallOutcome/RuntimeError | clippy -D warnings 0；单测覆盖 sha 失配/陈锁/allowed_hosts 重定向/zip-slip | 真实下载 1 个小文件 + sha 校验通过 |
| **19**（PLAN 细化） | servers.toml schema + ConfigAdapter（ServerSpec→InstallSpec 映射 + override 优先级） | schema 反序列化测试；坏 toml → InvalidSpec | 5 个 T0 LS（bash/json/yaml/marksman/crystal）冒烟 |
| **20**（PLAN 细化） | 73 LS toml 收录（批量数据工作） | loader 全量读入 + 每条必填字段校验 | `install --list` 列 73；**覆盖率分母 = 有 URL 模板的 LS（A+B+C+D+E = 49）× 各自声明的平台对，分子 = 平台产物实际可下**，目标 ≥75% |
| **21**（PLAN 细化） | 根发现 + SKIP + install 命令 + 双路径 | 根发现单测；项目 .serena 不参与 cmd（安全测试） | A/B 双路径各 1 条真实命令；`--allow-unsigned-sha` 越狱；`--ls-path /nonexistent` → MissingRuntime |
| **29**（新） | TransportKind::Tcp（ls-runtime/process.rs）+ godot 适配 | Tcp 泵单测（本地 mock TCP LS） | **manual**：Godot 编辑器 6008 在跑 → overview 走通（活体依赖，不进 CI） |
| **30**（新） | 特殊形态：groovy / julia / msl（内嵌物化）/ scala | 各自单测 | 各 1 条端到端 |
| **31**（新） | CLI install/doctor 子命令 | clap 集成测试 | doctor 输出 runtime + LS 状态表 |

## 8. 风险

| 风险 | 缓解 |
|---|---|
| A 类 27×~4 平台 sha256 数据量大 | 首批 5 T0 + 7 T2 逐条核对真值；其余留空 → auto 拒绝并提示补 sha |
| npm/dotnet/gem Windows 权限坑 | 失败回退 install_hint |
| toml 数据漂移 | 每条注 `source_commit = "43ae0211"` |
| 系统 xz 缺失 | MissingRuntime + hint |
| 首装时长逼近工具 300s 超时 | CLI 下载进度输出 + 提示后台 `install` 预热 |

## 9. 不做

LS 自更新/自删除；system 级安装范围；multi-runtime 强探测；lsp-proxy 形态；项目级 override；环境变量 override；LS 源码编译；外部 servers.toml 覆盖（v1）。

---

## 下一步（拍板后）

1. fullstack 按 Task 18 → 19 → 20 → 21 → 29 → 30 → 31 逐个实现，每 Task 双验收
2. sha256 真值核对（首批 12 条查 upstream releases）
3. 末轮：全 workspace clippy + test + e2e-runner 独立验证；ARCH §6.1 Δ 增补回写 + ADR
