# 0012 · content_version 版本账本 + 第 11 个 wire 错误码 `STALE_READ`

- 状态：Accepted
- 日期：2026-10-26（bd `serena-rust-1282` AI 原生协同）
- 对应 ARCHITECTURE：§6.3 错误码表 / §3.4 锁清单
- 反向指针：`docs/api/contract.md` §4 + `crates/supervisor/src/version_ledger.rs` + `crates/daemon/src/dto.rs` `WireErrorCode::StaleRead` + `crates/supervisor/src/undo.rs` `recorded_write`

## 背景

三方碰撞实验实锤：两个 agent 先后对同一符号 `replace-body`，后写基于旧读静默覆盖先写（`applied:true` 无告警）——写路径没有 read-write lineage，AI 协同场景（多 agent 同仓）需要一个"我读的版本还是不是当前"的硬护栏。

已否决的形态：直播流 / 广播 / claim 锁（需要代理自觉配合或常驻订阅，成本与脆弱度都不划算）。

选型：**版本戳 + others_active 搭车字段**。账本在 daemon 侧自动维护，代理只需"读响应 → 带版本写"一条约定，无需任何协调协议。

## 决策

1. **版本账本**：`version_ledger.rs` 按 canonical file 维护 daemon 生命周期内单调递增 `content_version`（初始 0）。**校验 + bump 单点收口在 `recorded_write`**（全部写类路径的统一写盘点，恒在写门内）——校验在 WAL 记账前（门内，封死「校验过 → 等门期间他人写盘 → 覆盖」竞窗），bump 在原子写成功后。`expected_version` 经 task-local（`WRITE_CTX`，TXN_STORE 同款模式）由 execute_tool 写类分支注入。
2. **wire 契约**：读类 4 工具（symbol-body / read-file / edit-context / overview）响应附 `content_version`；写类 9 工具接受可选 `expected_version`——不符 → 新错误码 `STALE_READ`（第 11 个，枚举纯新增非破坏），wire 附 `expected_version`/`current_version`/`changed_since` 三要素 + `hint: re-read then re-apply`；未提供 → 照旧应用（零破坏），响应附当前版本。
3. **others_active 搭车字段**：活动环（5min 窗，cap 256）自动推导「同 root 其他文件的近期写活动」，附在写成功 / edit-context / overview 响应（上限 5 条，空省略）。只做可见不做拦截——版本戳已是硬护栏。
4. **边界（写入字段 doc 与 §6.3 表）**：版本作用域 = daemon 生命周期（重启归零）；外部编辑不 bump；undo 回滚内容不回退版本（单调防 ABA）；`rename-symbol`/`safe-delete-symbol` 不接受 expected_version（显式传 = BAD_ARGS，静默忽略 = 假安全）；overview 默认裸数组 wire 无处塞字段即跳过（bd kq6e wire 兼容优先，summary/delta object 形态有字段）。

## 后果

- **正向**：多 agent 同仓写碰撞从「静默覆盖」变为「STALE_READ + 一眼可执行的重放三要素」；AI 还能从 `others_active` 看见别人刚动过的文件。未提供 expected_version 的调用方行为与此前完全一致。
- **代价**：读类响应多一个 `content_version` 字段（旧客户端 serde 忽略未知字段，无害）；io 错误面不保真 STALE_READ 结构——经 uid 打标的 `STALE_HITS` 侧信道在 execute_tool 收尾还原（recorded_write 错误签名不变，调用点零改动）。
- **回滚成本**：低（枚举变体 + 两表行 + 模块；既有 wire 字节不变）。
