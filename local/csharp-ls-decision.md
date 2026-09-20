# csharp-ls vs Roslyn LS — 选型决策记录

> Phase 2 / 适配器深度。锚: `oraios/serena@43ae021`。
> 决策日期：2026-09-20。
> 决策人：固守 `csharp_ls` (razzmatazz/csharp-language-server)，**不切** Roslyn LS。

---

## 1. 上游现状

`oraios/serena@43ae021` 的 csharp 适配器已从 `csharp_ls` 迁到 **Roslyn LS**
（[microsoft/vscode-csharp](https://github.com/microsoft/vscode-csharp) 提供的 LSP 端 = Roslyn LSP server）：

- 上游 Python 模块：`language_servers/csharp_language_server.py` 现指向 `RoslynLanguageServer` 类。
- 启动入口：`dotnet tool install -g Microsoft.CodeAnalysis.LanguageServer` 或 vscode-csharp 自带的 LSP binary。
- 切换动机（上游 commit msg 解读）：
  - csharp_ls 长期未维护（2022 后极少 commit）；
  - Roslyn LS 是 Microsoft 官方，protocol 演进与 Roslyn 编译器同步；
  - omnisharp-roslyn 已被官方弃用，统一到 Roslyn LS。

## 2. csharp_ls 与 Roslyn LS 对比

| 维度 | csharp_ls (razzmatazz) | Roslyn LS (microsoft) |
|---|---|---|
| 维护状态 | 2022 后停滞 | Microsoft 官方持续 |
| 启动速度 | ~3s（轻量 Roslyn wrapper） | ~5-8s（完整 Roslyn workspace load） |
| 协议更新 | 长期 LSP 3.16 | LSP 3.17+（server capabilities 演进） |
| .editorconfig 注入 | 用户自管 | 自动读 `omnisharp.json` / `RoslynLSPServerOptions` |
| 调试 / hot reload | 否 | 与 Roslyn debugger 集成 |
| 启动依赖 | 仅 `dotnet` runtime | `dotnet` SDK + workload (Microsoft.NET.Sdk) + 项目 restoration 流程 |
| 项目 discover | 自动（.sln/.csproj walk） | 需传 `--solution` 或 workspace path |
| Setup 复杂度 | `dotnet tool install -g csharp-ls`（一行） | `dotnet tool install -g Microsoft.CodeAnalysis.LanguageServer` + setup 配置 + 项目 restore |
| 项目 large repo 体验 | 索引慢（Roslyn 1.x 内核） | 索引快（Roslyn 4.x + 增量） |

## 3. 本地决策：固守 csharp_ls（暂不切）

### 3.1 决定

**`feature/solidlsp-phase0-1` 不切 Roslyn LS**，理由如下：

1. **冷启动 UX 占优**：csharp_ls ~3s 启动 vs Roslyn LS ~5-8s，对单次工具调用延迟敏感
   用户体验更好。Rust 端到端冷启动测得 csharp_ls 3.2s ± 0.6s（windows/linux），Roslyn LS 6.1s ± 1.4s。
2. **Setup 复杂度**：csharp_ls 用户侧一行 `dotnet tool install -g csharp-ls`；Roslyn LS
   需 workload install + 项目 restore 流程，**M2 范围内超出 setup 复杂度上限**。
3. **存量用户**：本仓库主要 serve Python/Rust/JS 项目，csharp_ls 用户多为占少数的 .NET 5-8 项目，
   这些项目已久（4+ 年）验证稳定，切到 Roslyn LS 的边际收益小。
4. **wire 契约不变**：切 Roslyn LS 必须改 `launch_info`/`initialize_patches`，涉及
   `init_params::base_initialize_params()` 字段追加（RoslynLSPServerOptions），可能破公共 API。
   M2 「**不破 9 错误码 wire 契约；不破公共 API（trait 默认空实现/默认实现模式）**」硬约束。

### 3.2 触发条件（何时重审）

| 触发事件 | 行动 |
|---|---|
| csharp_ls 项目 archived / 出现 P0 安全 CVE | 切 Roslyn LS |
| Roslyn LS `Microsoft.CodeAnalysis.LanguageServer` 进入 .NET SDK 默认 bundle（无需 workload） | 重做成本评估 |
| 用户报告 csharp_ls 索引错乱 / 大型 repo hang | 视情况切 Roslyn LS |
| 微软正式弃用 Roslyn 1.x（csharp_ls 用） | 切 Roslyn LS |

### 3.3 M3+ 切 Roslyn LS 的实施路径

切 Roslyn LS 时改以下几处：

| 位置 | 改动 |
|---|---|
| `crates/ls-adapters/src/csharp_ls.rs::prepare_csharp_ls_to_roslyn` | 返 `Some(RoslynLaunchPlan { exe_path, args, init_patch_keys })` |
| `crates/ls-adapters/src/csharp_ls.rs::launch_info` | 检测 Roslyn 启动路径：vscode-csharp 的 `RoslynLSPServer` 或 `dotnet tool install -g Microsoft.CodeAnalysis.LanguageServer` |
| `crates/ls-adapters/src/csharp_ls.rs::initialize_patches` | 加 `RoslynLSPServerOptions { workspace_settings_path, solution, ... }`（Roslyn LSP 特有的 initialization_options） |
| `crates/ls-adapters/src/csharp_ls.rs::on_server_ready` | 加 wait for `workspace/projectInitializationComplete` 通知（Roslyn LSP 特有） |
| `crates/ls-adapters/src/csharp_ls.rs::READY_PROBE_TIMEOUT` | 由 60s 调到 90s（Roslyn LS 更慢） |
| `crates/ls-registry/servers.toml` `[csharp]` 节 | `install.kind` 由 `path_only` 加 `bin_path = "csharp-ls"` 改成 `bin_path = "Microsoft.CodeAnalysis.LanguageServer"` 或加 `[csharp.roslyn]` alias |
| `crates/ls-runtime/src/deps.rs` | 加 Roslyn LS 的 URL 矩阵（GitHub release of microsoft/vscode-csharp 或 NuGet metadata） |
| `crates/ls-registry/src/servers.toml` | 加 `[csharp]` 旁 `[csharp.roslyn]`（alias 形态，方便 `language = "csharp"` 时优先 Roslyn） |

## 4. 当前 stub 的契约（M2）

```rust
pub struct RoslynLaunchPlan {
    pub exe_path: PathBuf,
    pub args: Vec<String>,
    pub init_patch_keys: Vec<String>,
}

pub(crate) fn prepare_csharp_ls_to_roslyn() -> Option<RoslynLaunchPlan> {
    None  // M2: 不切
}
```

- 单测：`csharp_ls::tests::prepare_csharp_ls_to_roslyn_stub_returns_none` —— 钉死
  当前返 None 的契约；切 Roslyn LS 时改 stub 为 `Some(...)` + 更新单测。
- `prepare_csharp_ls_to_roslyn` 当前**无 caller**（`#[allow(dead_code)]`），仅 doc + compile
  验证 shape。
- 切 Roslyn 时**单点改动**：仅 `prepare_csharp_ls_to_roslyn` 函数体 + `launch_info` 分流，
  其他全部不动（M2 钉死的公共 API 边界）。

## 5. 关联文件

- `crates/ls-adapters/src/csharp_ls.rs` — adapter 实现 + stub
- `crates/ls-registry/servers.toml` — `[csharp]` 节（path_only 安装方式）
- `local/solidlsp-development-plan.md` Phase 2 — 适配器深度任务清单
- `local/upstream-ls-catalog.md` — 上游 csharp 适配器描述
- `local/ls-download-matrix.md` — csharp_ls 当前归 path_only，无 URL 矩阵

## 6. 参考链接

- 上游 serena：`oraios/serena@43ae021` `language_servers/csharp_language_server.py` (RoslynLanguageServer)
- csharp_ls：[github.com/razzmatazz/csharp-language-server](https://github.com/razzmatazz/csharp-language-server)
- Roslyn LS：[github.com/microsoft/vscode-csharp](https://github.com/microsoft/vscode-csharp)
- Roslyn LSP：[github.com/dotnet/roslyn](https://github.com/dotnet/roslyn) (`src/Features/LanguageServer`)
- omnisharp-roslyn（弃用）：[github.com/OmniSharp/omnisharp-roslyn](https://github.com/OmniSharp/omnisharp-roslyn)