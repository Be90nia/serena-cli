# dia · 公开 API 契约入口

> 公开契约起点：所有 CLI / HTTP / wire 契约以本文件为索引，权威内容在 `ARCHITECTURE.md §6.3` 与 `docs/rpc-catalog.json`。
> 本文件**不复述权威内容**，仅指向并标注破坏性变更原则。

## 1. 契约分层

| 层 | 权威 | 角色 |
|---|---|---|
| HTTP wire（daemon ↔ cli） | [`ARCHITECTURE.md §6.3`](../../ARCHITECTURE.md) | 请求/响应包络 + 错误码枚举 + retryable + CLI exit |
| 工具目录 | [`docs/rpc-catalog.json`](../rpc-catalog.json) | CLI 子命令清单 + input/output schema |
| 双源同步 | `crates/supervisor/src/catalog.rs` 内嵌 `json!` | catalog 双源（增 execute_tool 分支必双同步） |

## 2. 契约稳定性（破坏性变更矩阵）

| 改动 | 破坏性 | 必追 |
|---|---|---|
| 错误码枚举增 | 否 | CHANGELOG.md Unreleased + ADR |
| 错误码枚举删/改 | **是** | CHANGELOG + ADR + 通知所有已知 client |
| wire 字段新增 | 否 | — |
| wire 字段删/类型变 | **是** | 同上 |
| CLI 子命令增 | 否 | CHANGELOG + README + catalog 双源 |
| CLI 子命令删 | **是** | CHANGELOG + deprecation warning 期 |
| 子命令选项删/类型变 | 否（同义改）/ **是**（语义改） | CHANGELOG |

## 3. 请求/响应包络（指针）

详见 `ARCHITECTURE.md §6.3`，要点：

- 请求体：`{ "project_root": "D:/proj", "args": { ... } }`
- 成功（HTTP 200）：`{ "ok": true, "data": "...", "format": "text", "~tokens": 17 }`（~tokens 可由 `SERENA_NO_TOKEN_ESTIMATE=1` 抑制）
- 失败（HTTP 200 携带业务失败，传输层错误才用 4xx/5xx）：
  ```json
  { "ok": false,
    "error": {
      "code": "WRITE_CONFLICT",
      "message": "file changed on disk since last sync: src/main.rs",
      "ls": "clangd",
      "retryable": false,
      "hint": "..."
    }
  }
  ```

## 4. 错误码枚举（9 个 + HTTP 保留码）

完整表见 `ARCHITECTURE.md §6.3`：

| error.code | retryable | CLI exit |
|---|---|---|
| `BAD_ARGS` | false | 2 |
| `LS_NOT_INSTALLED` | false | 1 |
| `LS_SPAWN_FAILED` | true | 5 |
| `LS_NOT_READY` | true | 5 |
| `LS_TERMINATED` | true | 5 |
| `LS_TIMEOUT` | true | 5 |
| `RPC_ERROR` | case | 1 |
| `WRITE_CONFLICT` | false | 1 |
| `INTERNAL` | false | 3 |

HTTP 保留：`404` 未知工具名、`503` daemon 关停中（CLI 自愈 ≤5s 窗口）、`426` 协议版本不匹配（待 `WIRE_PROTOCOL_VERSION` 协商机制落地，bd `serena-rust-1mnf`）。

## 5. 工具目录（catalog）

- 入口：`docs/rpc-catalog.json`（已构建产物，对外可读）
- 源：`crates/supervisor/src/catalog.rs` 内嵌 `json!`（编译期检查）
- **catalog 双源**纪律：增 `execute_tool` 分支 = 两处同步（教训：undo/redo 战役 agent 漏内嵌源，本地 dev 全绿 + CI 对账测试红）

## 6. 协议版本

- 当前：`WIRE_PROTOCOL_VERSION = 1`
- 协商机制：**未落地**（bd `serena-rust-1mnf`）—— 暂以 HTTP 426 形式预埋
- 客户端必须忽略未知字段（向后兼容）

## 7. 客户端使用要点

- **工具响应分层双轨**：`format` 字段声明 `text` / `json` / `record`，解析客户端按 `format` 路由
- **`pending` in diagnostics**：`diagnostics --wait-gen` 返回 `{items, pending}`，`pending` = 真值判断，详见 README §Reading `pending`
- **`post_write_diagnostics` snapshot**：写工具响应附带诊断快照，详见 README 同节
- **CLI 输出可能带 `[warn]` 人读行**（python json.load 直接炸——先 `find('{')` 切再 parse）

## 8. 破坏性变更通告

- 提交 = `git push` 前**必须**确认：
  - 错误码枚举删/改 → 通知所有已知 client + ADR
  - CLI 子命令删 → README deprecation note + deprecation warning 期 ≥1 minor version
  - wire 字段删/类型变 → ADR + 主版本号 bump

## 9. 修订记录

| 日期 | 摘要 |
|---|---|
| 2026-10-12 | 一次性补建（bd `serena-rust-j0hf`）：CLAUDE.md + 九件 constitution + 本文件 + 首批 7 条 ADR + plans/ 落地子计划 + CHANGELOG Unreleased 段 |