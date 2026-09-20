//! 托管子进程：spawn + Job Object 进程树治理 + 运行时依赖下载（deps.rs）+ 自动安装（install.rs / install_pkg.rs / install_extra.rs）。

pub mod deps;
pub mod install;
pub mod install_extra;
pub mod install_pkg;
pub mod process;
