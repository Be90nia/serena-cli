# Serena 原版工具 vs serena-rust 实现对照

> 用户问题：原版功能是不是全部学过来了？
> 答案：**没有**——M1+M2 设计只覆盖了一部分。

---

## LSP 符号级工具（核心）

| 原版工具 | serena-rust 状态 | 备注 |
|---|---|---|
| `get_symbols_overview` | ✅ `tool_overview` | M0 完成 |
| `find_symbol` | 🟡 partial: 只有 file 维度（documentSymbol） | M1+ 设计为 workspace/symbol 兜底（PLAN §310 Task 16）但未实现 |
| `find_symbol(include_body=true)` | ❌ 缺失 | 需要 client 自己做二次 documentSymbol 拉 body |
| `find_declaration` | ✅ `tool_def` | M0 完成 |
| `find_referencing_symbols` | ✅ `tool_refs` | M0 完成 |
| `find_implementations` | ❌ 缺失 | 需要 textDocument/implementation（clangd 支持） |
| `replace_symbol_body` | ✅ `tool_replace_body` | Task 15 完成（C3 链路） |
| `insert_before_symbol` | ❌ 缺失 | 实现简单：documentSymbol 拿 range → start_byte 插入 |
| `insert_after_symbol` | ❌ 缺失 | 同上，end_byte 插入 |
| `rename_symbol` | ❌ 缺失 | workspace/executeCommand + LSP prepareRename → rename |
| `safe_delete_symbol` | ❌ 缺失 | workspace/executeCommand 或 LS 扩展 |
| `restart_language_server` | ❌ 缺失 | 简单：shutdown session → evict → 重建 |
| `find_referencing_code_snippets` | ❌ 缺失 | 与 `find_referencing_symbols` 区别在带 snippet preview |

## 文件级工具

| 原版工具 | serena-rust 状态 | 备注 |
|---|---|---|
| `read_file` | ❌ 缺失 | 直接 `tokio::fs::read_to_string` 即可 |
| `create_text_file` | ❌ 缺失 | `tokio::fs::write` + 写门 |
| `edit_file` | ❌ 缺失 | LSP textDocument/applyEdit 或 diff-based |
| `delete_lines` | ❌ 缺失 | offset 切片 + write |
| `insert_at_line` | ❌ 缺失 | 与 insert_before/after 不同 |
| `list_dir` | ❌ 缺失 | `std::fs::read_dir` |
| `find_file` | ❌ 缺失 | glob + gitignore（已有 `ignore` crate） |

## 搜索工具

| 原版工具 | serena-rust 状态 | 备注 |
|---|---|---|
| `search_for_pattern` | ❌ 缺失 | `regex` crate 已在 workspace.dependencies |
| `find_referencing_symbols_with_pattern` | ❌ 缺失 | search_for_pattern + symbol 解析组合 |

## Memory 系统

| 原版工具 | serena-rust 状态 | 备注 |
|---|---|---|
| `list_memories` | ❌ 缺失 | 跨会话持久化 metadata |
| `write_memory` | ❌ 缺失 | |
| `read_memory` | ❌ 缺失 | |
| `edit_memory` | ❌ 缺失 | |
| `delete_memory` | ❌ 缺失 | |

> Memory 系统是 serena 杀手特性——AI agent 在多轮调用间记住上下文。
> PLAN §326 没列入 M2-M4，估计要新增 M5 范畴。

## 项目管理

| 原版工具 | serena-rust 状态 | 备注 |
|---|---|---|
| `activate_project` | 🟡 部分 | `--project` flag 等价 |
| `get_active_project` | ❌ 缺失 | status 命令可补 |
| `list_projects` | ❌ 缺失 | daemon::list 之类 |
| `initial_project_setup` | ❌ 缺失 | 自动生成 `.serena/project.yml` |
| `check_onboarding_performed` | ❌ 缺失 | 首次运行引导 |

## 系统/调试

| 原版工具 | serena-rust 状态 | 备注 |
|---|---|---|
| `execute_shell_command` | ❌ 缺失 | **不应该有**——agent 滥用风险；不在 Rust 版范围 |
| `think` | ❌ 缺失 | **不应该有**——同 |
| `prepare_for_new_conversation` | ❌ 缺失 | 简易：清缓存 |
| `switch_chat_model` | ❌ 缺失 | **不应该有**——CLI 不管 chat model |

---

## 总计

| 类别 | 原版 | 已实现 | 缺口 |
|---|---|---|---|
| 符号级 LSP | 13 | 5 | **8** |
| 文件级 | 7 | 0 | **7** |
| 搜索 | 2 | 0 | **2** |
| Memory | 5 | 0 | **5** |
| 项目管理 | 5 | 0 | **5** |
| 系统/调试 | 5 | 0 | **0** (4 个刻意不做) |
| **小计（应做）** | **32** | **5** | **27** |

**serena-rust 当前实现覆盖率约 16%**。

---

## 缺口优先级（AI agent 实用性）

按 AI 调用频率排：

### 高（每次都要用）

1. **`search_for_pattern`** —— 找文本/正则，全 codebase grep 替代。**150 行 + regex crate**
2. **`find_symbol` (workspace 范围)** —— 跨文件找符号定义。当前 `tool_overview` 只看单文件。**100 行 + workspace/symbol LSP**
3. **`find_referencing_code_snippets`** —— ref 工具增强版带 snippet。**80 行**
4. **`rename_symbol`** —— 跨文件重命名。**150 行 + workspace/executeCommand**

### 中

5. **`read_file`** —— 基础读文件。**30 行**
6. **`list_dir` / `find_file`** —— 文件枚举。**50 行 + ignore crate**
7. **`find_implementations`** —— LSP 标准接口。**80 行**
8. **`insert_before_symbol` / `insert_after_symbol`** —— 复用写门。**80 行**
9. **`safe_delete_symbol`** —— 写门 + 安全删除。**100 行**

### 低

10. `restart_language_server` —— 30 行
11. `edit_file` / `create_text_file` / `delete_lines` / `insert_at_line` —— 各 50 行

### 暂缓（待定）

12. **Memory 系统**（5 工具）—— 跨会话状态。需先有缓存层。
13. 项目管理（5 工具）—— M3+ 范畴。

---

## 实施建议（与用户对齐）

按 AI 实用性 + 工作量，**M2 应实现**：
1. **search_for_pattern**（最高 ROI）
2. **find_symbol workspace 范围**（与 overview 互补）
3. **read_file**（基础）
4. **list_dir + find_file**（基础）
5. **find_implementations**（LSP 标准）
6. **rename_symbol**（杀手特性）

**M3 候选**：insert_before/after + safe_delete + restart_language_server + 文件编辑四件套

**M4+**：Memory 系统 + 项目管理

---

## 一句话总结

**当前只复刻了 16%**——核心 LSP 工具 5 个（overview/def/refs/symbol-body/replace-body）。
**还差 27 个工具**才能与原版功能对齐。优先级推荐 M2 加 6 个高频工具（搜索 + 跨文件 find + 读文件 + 文件枚举 + impl + rename），约 500-700 行代码。
