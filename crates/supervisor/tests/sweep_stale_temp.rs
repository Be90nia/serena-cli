//! serena-rust-nodd：跨 run tempdir 残留清扫。
//!
//! 历史 cargo test（崩溃 / kill / 并发 run）在 TEMP 根留下的 serena 测试目录
//! 无人收尸，一度堆到 2400+ 条。本套件每次 `cargo test -p supervisor` 启动时
//! 删除 >2h 的本套件前缀残留——age 阈值避开仍在运行的并行 run（同前缀自愈
//! scratch helper 只清自己的，清不掉别的 pid 的遗留）。

use std::time::{Duration, SystemTime};

/// supervisor 测试专用前缀（非 `serena` 开头的 scratch 命名）。
const EXTRA_PREFIXES: [&str; 5] = [
    "ct_b2_",
    "recipe_b4_",
    "recipe_4nuq_",
    "warm_bin_",
    "fs_bin_read_",
];

fn sweep_stale_test_temp() {
    let cutoff = SystemTime::now() - Duration::from_secs(2 * 3600);
    let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        // serena-powershell 是 powershell LS 的运行时日志目录（非测试产物），不碰。
        if name == "serena-powershell" {
            continue;
        }
        let ours = name.starts_with("serena") || EXTRA_PREFIXES.iter().any(|p| name.starts_with(p));
        if !ours {
            continue;
        }
        let stale = ent
            .metadata()
            .and_then(|m| m.modified())
            .map(|t| t < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = if ent.path().is_dir() {
                std::fs::remove_dir_all(ent.path())
            } else {
                std::fs::remove_file(ent.path())
            };
        }
    }
}

#[test]
fn sweep_stale_serena_tempdirs() {
    sweep_stale_test_temp();
}
