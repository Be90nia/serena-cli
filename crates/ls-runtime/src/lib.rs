//! 托管子进程：spawn + Job Object 进程树治理 + 运行时依赖下载（deps.rs）+ 自动安装（install.rs）。

pub mod deps;
pub mod install;
pub mod process;
