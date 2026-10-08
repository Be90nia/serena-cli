//! 托管子进程 spawn 与进程树治理（Windows Job Object / Unix 进程组）。
//!
//! ↖ mirror: ls_process.py@43ae021 `ManagedSubprocess`
//! Δ 上游以 `start_independent_lsp_process=True` 独立进程组躲 Python 崩溃连坐；
//!   本设计语义反转：持有树治理句柄保证宿主崩溃/被杀时 LS 全家陪葬，不留孤儿
//!   （ARCHITECTURE §3.2）。Windows 走 Job Object（KILL_ON_JOB_CLOSE），Unix 走
//!   进程组 + `killpg(SIGKILL)`（linux 另挂 PDEATHSIG、macos 另 fork kqueue
//!   watchdog 补父死兜底，见 `ProcessTreeGuard` 文档的取舍说明）。

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

/// 进程树治理句柄（ARCH §3.2：持有即保活，drop/kill 即灭树）。
///
/// - Windows：Job Object（KILL_ON_JOB_CLOSE）——drop 关句柄由内核终止全树；宿主
///   崩溃/被杀时句柄随进程回收，同机制兜底，无孤儿。
/// - Unix：进程组（spawn 时 `setpgid(0,0)`，pgid == 直接子进程 pid）——drop =
///   `killpg(pgid, SIGKILL)`。父死兜底**不对称**：linux 经
///   `prctl(PR_SET_PDEATHSIG, SIGKILL)` 由内核补齐（注意 PDEATHSIG 绑定的是
///   **创建线程**；tokio worker 线程与 runtime 同生命周期，线程退出≈进程退出，
///   误杀窗口可忽略）；macos 经 fork 独立 watchdog 进程 + kqueue
///   EVFILT_PROC(NOTE_EXIT|NOTE_REAP) 监听宿主补齐（见下方 macOS 小节），
///   kill 走 LS 原始 pid（LS setsid 脱组后 killpg 不可达，单 pid 不受影响）。
#[derive(Debug)]
pub enum ProcessTreeGuard {
    #[cfg(windows)]
    Job(win32job::Job),
    #[cfg(unix)]
    Group(u32),
}

