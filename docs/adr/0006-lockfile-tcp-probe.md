# 0006 · lock file 判活 = TCP 探测 + boot_ms + token（不引进程探活 API）

- 状态：Accepted
- 日期：2026-09-04（C1 修复落地）/ 2026-10-12 抽 ADR（bd `serena-rust-j0hf`）
- 对应 ARCHITECTURE：§6.4 + §9 A6
- 反向指针：ARCHITECTURE.md §6.4 + §9 A6

## 背景

daemon 单例仲裁需解决两个窗口：
1. **启动中窗口**：daemon 正在 bind :7860 但还没 ready，锁已写
2. **draining 收尾窗口**：`stop-all` 后 reaper 收尾（stop_all 加端口探活见 bd `serena-rust-3ab`）

直觉方案：lockfile 含 PID + 进程探活。但 Windows / Unix 的进程探活 API 各异，跨平台路径 = 多套实现。

## 决策

lock file 判活 = **TCP 探测（500ms 超时）+ boot 时间戳 + token**，不引进程探活 API：

- **TCP 探测**：daemon bind :7860 后能接受探测 = 判活
- **boot 时间戳**：daemon 启动时写入 lockfile（`boot_ms`），客户端连接前比较 — 防止活 daemon 误判死
- **token**：daemon 启动时随机生成（`gen_token`，非加密安全有意为之）
- **唯一双实例路径**（活 daemon 被误判死）由 idle 自杀收敛（默认 15min 后自杀）
- **stop-all 残留兜底**：详见 bd `serena-rust-3ab`（锁已删、进程未死 → `netstat -ano` 按端口反查 LISTENING PID + 映像名白名单校验（非本 CLI 映像不动手）+ `taskkill /F` 强杀；Unix 走 `lsof`/`kill -9` 同构路径；反查失败不误杀，转 stderr 告警提示手动处置——`cli/main.rs` residual-reap 模块）

`gen_token` 非加密安全，有意为之：校验目标是**本机防误连 / 防跨会话误投**，不作抗恶意进程边界。

## 后果

- **正向**：最少机制覆盖崩溃残留；跨平台路径统一（TCP + std::fs）
- **代价**：极小概率的短暂双实例（活 daemon 误判死，新 daemon bind 成功；窗口 < idle 自杀时间，无正确性影响）
- **回滚成本**：低（lockfile 字段增减）

## 备选

- **进程探活 API**：Windows / Unix 各一套 — 否决
- **etcd / consul**：过重（无集中状态）

## 关联

- 教训 `d591316` 战役：stop-all 只认锁文件，锁删进程未死（lazy-spawn daemon 被 kill 脱管 / 超时截断时产生）会继续占 :7860，新 lazy-spawn bind 失败或请求打到老进程 — 详见 `bd serena-rust-3ab`
- `gen_token` 不作抗恶意边界：详见 ARCHITECTURE.md §9.1 Δ