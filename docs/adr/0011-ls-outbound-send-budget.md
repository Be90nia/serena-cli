# 0011 · LS 工具级预算覆盖出站发送段（send纳入同一计时）

- 状态：Accepted
- 日期：2026-10-12（bd `serena-rust-qq3a` perf-P1-2）
- 对应 ARCHITECTURE：§3.4 锁表（出站 mpsc cap 64）、§6.3 wire 错误码（LS_TIMEOUT）
- 反向指针：lsp-core `Client::send_once_at`、tests/client_outbound_budget.rs

## 背景

bd qq3a 拍板前提「LS 无工具级预算（仅 CLI 300s 兜底）」经代码盘点**不成立**：lsp-core `Client::request` 强制 timeout 参数，supervisor 全部工具往返已带预算（普通 30s / 索引类 120s / find-symbol warmup 15s），`CoreError::Timeout` 已映射 wire `LS_TIMEOUT` retryable（dto.rs，§6.3 既有 9 码），且与 Q2 `timeout_streak`（连续 2 次超时 evict+respawn）自愈链联动。批2-A 已把 find-symbol 的 120s 死等改为 warmup 预算——bd 记录的「find-symbol 120s 超时」实测数据属该修复之前的旧账。

真实缺口在**预算计时的边界之外**：`send_once_at` 先 `outbound.send().await` 再 `timeout(timeout, rx)`。出站 mpsc（cap 64）的 writer 卡死（LS 假死、stdin 管道满）→ 通道积压打满 → 后续所有请求在**发送段无界阻塞**——预算形同虚设，「无界排队」的最后一口。

## 决策

预算覆盖「发送 + 等待响应」全程：`outbound.send()` 包进与响应等待**共享**的同一 timeout。超时/发送失败均清理 pending 表（防 zombie oneshot），返 `CoreError::Timeout` / `Terminated`——wire 契约零新码，调用方（supervisor 工具层）零改动。

- 预算值可配置性：既有三层合并（CLI `--request-timeout` > servers.toml `timeout_ms` > 默认 30s）+ 请求参数注入（测试传小预算即可），**无需新 env**（设计约束③，宪法 §5.2 无需变更）。
- TG-M4：`FORWARD_TIMEOUT`（CLI 转发硬顶 300s）不涉及测试注入场景（预算注入走 request 参数 = builder 化既有形态），不改动。
- 回归锁：`crates/lsp-core/tests/client_outbound_budget.rs`——cap=1 通道占满 + receiver 不消费，request 必须在预算内返 `Timeout`（修复前无界阻塞被外层兜底打断 = 红）。

## 后果

- **正向**：LS writer 卡死场景下，请求在预算内拿结构化 retryable 错误（重试触发 timeout_streak 自愈链），不再无限排队占住 AI 会话。
- **代价**：发送段慢时吃掉部分响应等待预算（总预算语义 = 往返全程，符合「工具级预算」本意）；积压丢弃的 send future 消息未入队（与既有 send 失败路径同语义）。
- **回滚成本**：低（单函数内恢复 send/timeout 拆分）。

## 备选

- **per-(root,lang) 实例池**：解决的是「A 会话占 LS、B 排队」的另一维度，改动面大，明确列非目标（bd qq3a 下批）。
- **load_gate 加超时**（冷启动去重门无预算）：上界 = 持门者握手预算（30s env 可调），弱无界；不在「LS 往返」面内，本轮不动，登记观察。

## 关联

- ADR-0004（全局写门 Mutex）：write_gate 60s 超时同哲学（「早失败好过 300s 后失败」），`SERENA_WRITE_GATE_TIMEOUT_SECS` 既有。
- Q2（timeout_streak evict+respawn）：本预算触发 Timeout 后自然进入该自愈链（连续 2 次 → evict），是有意设计非回归。
