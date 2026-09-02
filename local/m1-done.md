# M1 DONE — feat(m1): daemon+cli product shell

## Git log (M1 全程 commits)
```
bd182cc feat(m1): M1 acceptance — 3 readers + 1 writer concurrency test
e642909 feat(cli): forwarding + lazy-spawn + 管理命令 status/stop-all (Task 16)
d3e61c3 feat(supervisor): write_gate + symbol-body + replace-body C3 链路 (Task 15)
f052164 feat(daemon): idle reaper + LRU + shutdown draining (Task 14)
ff91374 feat(supervisor): Task 13 - per-key load gate + key normalization + Failed 懒重启 + SupervisorTrait
2347050 feat(daemon): HTTP front + wire DTO + token middleware (Task 12)
4af7738 feat(daemon): singleton lock arbitration (C1) - lockfile.rs TDD
```

## M1 验收 (Task 17, PLAN §312)
- [x] 集成测试：3 并发读 + 1 写同文件 (`e2e_concurrency.rs`) —— 写串行、读不阻塞、最终一致 ✅
- [x] `cargo build --release` —— 单 exe 6.53 MB ✅
- [x] C2 Job Object 验收：`taskkill /F /IM cli.exe` → `tasklist | findstr clangd` 空输出 ✅
- [x] GH Actions `m0-ci.yml` (fmt + clippy + test + release build + artifact upload)
- [ ] `feat(m1): daemon+cli product shell` —— 见下

## 测试统计
- 86 passed / 0 failed（85 + 1 e2e_concurrency）
- clippy 0 error
- fmt clean

## 工具全集
M1 daemon HTTP 端点 + CLI 转发已实现：
- `overview / def / refs / symbol-body / replace-body` 5 个 LSP 工具
- `status / stop-all` 2 个管理命令
- `--daemon` 模式入口
- 默认转发模式（lazy-spawn）

## MSYS bash 测试说明
lazy-spawn 链路在 MSYS bash 下 spawn 出的 daemon 子进程会被 bash reap，
**真 Windows shell（cmd/PowerShell）下正常**。C2 验证已用真 cmd 跑通。

## 收尾动作（你来定）
PM 推荐：**手动 `git commit --allow-empty -m "feat(m1): daemon+cli product shell"` 触发 M1 DONE 门槛**
——因为代码改动已在 bd182cc 完整；M1 DONE 门槛纯文本无需新文件。

或者把 7 个 commit squash 成一个 `feat(m1): daemon+cli product shell`，
但这样会丢失中间的 dev 痕迹——不推荐。

## 下一步（M2 范畴）
1. **代码补全（completion）** —— 给 AI agent 用，省 token（用户已要求）
2. `--json` / `--record` 输出（PLAN §326）
3. 多语言适配器（pyright / gopls / typescript）
4. MCP endpoint（ARCH §3.2）
5. 缓存（M4）
