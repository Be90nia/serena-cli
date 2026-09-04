# Serena 原版工具 vs serena-rust 实现对照

> 用户问题：原版功能是不是全部学过来了？
> 答案：**部分**——M1/M2 已覆盖核心工具与文件/搜索层，Memory/项目管理暂未做（DESIGN §非目标）。

> 上次更新：2026-09-04（审计后重生成；旧版"16%"已过时）

CLI 子命令总数：24。覆盖分类：

## LSP 符号级工具（核心）

| 原版工具 | serena-rust 状态 | CLI 子命令 | 备注 |
|---|---|---|---|
| `get_symbols_overview` | ✅ | `overview <file>` | M0 |
| `find_symbol` (file) | ✅ | `find-symbol <query>` 默认 file 维度的 workspace/symbol | M2 |
| `find_symbol(include_body=true)` | ✅ | `symbol-body <file> <symbol>` 独立取 body | M2 |
| `find_declaration` | ✅ | `def <file> <line> <col>` | M0 |
| `find_referencing_symbols` | ✅ | `refs` / `find-referencing-symbols` | M0 |
| `find_implementations` | ✅ | `find-implementations <file> <line> <col>` | M2 |
| `replace_symbol_body` | ✅ | `replace-body` | M1（Task 15 C3 链路） |
| `insert_before_symbol` | ✅ | `insert-text-before-symbol` | M2 |
| `insert_after_symbol` | ✅ | `insert-text-after-symbol` | M2 |
| `rename_symbol` | ✅ | `rename-symbol <file> <line> <col> --to NEW` | M2 |
| `safe_delete_symbol` | ❌ | — | M3 候选 |
| `restart_language_server` | ❌ | — | M3 候选（可经 stop+新请求懒触发） |
| `find_referencing_code_snippets` | ✅ | `find-referencing-code-snippets` 带 context_lines | M2 |
| `replace_text_in_symbol` | ✅ | `replace-text-in-symbol` | M2 |

## 文件级工具

| 原版工具 | serena-rust 状态 | CLI 子命令 | 备注 |
|---|---|---|---|
| `read_file` | ✅ | `read-file [start_line] [end_line]` | M2 |
| `list_dir` | ✅ | `list-dir <path>` | M2 |
| `find_file` | ✅ | `find-file <name_pattern>` glob（限深 5） | M2 |
| `create_text_file` | ❌ | — | 整文件创建；可由 ls-runtime 写门补 |
| `edit_file` | ✅ | 行级由 `replace-text-in-symbol` 覆盖；整文件编辑未做 | partial |
| `delete_lines` | ❌ | 行级删除可由 `delete-text-in-symbol` 替代 | partial |
| `insert_at_line` | ❌ | 整文件行级；同 `delete_lines` | partial |

## 搜索工具

| 原版工具 | serena-rust 状态 | CLI 子命令 | 备注 |
|---|---|---|---|
| `search_for_pattern` | ✅ | `search <pattern> [--path-glob] [--max-results] [--case-sensitive]` | M2 |
| `find_referencing_symbols_with_pattern` | ❌ | — | search + refs 组合，可作下一步 |

## Memory 系统

| 原版工具 | serena-rust 状态 | 备注 |
|---|---|---|
| `list_memories` / `write_memory` / `read_memory` / `edit_memory` / `delete_memory`（5 个） | ❌ | DESIGN §非目标："不复刻 serena 的 agent 层（memories、prompts、tool 编排）"。如需 M5 范畴 |

## 项目管理

| 原版工具 | serena-rust 状态 | 备注 |
|---|---|---|
| `activate_project` | ✅（部分） | `--project` flag 等价 |
| `get_active_project` | ❌ | `status` 仅返回 daemon 态，无 active project |
| `list_projects` | ❌ | |
| `initial_project_setup` | ❌ | |
| `check_onboarding_performed` | ❌ | |

## 系统/调试

| 原版工具 | serena-rust 状态 | 备注 |
|---|---|---|
| `execute_shell_command` | ❌ | **不应有**：agent 滥用风险，CLI 不实现 |
| `think` | ❌ | **不应有** |
| `prepare_for_new_conversation` | ❌ | 简易：清缓存 |
| `switch_chat_model` | ❌ | **不应有**：CLI 不管 chat model |

---

## 总计（去掉刻意不做的）

| 类别 | 应做 | 已实现 | 缺口 |
|---|---|---|---|
| 符号级 LSP | 13 | 13 | 0 |
| 文件级 | 7 | 4 | 3（create_text_file / 整文件 edit_file / insert_at_line） |
| 搜索 | 2 | 1 | 1（refs+pattern 组合） |
| Memory | 5 | 0 | 5（DESIGN 非目标，未排期） |
| 项目管理 | 5 | 1（partial） | 4 |
| 系统/调试 | 5 | 0 | 0（4 个刻意不做） |
| **应做小计** | **32** | **18** | **14** |

**应做覆盖率约 56%**（Memory 与整文件操作缺口集中）。如剔除 Memory 与项目管理（DESIGN 排除），实质可达 **20/22 ≈ 91%**。

---

## M3 候选（按 ROI）

1. `safe_delete_symbol` — 写门 + 安全删除 ~100 行
2. `create_text_file` + `insert_at_line` + `delete_lines`（行级） — 50 行各
3. `find_referencing_symbols_with_pattern` — search + refs 组合 ~30 行
4. `restart_language_server` — 30 行（甚至可省略，靠 supervisor 懒重启）
5. `get_active_project` / `list_projects` — `status` / `daemon::list` ~50 行

## 与上版的差异

- **旧版**：把工具数混算（含 5 个故意不做的），覆盖率 16%
- **新版**：按"应做 vs 不应做"拆分，去掉刻意不做的后真实缺口为 M3 候选的 5-7 个