impl Drop for ProcessTreeGuard {
    fn drop(&mut self) {
        // unix 编译下 Group 是唯一变体（单分支 match 穷尽，无 irrefutable 警告）；
        // windows 下 Drop 体为空——内层 Job 字段 drop 即关句柄，内核灭树。
        #[cfg(unix)]
        match self {
            ProcessTreeGuard::Group(pgid) => {
                // SAFETY: killpg 仅投递信号；pgid 是 spawn 时登记的真实进程组。
                // ESRCH（组不存在）= 目标已死，按成功处理（对齐 Job 句柄重复关闭语义）。
                unsafe {
                    libc::killpg(*pgid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }
}

// ─── macOS 父死兜底 ─────────────────────────────────────────────────
// macOS 无 PR_SET_PDEATHSIG 等价物，宿主（LS 的父）崩溃时无法靠内核带走 LS；
// 宿主死后进程内线程也随之消亡，故兜底实体必须是宿主 fork 出的独立 watchdog
// 进程：kqueue 监听其父（即宿主）的 EVFILT_PROC(NOTE_EXIT|NOTE_REAP)，触发即
// 持 LS 原始 pid 直接 SIGKILL——对齐 Job Object「宿主崩溃连坐」语义；不依赖
// 进程组，LS setsid 脱组后仍可杀。watchdog 只在宿主死后退出，永不成为宿主的
// zombie；正常关闭路径 killpg 先行，watchdog 随后触发时 kill 已死 pid 为无害
// no-op。
//
// fork-without-exec 纪律：watchdog 分支体仅 async-signal-safe 调用（close/
// kqueue/kevent/kill/_exit），事件缓冲用栈上数组零堆分配，不触碰任何加锁的
// 运行时状态（tokio/malloc/logger）；继承 fd（≥3）一并关闭，避免 watchdog
// 持管道副本延迟宿主侧 EOF。kqueue/注册失败即 _exit 放弃兜底（优雅退化，等同
// 引入前的 macOS 边界），不误杀 LS。
// Apple SDK 自 MacOSX 10.9 将 NOTE_REAP 标弃（未移除，仍投递）；父死兜底按
// 惯用式保留 NOTE_EXIT|NOTE_REAP 组合（REAP 确保触发时目标已完全消失），弃用
// 标记显式豁免于此函数。
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn register_proc_exit(kq: i32, pid: u32) -> bool {
    let mut change = libc::kevent {
        ident: pid as usize,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_RECEIPT,
        fflags: libc::NOTE_EXIT | libc::NOTE_REAP,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    let mut receipt =
        [libc::kevent { ident: 0, filter: 0, flags: 0, fflags: 0, data: 0, udata: std::ptr::null_mut() }; 1];
    // SAFETY: kq 为调用方刚创建的有效 kqueue；changelist/eventlist 均为有效
    // 栈指针，数组长度与传入计数一致；kevent 仅读写这两个栈数组。
    let n = unsafe {
        libc::kevent(kq, &mut change, 1, receipt.as_mut_ptr(), 1, std::ptr::null())
    };
    n == 1 && receipt[0].flags & libc::EV_ERROR == 0
}

/// watchdog 主体：仅在 `fork()` 出的子进程内、任何非 async-signal-safe 调用
/// 之前调用，永不返回（触发或失败路径均 `_exit`）。
#[cfg(target_os = "macos")]
fn run_parent_death_watchdog(child_pid: u32) -> ! {
    // SAFETY: fork 后子分支，此刻仅做 fd 关闭与后续 syscall；fork 副本独立，
    // 关闭继承 fd 不影响宿主。覆盖 macOS 默认 RLIMIT_NOFILE=256；更高 fd 数的
    // 极端场景退化为潜在 EOF 延迟（LS 终由 SIGKILL 兜住），无安全影响。
    unsafe {
        for fd in 3..256 {
            libc::close(fd);
        }
    }
    let kq = unsafe { libc::kqueue() };
    if kq < 0 || !register_proc_exit(kq, unsafe { libc::getppid() } as u32) {
        // kqueue/注册失败：放弃兜底（见小节注释），不误杀。
        unsafe { libc::_exit(1) };
    }
    let mut event: libc::kevent = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: kq 有效；event 为有效栈指针；NULL 超时 = 阻塞等宿主退出事件。
        let n = unsafe {
            libc::kevent(kq, std::ptr::null(), 0, &mut event, 1, std::ptr::null())
        };
        if n == 1 {
            break;
        }
        if n == -1 && unsafe { *libc::__error() } == libc::EINTR {
            continue;
        }
        unsafe { libc::_exit(1) };
    }
    // 宿主已死：持原始 pid 直接 SIGKILL（kill 单 pid，兼容 setsid 脱组场景）。
    unsafe { libc::kill(child_pid as libc::pid_t, libc::SIGKILL) };
    unsafe { libc::_exit(0) };
}

#[cfg(target_os = "macos")]
fn fork_parent_death_watchdog(child_pid: u32) {
    // SAFETY: fork 后子分支立即进入 run_parent_death_watchdog（纯
    // async-signal-safe 路径，见小节纪律）；父分支仅丢弃 pid，不做子进程
    // 簿记——watchdog 只在宿主死后退出，宿主存活期不存在可收割的 zombie。
    match unsafe { libc::fork() } {
        0 => run_parent_death_watchdog(child_pid),
        // 降级：fork 失败放弃兜底，不阻断 LS spawn；stderr 同 687g 告警风格。
        -1 => eprintln!("warn: parent-death watchdog fork failed; LS orphan reaping unavailable"),
        _ => {}
    }
}

/// 托管子进程句柄：spawn 后字段即刻可用，stdio 交泵消费（lsp-core transport）。
pub struct ChildHandle {
    pub stdin: tokio::process::ChildStdin,
    pub stdout: tokio::process::ChildStdout,
    pub stderr: tokio::process::ChildStderr,
    /// 进程树治理句柄。持有即保活；drop/kill 灭树（Windows=Job Object，Unix=killpg）。
    pub tree: Option<ProcessTreeGuard>,
    /// 直接子进程 pid（测试断言进程回收用；spawn 后理论上不为 None）。
    pub pid: Option<u32>,
    /// 保留的进程本体：调用方需要 `wait()/try_wait()` 监视伴生进程时，在把 handle
    /// 交给 `Session::start` 前用 `take_child` 取走（stdio 已拆出，wait 只等进程退，
    /// 不碰管道——安全）。不取则随 handle drop（脱离，tree 兜底灭树）。
    pub child: Option<tokio::process::Child>,
}

impl ChildHandle {
    /// 显式终止进程树：丢 guard → Windows 关 Job 句柄（内核灭树）/ Unix killpg(SIGKILL)。
    pub fn kill(&mut self) {
        self.tree.take();
    }

    /// 取走进程本体供调用方监视（伴生进程编排：vue adapter 监听伴生 TS LS 退出）。
    pub fn take_child(&mut self) -> Option<tokio::process::Child> {
        self.child.take()
    }
}

/// spawn 构造命名空间。↖ mirror: ls_process.py@43ae021 `ManagedSubprocess.start`
pub struct Child;

impl Child {
    /// 拉起托管子进程：tokio `Command` 挂平台树治理配置后 spawn，再取走 stdio。
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
        // Unix：子进程自成进程组（setpgid(0,0)，pgid==pid）——kill/drop 兜底
        // killpg 全灭，对齐 Job Object 的显式灭树路径。
        #[cfg(unix)]
        cmd.process_group(0);
        // linux：父死兜底（PR_SET_PDEATHSIG）——父进程崩溃也带走子树，补齐
        // Job Object「宿主崩溃连坐」语义；macos 等价兜底由 fork watchdog 承担
        // （spawn 尾部，见 guard 文档取舍）。
        #[cfg(target_os = "linux")]
        unsafe {
            // SAFETY: pre_exec 闭包在 exec 前的子进程上下文执行；prctl 仅设置
            // 当前进程的信号属性，无内存操作。闭包词法位于本 unsafe 块内，
            // 体内无需再包 unsafe（外层覆盖，多余包裹会触发 unused_unsafe）。
            cmd.pre_exec(|| {
                // 父线程退出 → 内核发 SIGKILL；设置失败仅放弃兜底，exec 照常。
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let spawn_err =
            |cmd: String| move |cause: std::io::Error| RuntimeError::Spawn { cmd, cause };
        let mut child = cmd.spawn().map_err(spawn_err(cmd_display.clone()))?;

        // 树治理句柄登记（句柄此刻必然有效）。
        //
        // ↖ mirror: oraios/serena PR #1918（subprocess_util.py `_get_process_descendants`
        //   + `_wait_for_processes`：psutil 快照子孙 → 逐个 wait → 超时 kill 兜底）。
        //   Windows 侧走内核 Job Object，覆盖面严格更广：
        //   ① 本 job 未设 BREAKAWAY_OK/SILENT_BREAKAWAY_OK，故 LS 自行 spawn 的子孙
        //     （jdtls 的 java、ts-server 的 node 等）创建时自动并入同一 job —— 无需快照，
        //     快照窗口期后新生的进程同样被覆盖（快照式 reap 的盲区）；
        //   ② drop/kill 关闭 job 句柄 → KILL_ON_JOB_CLOSE 由内核终止全树（含 LS 已
        //     退出但其子孙仍存活的场景 —— 上游 PR #1918 要修的正是这个泄漏）；
        //   ③ 宿主崩溃时句柄随进程关闭，同机制兜底，无孤儿。
        //   Unix 侧进程组覆盖 ①② 的直接子树（组内进程 killpg 全灭）；LS 若自行
        //   setsid 脱组则脱离 killpg 治理（现实中 LS 不这么做）；③ 由 PDEATHSIG
        //   （linux）/ kqueue watchdog（macos，持原始 pid 直杀，兼容脱组）补齐。
        #[cfg(windows)]
        let tree = {
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
            Some(ProcessTreeGuard::Job(job))
        };
        #[cfg(unix)]
        let tree = child.id().map(ProcessTreeGuard::Group);

        // macos 父死兜底：宿主（本进程）崩溃后由 kqueue watchdog 补杀 LS——
        // 对齐 Job Object ③「宿主崩溃连坐」；fork 失败降级为无兜底。
        #[cfg(target_os = "macos")]
        if let Some(pid) = child.id() {
            fork_parent_death_watchdog(pid);
        }

        Ok(ChildHandle {
            stdin: child.stdin.take().expect("stdin piped above"),
            stdout: child.stdout.take().expect("stdout piped above"),
            stderr: child.stderr.take().expect("stderr piped above"),
            tree,
            pid: child.id(),
            child: Some(child),
        })
    }
}

#[cfg(test)]
mod tests {
    // macOS：kqueue watcher 注册/触发语义单测（宿主侧逻辑；fork 端到端归真机）。
    #[cfg(target_os = "macos")]
    mod macos_parent_death {
        use super::super::register_proc_exit;

        fn temp_kqueue() -> i32 {
            // SAFETY: kqueue() 无参数；负返回值直接 panic 由用例报错。
            let kq = unsafe { libc::kqueue() };
            assert!(kq >= 0, "kqueue creation failed");
            kq
        }

        /// 注册语义：EVFILT_PROC(NOTE_EXIT|NOTE_REAP) 注册成功（EV_RECEIPT 无 EV_ERROR）。
        #[test]
        fn proc_exit_filter_registers() {
            let kq = temp_kqueue();
            assert!(register_proc_exit(kq, std::process::id()));
        }

        /// 触发语义：监听短命子进程，kill + wait（reap）后 kevent 超时窗内投递该事件。
        #[test]
        fn proc_exit_event_fires_on_child_death() {
            let mut sleeper = std::process::Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .expect("spawn /bin/sleep");
            let pid = sleeper.id();

            let kq = temp_kqueue();
            assert!(register_proc_exit(kq, pid), "register EVFILT_PROC");

            sleeper.kill().expect("kill sleep child");
            sleeper.wait().expect("reap sleep child");

            let mut event: libc::kevent = unsafe { std::mem::zeroed() };
            let timeout = libc::timespec { tv_sec: 5, tv_nsec: 0 };
            // SAFETY: kq 有效；event/timeout 为有效栈指针；kevent 至多 5s 返回。
            let n = unsafe {
                libc::kevent(kq, std::ptr::null(), 0, &mut event, 1, &timeout)
            };
            assert_eq!(
                n, 1,
                "NOTE_EXIT|NOTE_REAP event must be delivered after reap"
            );
            // libc kevent 在部分 target 是 packed：assert_eq! 会创建字段引用
            // 触发 E0793，必须按 unaligned 读拷出值再断言。
            let event_ident = unsafe { std::ptr::addr_of!(event.ident).read_unaligned() };
            assert_eq!(
                event_ident,
                pid as usize,
                "event ident must be child pid"
            );
        }
    }
}
