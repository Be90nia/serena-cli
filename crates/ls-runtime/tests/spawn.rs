//! ls-runtime 集成测试：spawn 管道接线 + Job Object 进程树治理。
//!
//! ↖ mirror: ls_process.py@43ae021 `ManagedSubprocess`（Δ Job Object）

use ls_runtime::process::{Child, LaunchInfo, TransportKind};
use std::ffi::OsString;
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
    {
        let _child = Child::spawn(launch(&["ping", "-n", "30", "127.0.0.1"])).unwrap();
        assert!(ping_running(), "spawn 后 ping 进程应可见");
    } // 此处 drop

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while ping_running() {
        assert!(
            std::time::Instant::now() < deadline,
            "drop ChildHandle 后 ping 进程仍残留：Job Object 未生效"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

fn ping_running() -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq ping.exe", "/NH"])
        .output()
        .expect("tasklist 可用（Windows 验收环境）");
    String::from_utf8_lossy(&out.stdout)
        .to_lowercase()
        .contains("ping.exe")
}
