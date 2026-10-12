# 0005 · 工具级失败走 HTTP 200 + `{ok:false}`，传输层错误才用 4xx/5xx

- 状态：Accepted
- 日期：2026-09-04（wire 契约落地）/ 2026-10-12 抽 ADR（bd `serena-rust-j0hf`）
- 对应 ARCHITECTURE：§6.3 + §9 A5
- 反向指针：ARCHITECTURE.md §6.3 + §9 A5

## 背景

传统 REST 范式：工具失败 = HTTP 4xx（如 404 / 422 / 500）。本项目 daemon HTTP 形态：
- CLI 转发路径要"按 body 分支"（retryable 重试 vs 仅 code 区分），看 HTTP 状态码二次判错易出 bug
- 纯 REST 探活（curl）也要读 body

## 决策

- **工具级失败走 HTTP 200 + `{ok:false}`**（业务失败用 body 表达）
- **传输层错误才用 4xx/5xx**：`404` 未知工具名、`503` daemon 关停中、`426` 协议版本不匹配（待 `WIRE_PROTOCOL_VERSION` 协商机制落地）
- CLI 判定单一入口：只看 body 的 `ok` / `error.code`，不看 HTTP 状态码二次判错

wire 错误码（9 个）+ retryable + CLI exit 表详见 `ARCHITECTURE.md §6.3`。

## 后果

- **正向**：CLI 分支只看 JSON 不看状态码；REST 探活仍可读 body；retryable 在 body 内表达
- **代价**：纯 REST 客户端（curl）需读 body 才知失败（轻微认知负担）
- **回滚成本**：中（HTTP 状态码 → 4xx 反向需要 wire 重新设计）

## 备选

- **REST 范式（工具失败 = 4xx）**：需双层判错，CLI bug 风险 — 否决
- **CLI 双读（body + status）**：与本选项差异重复 — 否决