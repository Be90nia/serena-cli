//! ls-runtime 集成测试：spawn 管道接线 + Job Object 进程树治理。
//!
//! ↖ mirror: ls_process.py@43ae021 `ManagedSubprocess`（Δ Job Object）
//!
//! 进程树断言一律按 pid 精确过滤（tasklist /FI "PID eq"）：并行测试可能各自
//! spawn 同名进程，IMAGENAME 级过滤会互相误判。

use ls_runtime::process::{Child, LaunchInfo, TransportKind};
use std::ffi::OsString;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;

fn launch(cmd: &[&str]) -> LaunchInfo {
    LaunchInfo {
        cmd: cmd.iter().map(OsString::from).collect(),
        cwd: std::env::temp_dir(),
        env: vec![],
        transport: TransportKind::Stdio,
    }
}

/// spawn 后 stdin/stdout/stderr 三根管道可用：stdout 收 echo，stderr 收 `1>&2` 重定向。
#[tokio::test]
async fn spawn_wires_stdio_pipes() {
    let mut child = Child::spawn(launch(&["cmd", "/C", "echo hello & echo err 1>&2"])).unwrap();

    let mut out = String::new();
    child.stdout.read_to_string(&mut out).await.unwrap();
    assert!(
        out.contains("hello"),
        "stdout 应收到 echo 输出，实际: {out:?}"
    );

    let mut err = String::new();
    child.stderr.read_to_string(&mut err).await.unwrap();
    assert!(
        err.contains("err"),
        "stderr 应收到重定向输出，实际: {err:?}"
    );
}

/// drop ChildHandle → Job 句柄关闭 → KILL_ON_JOB_CLOSE 灭掉整棵进程树，无孤儿残留。
#[tokio::test]
async fn drop_kills_process_tree() {
    let pid = {
        let _child = Child::spawn(launch(&["ping", "-n", "30", "127.0.0.1"])).unwrap();
        let pid = _child.pid.expect("spawn 后 pid 应可用");
        assert!(pid_running(pid), "spawn 后子进程应存活, pid={pid}");
        pid
    }; // 此处 drop

    wait_gone(pid, "drop ChildHandle 后子进程仍残留：Job Object 未生效");
}

/// ↖ mirror: oraios/serena PR #1918（reap language server descendants）。
/// LS 自身 spawn 的子孙进程（模拟：cmd 直接子进程 → ping 孙子进程）必须随 job
/// 句柄关闭一并被 reap —— 子孙创建时自动并入 job（未开 breakaway），无需快照枚举。
#[tokio::test]
async fn descendants_reaped_on_kill() {
    let mut child = Child::spawn(launch(&["cmd", "/C", "ping -n 30 127.0.0.1"])).unwrap();
    let cmd_pid = child.pid.expect("spawn 后 pid 应可用");

    // 轮询等 cmd 把 ping spawn 出来（孙子进程）。
    let deadline = Instant::now() + Duration::from_secs(10);
    let ping_pid = loop {
        let kids = child_pids(cmd_pid);
        if let Some(&k) = kids.first() {
            break k;
        }
        assert!(
            Instant::now() < deadline,
            "cmd 未按预期 spawn ping 孙子进程, cmd_pid={cmd_pid}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    child.kill();

    wait_gone(cmd_pid, "kill 后直接子进程 cmd 仍残留");
    wait_gone(ping_pid, "kill 后孙子进程 ping 仍残留：job 未覆盖 LS 自 spawn 的子孙");
}

fn pid_running(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .expect("tasklist 可用（Windows 验收环境）");
    String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
}

/// 枚举 pid 的直接子进程（PowerShell CIM；测试环境自带）。
fn child_pids(pid: u32) -> Vec<u32> {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "(Get-CimInstance Win32_Process -Filter 'ParentProcessId={pid}').ProcessId"
            ),
        ])
        .output()
        .expect("powershell 可用（Windows 验收环境）");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|s| s.parse::<u32>().ok())
        .collect()
}

/// 轮询断言进程已退出（内核 TerminateJobObject 是同步的，留余量轮询即可）。
fn wait_gone(pid: u32, msg: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_running(pid) {
        assert!(
            Instant::now() < deadline,
            "{msg} (pid={pid})"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
