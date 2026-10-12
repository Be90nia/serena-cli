# 0008 · 第 10 个 wire 错误码 `DAEMON_DRAINING` 登记（枚举化，b 路）

- 状态：Accepted
- 日期：2026-10-12（bd `serena-rust-xvs7` A-3，宪法审计 P1 批）
- 对应 ARCHITECTURE：§6.3 错误码表
- 反向指针：`docs/api/contract.md` §4 + `crates/daemon/src/dto.rs` `WireErrorCode` + `crates/daemon/src/http.rs` `draining_response`

## 背景

draining 503 响应 body 的 `error.code` 自 g0m（客户端自愈）起一直硬编码字符串 `"DAEMON_DRAINING"`（`http.rs` `draining_response`），不在 `WireErrorCode` 枚举里——wire 错误码权威表（ARCH §6.3）与枚举都停在 9 码，与线上实际 wire 形态漂移（宪法审计 A-3：公开契约「文档承诺 9 码、实际 10 码」）。

选型三路：

- **a 路**：枚举加变体 + wire v2 bump（破坏性，通知所有 client）
- **b 路**：枚举加变体 + §6.3/contract.md 登记为第 10 码（非破坏性）
- **c 路**：删 body `code` 字段只留 HTTP 503（客户端失去按码编程能力，g0m 自愈判据退化）

## 决策

**b 路**。`WireErrorCode::DaemonDraining` 入枚举（serde `SCREAMING_SNAKE_CASE` → `"DAEMON_DRAINING"`，与既有线上字节逐位一致，零破坏）；§6.3 + contract.md 登记为第 10 码；`draining_response` 改用枚举（code 与 retryable 都从枚举派生）。

- **语义**：daemon 正处 `ShutdownDraining`（stop-all / 全局空闲自杀的收尾窗口），拒绝新请求。body `retryable: false`；CLI 客户端对 `503 + 此码` 做 ≤5s 退避自愈（bd serena-rust-g0m），超窗原样报错 rc=3 —— CLI exit 列 = 3 与该行为对齐。
- **不 bump wire 版本**：变体纯新增且序列化字节与既有线上一致，按 contract.md §2 破坏性矩阵「错误码枚举增 = 非破坏」，必追项 = CHANGELOG Unreleased + 本 ADR。
- **对账测试**：dto 映射单测锁定 wire 形态（序列化串 / roundtrip / retryable=false / exit=3），防再次漂移。

## 后果

- **正向**：wire 码集合 = 枚举变体 = §6.3 表行数 = CLI exit 表覆盖，四方对账闭合；daemon 侧不再有裸字符串 wire 码。
- **代价**：cli 侧 `is_daemon_draining` 维持字符串比较（客户端判据；枚举常量共享留给后续任务，非本单范围）。
- **回滚成本**：低（删变体 + 两表行，序列化字节不变故无 client 兼容面）。
