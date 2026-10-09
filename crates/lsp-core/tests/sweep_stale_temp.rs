//! serena-rust-nodd：跨 run tempdir 残留清扫（lsp-core 侧前缀：serena-record-*
//! / serena-rec-* / serena-fc-* / serena_avw_test，全在 `serena` 前缀内）。
//! cargo test 崩溃/kill 留下的目录无人收尸；每次套件启动删 >2h 残留，
//! age 阈值避开并行 run 的在用目录。

use std::time::{Duration, SystemTime};

#[test]
fn sweep_stale_serena_tempdirs() {
    let cutoff = SystemTime::now() - Duration::from_secs(2 * 3600);
    let Ok(rd) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        // serena-powershell 是 powershell LS 的运行时日志目录（非测试产物），不碰。
        if name == "serena-powershell" || !name.starts_with("serena") {
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
