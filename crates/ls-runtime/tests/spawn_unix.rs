//! ls-runtime unix 侧集成测试：进程组树治理（Windows 侧等价测试见 spawn.rs）。
//!
//! zombie 注：被 SIGKILL 的进程在无人 reap 时以 Z（zombie）态挂在进程表——
//! 死亡判定用 `ps -o stat=`：Z 态或查无此 pid 均算死，`kill -0` 无法区分僵尸。
#![cfg(unix)]

use ls_runtime::process::{Child, LaunchInfo, TransportKind};
use std::ffi::OsString;
use std::time::{Duration, Instant};

fn launch(prog: &str) -> LaunchInfo {
    LaunchInfo {
        cmd: vec![OsString::from(prog), OsString::from("30")],
        cwd: std::env::temp_dir(),
        env: vec![],
        transport: TransportKind::Stdio,
    }
}

/// 死亡判定：STAT 以 Z 开头（被 SIGKILL 无人 reap）或 ps 查无此 pid 均为死。
fn process_dead(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps available on unix");
    if !out.status.success() {
        return true; // 查无此 pid = 已 reap
    }
    let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
    stat.is_empty() || stat.starts_with('Z')
}

/// 轮询断言进程已死（killpg 是同步投递，留余量轮询即可）。
fn wait_dead(pid: u32, msg: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !process_dead(pid) {
        assert!(Instant::now() < deadline, "{msg}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// spawn 后 unix 树句柄 = 进程组 guard，pgid == 直接子进程 pid（setpgid(0,0)）。
#[tokio::test]
async fn spawn_sets_process_group() {
    let child = Child::spawn(launch("sleep")).unwrap();
    let pid = child.pid.expect("pid present after spawn");
    let Some(ls_runtime::process::ProcessTreeGuard::Group(pgid)) = child.tree else {
        panic!("unix tree guard must be Group(pgid)");
    };
    assert_eq!(pgid, pid, "setpgid(0,0) → pgid==pid");
}

/// kill() → guard drop → killpg(SIGKILL) → 直接子进程（组领导者）死亡。
#[tokio::test]
async fn kill_reaps_process_group() {
    let mut child = Child::spawn(launch("sleep")).unwrap();
    let pid = child.pid.unwrap();
    child.kill();
    wait_dead(pid, "kill() 后组领导者仍存活");
}

/// drop ChildHandle → guard 随之 drop → killpg 兜底灭组（对齐 windows drop_kills_process_tree）。
#[tokio::test]
async fn drop_kills_process_group() {
    let child = Child::spawn(launch("sleep")).unwrap();
    let pid = child.pid.unwrap();
    drop(child);
    wait_dead(pid, "drop 后组领导者仍存活");
}
