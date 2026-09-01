//! Tool semantics layer (PLAN Task 10 / ARCH §3.3).
//!
//! M0 工具实现直接挂在 `Supervisor` 上的 `tool_overview` / `tool_def` / `tool_refs`
//! 方法（M0 单文件易读）；本目录为 Task 13+ 拆分入口 —— M1 把工具实现迁出 lib.rs，
//! `tools::find_symbol` / `symbol_body` / `replace_body` 等遵循"小实现就近"原则就近放。
//!
//! 当前文件仅 re-export 三个核心工具方法所属 trait 标记 —— 不引入新抽象。
