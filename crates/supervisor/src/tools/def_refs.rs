//! `textDocument/definition` 与 `textDocument/references` 工具（PLAN Task 10 / M0）。
//!
//! M0 实现位于 `Supervisor::tool_def` / `Supervisor::tool_refs`（lib.rs 内联）。
//! 本文件为 Task 13+ 拆分入口 —— M1 把 `tool_def` / `tool_refs` 从 lib.rs 迁到这里。
//!
//! ponytail: 不预抽 `Tool` trait —— 单 trait、单实现、两行分发，搬到 trait 后只多
//! 抽象层；M1 真有 `>3` 工具再抽。
