//! 托管子进程 spawn 与 Windows Job Object 进程树治理。
//!
//! ↖ mirror: ls_process.py@43ae021 `ManagedSubprocess`
//! Δ 上游以 `start_independent_lsp_process=True` 独立进程组躲 Python 崩溃连坐；
//!   本设计语义反转：Job Object（KILL_ON_JOB_CLOSE）保证宿主崩溃/被杀时 LS 全家
//!   陪葬，不留孤儿（ARCHITECTURE §3.2）。Unix 等价路径 PR_SET_PDEATHSIG 后续标注。

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Stdio;

/// Windows 下隐藏子进程控制台窗口。勿用 `0x08`——那是 DETACHED_PROCESS，语义不同。
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// ls-runtime 具名错误（ARCHITECTURE §6.1 + auto-install-design §3 Δ 增补）。
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("failed to spawn `{cmd}`: {cause}")]
    Spawn {
        cmd: String,
        #[source]
        cause: std::io::Error,
    },
    /// 下载/校验/解压失败（auto-install-design §3；ARCH §6.1 Δ 增补，回写见 ADR）。
    #[error("download failed: {cause} (url={url})")]
    Download {
        url: String,
        expected_sha: Option<String>,
        actual_sha: Option<String>,
        cause: String,
    },
    /// 系统工具/runtime 缺失（如 tar/xz/unzip；auto-install-design §3）。
    #[error("missing runtime `{what}`: {install_hint}")]
    MissingRuntime { what: String, install_hint: String },
}

pub type Result<T, E = RuntimeError> = std::result::Result<T, E>;

/// LS 传输方式。TCP（Godot 6008 场景）随 servers.toml（M2）落地。
#[derive(Debug, Clone)]
pub enum TransportKind {
    Stdio,
}

/// 启动信息。
///
/// ↖ mirror: lsp_protocol_handler/server.py@43ae021 `ProcessLaunchInfo`
/// （字段按 ARCHITECTURE §4.1 定稿；ls-adapters 产出、ls-runtime 消费，故定义于本 crate）
#[derive(Debug, Clone)]
pub struct LaunchInfo {
    /// argv 列表形式，杜绝引号拼接 quirk
    pub cmd: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub transport: TransportKind,
}

/// 托管子进程句柄：spawn 后字段即刻可用，stdio 交泵消费（lsp-core transport）。
pub struct ChildHandle {
    pub stdin: tokio::process::ChildStdin,
    pub stdout: tokio::process::ChildStdout,
    pub stderr: tokio::process::ChildStderr,
    /// Windows Job Object（KILL_ON_JOB_CLOSE）。持有即保活；drop/kill 关句柄即灭树。
    pub job: Option<win32job::Job>,
    /// 直接子进程 pid（测试断言进程回收用；spawn 后理论上不为 None）。
    pub pid: Option<u32>,
    /// 保留的进程本体：调用方需要 `wait()/try_wait()` 监视伴生进程时，在把 handle
    /// 交给 `Session::start` 前用 `take_child` 取走（stdio 已拆出，wait 只等进程退，
    /// 不碰管道——安全）。不取则随 handle drop（脱离，job 兜底灭树）。
    pub child: Option<tokio::process::Child>,
}

impl ChildHandle {
    /// 显式终止进程树：丢掉 Job → 句柄关闭 → 内核按 KILL_ON_JOB_CLOSE 清场。
    pub fn kill(&mut self) {
        self.job.take();
    }

    /// 取走进程本体供调用方监视（伴生进程编排：vue adapter 监听伴生 TS LS 退出）。
    pub fn take_child(&mut self) -> Option<tokio::process::Child> {
        self.child.take()
    }
}

/// spawn 构造命名空间。↖ mirror: ls_process.py@43ae021 `ManagedSubprocess.start`
pub struct Child;

impl Child {
    /// 拉起托管子进程：tokio `Command` 挂 CREATE_NO_WINDOW 后 spawn，
    /// 进程句柄先登记进 Job Object 再取走 stdio。
    pub fn spawn(info: LaunchInfo) -> Result<ChildHandle> {
        let cmd_display = info
            .cmd
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");

        let mut cmd = tokio::process::Command::new(&info.cmd[0]);
        cmd.args(&info.cmd[1..]);
        if !info.cwd.as_os_str().is_empty() {
            cmd.current_dir(&info.cwd);
        }
        cmd.envs(info.env.iter().map(|(k, v)| (k, v)));
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let spawn_err =
            |cmd: String| move |cause: std::io::Error| RuntimeError::Spawn { cmd, cause };
        let mut child = cmd.spawn().map_err(spawn_err(cmd_display.clone()))?;

        // Job Object：先登记进程句柄再交出 stdio（句柄此刻必然有效）。
        //
        // ↖ mirror: oraios/serena PR #1918（subprocess_util.py `_get_process_descendants`
        //   + `_wait_for_processes`：psutil 快照子孙 → 逐个 wait → 超时 kill 兜底）。
        //   Windows 侧等价实现走内核 Job Object，覆盖面严格更广：
        //   ① 本 job 未设 BREAKAWAY_OK/SILENT_BREAKAWAY_OK，故 LS 自行 spawn 的子孙
        //     （jdtls 的 java、ts-server 的 node 等）创建时自动并入同一 job —— 无需快照，
        //     快照窗口期后新生的进程同样被覆盖（快照式 reap 的盲区）；
        //   ② drop/kill 关闭 job 句柄 → KILL_ON_JOB_CLOSE 由内核终止全树（含 LS 已
        //     退出但其子孙仍存活的场景 —— 上游 PR #1918 要修的正是这个泄漏）；
        //   ③ 宿主崩溃时句柄随进程关闭，同机制兜底，无孤儿。
        #[cfg(windows)]
        let job = {
            let job_err = |cmd: String| {
                move |e: win32job::JobError| RuntimeError::Spawn {
                    cmd,
                    cause: e.into(), // From<JobError> for io::Error（win32job 提供）
                }
            };
            let job = win32job::Job::create().map_err(job_err(cmd_display.clone()))?;
            let mut limit = job
                .query_extended_limit_info()
                .map_err(job_err(cmd_display.clone()))?;
            limit.limit_kill_on_job_close();
            job.set_extended_limit_info(&limit)
                .map_err(job_err(cmd_display.clone()))?;
            let handle = child
                .raw_handle()
                .expect("child just spawned; process handle alive");
            job.assign_process(handle as isize)
                .map_err(job_err(cmd_display.clone()))?;
            Some(job)
        };
        #[cfg(not(windows))]
        let job = None;

        Ok(ChildHandle {
            stdin: child.stdin.take().expect("stdin piped above"),
            stdout: child.stdout.take().expect("stdout piped above"),
            stderr: child.stderr.take().expect("stderr piped above"),
            job,
            pid: child.id(),
            child: Some(child),
        })
    }
}
