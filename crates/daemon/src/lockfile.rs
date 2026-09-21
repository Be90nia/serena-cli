//! 单例 lockfile 仲裁（PLAN Task 11 / DESIGN §3 C1 / ARCHITECTURE §2 A6）。
//!
//! 用 `create_new(true)` 原子建文件解决两个 CLI 同时冷启动的竞态：
//! 胜者 spawn daemon 并在 bind 成功后回填端口；败者不 spawn，读胜者端口。
//!
//! 判活 = TCP 探活（connect 超时 500ms）+ boot_ms 比对（防 pid 复用）+ token 校验；
//! 残留无响应 → 清理重启。
//!
//! ponytail: 不引进程探活 API —— lock 内 `boot_ms` + token + TCP 三检已够（A6）。
//! 端口为可选项；先回填前 spawn 端不知自己端口，按 `127.0.0.1:0` 让 OS 分配后回写。

use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Lock 文件内容（DESIGN §3 C1）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockEntry {
    /// daemon 进程 PID（仅供 debug；判活以 TCP 探活 + boot_ms 为主）。
    pub pid: u32,
    /// daemon 监听端口（bind 成功后回填）。
    pub port: u16,
    /// daemon 启动时的 unix 毫秒时间戳（防 pid 复用覆盖 lock）。
    pub boot_ms: u128,
    /// 128-bit hex 随机 token，CLI 请求需带 `X-Serena-Token` 头校验。
    pub token: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

/// `try_become_daemon` 的结果。
#[derive(Debug)]
pub enum Outcome {
    /// 胜者：lock 已建，需要监听 `port`；token 为本次生成的鉴权令牌（不回读文件）；
    /// boot_ms 为本次 lock 的归属戳（收尾删 lock 时校验用）。
    Won {
        port: u16,
        file: PathBuf,
        token: String,
        boot_ms: u128,
    },
    /// 败者：现成 daemon 已在 `addr` 监听，CLI 转发给它。
    Lost { addr: SocketAddr },
}

/// 探测超时（A6 / DESIGN §3 C1）。
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
/// 宽限探活次数与间隔：判 stale 前的最小观察窗（≈0.6s 典型 / ≤2.1s 最坏）。
const GRACE_PROBES: u32 = 3;
const GRACE_INTERVAL: Duration = Duration::from_millis(300);
/// Token 长度（hex 字符数；128-bit → 32 hex chars）。
const TOKEN_LEN: usize = 32;

/// 内部：生成 128-bit hex token（用进程单调时钟 + pid 播种）。
fn gen_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let cnt = COUNTER.fetch_add(1, Ordering::Relaxed);
    // 拼 4 个 u64 → 32 hex chars（hash-like；非加密安全，足够本机 token 用途）
    let mut bytes = [0u8; 32];
    for (i, chunk) in [now, pid, cnt, now ^ cnt.wrapping_mul(0x9E37_79B9_7F4A_7C15)]
        .iter()
        .enumerate()
    {
        let bytes_8 = chunk.to_le_bytes();
        bytes[i * 8..(i + 1) * 8].copy_from_slice(&bytes_8);
    }
    let mut hex = String::with_capacity(TOKEN_LEN);
    for b in &bytes[..TOKEN_LEN / 2] {
        hex.push_str(&format!("{b:02x}"));
    }
    hex
}

fn boot_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 探测 TCP 端口是否有人应答（500ms 超时）。
fn probe_alive(addr: SocketAddr) -> bool {
    TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok()
}

/// 端口是否可绑定（用于败者路径"清理重建"前的端口检查）。
///
/// M1 端口是 CLI 传入的；这里只用来 verify 占位锁的回填路径。
/// M1 暂用 127.0.0.1:7860（DESIGN §3）；真 spawn 后由 bind 决定端口回填。
fn try_become_daemon_impl(lock_path: &Path, candidate_port: u16) -> Result<Outcome, LockError> {
    let pid = std::process::id();
    let boot_ms = boot_ms_now();
    let token = gen_token();

    let entry = LockEntry {
        pid,
        port: candidate_port,
        boot_ms,
        token: token.clone(),
    };
    let serialized = serde_json::to_vec_pretty(&entry)?;

    // 原子创建：create_new(true) → 已存在则返回 AlreadyExists，胜者独占。
    let create_result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(lock_path);

    match create_result {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(&serialized)?;
            Ok(Outcome::Won {
                port: candidate_port,
                file: lock_path.to_path_buf(),
                token,
                boot_ms,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // 败者：读现有 lock → 宽限探活。主人可能正在启动（lock 已建、端口未
            // bind）或在 draining 收尾（端口已关、进程未退）——单次探活失败就删
            // lock 会误判 stale，产生孤儿 daemon / token 错配（bd y2y 根因 2）。
            let raw = std::fs::read_to_string(lock_path)?;
            let existing: LockEntry = serde_json::from_str(&raw)?;
            let addr: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, existing.port).into();
            // bind-first 仲裁：走到这里时本进程已 bind 成功 candidate_port，端口上
            // 必无其他 listener。若 lock 记录的端口 == candidate_port，宽限探活的
            // connect 只会打进**自己** listener 的 accept backlog（内核握手，无需
            // 应用 accept）→ 必然假阳性 → 死 lock 永远无人接管，daemon 自杀循环。
            // 故 port 相同直接按死 lock 接管；端口不同才以 TCP 探活区分真主人生死。
            let probing_self = existing.port == candidate_port;
            if !probing_self && is_alive_graceful(existing.port) {
                Ok(Outcome::Lost { addr })
            } else {
                // 宽限后仍无响应 → 残留死 lock → 清理重建（自己当胜者）。
                let _ = std::fs::remove_file(lock_path);
                try_become_daemon_impl(lock_path, candidate_port)
            }
        }
        Err(e) => Err(LockError::Io(e)),
    }
}
/// `candidate_port` 是 daemon 期望的端口
pub fn try_become_daemon(lock_path: &Path, candidate_port: u16) -> Result<Outcome, LockError> {
    try_become_daemon_impl(lock_path, candidate_port)
}

/// 把 lock 文件回填为最终内容（bind 成功后的端口/token 已确定）。
pub fn write_final(lock_path: &Path, entry: &LockEntry) -> Result<(), LockError> {
    let serialized = serde_json::to_vec_pretty(entry)?;
    // 写临时文件 + rename 原子替换（避免读到半写状态）。
    let tmp = lock_path.with_extension("lock.tmp");
    std::fs::write(&tmp, &serialized)?;
    std::fs::rename(&tmp, lock_path)?;
    Ok(())
}

/// 读 lock 文件；不存在返回 Ok(None)。
pub fn read(lock_path: &Path) -> Result<Option<LockEntry>, LockError> {
    if !lock_path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(lock_path)?;
    let entry: LockEntry = serde_json::from_str(&raw)?;
    Ok(Some(entry))
}

/// 归属校验删除 lock：`pid` + `boot_ms` 都匹配当前 lock 内容才删；
/// lock 已易主（新 daemon 接管）或不存在时跳过，返回 `false`。
///
/// daemon shutdown 收尾必须走这里——无条件删除会把接管者的 lock 一起删掉，
/// 让新 daemon 变成"活着但没有 lock"的孤儿（bd y2y 根因 1）。
pub fn remove_owned(lock_path: &Path, pid: u32, boot_ms: u128) -> bool {
    let Ok(Some(entry)) = read(lock_path) else {
        return false;
    };
    if entry.pid != pid || entry.boot_ms != boot_ms {
        tracing::info!(
            lock_pid = entry.pid,
            own_pid = pid,
            "lock ownership mismatch; skip remove (lock was taken over)"
        );
        return false;
    }
    match std::fs::remove_file(lock_path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            tracing::warn!("remove owned lock failed: {e}");
            false
        }
    }
}

/// 宽限探活：连探 3 次（间隔 `GRACE_INTERVAL`）任一成功即活。
///
/// 覆盖两个窗口：daemon 启动中（lock 已建、bind 未完成）与 draining 收尾
/// （listener 已关、进程未退）。全部失败才允许判 stale。
pub fn is_alive_graceful(port: u16) -> bool {
    let addr: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, port).into();
    for i in 0..GRACE_PROBES {
        if probe_alive(addr) {
            return true;
        }
        if i + 1 < GRACE_PROBES {
            std::thread::sleep(GRACE_INTERVAL);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_lock() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("daemon.lock");
        (dir, path)
    }

    #[test]
    fn fresh_create_wins() {
        let (_dir, path) = fresh_lock();
        let out = try_become_daemon(&path, 7860).expect("first create wins");
        match out {
            Outcome::Won { port, .. } => assert_eq!(port, 7860),
            Outcome::Lost { .. } => panic!("first create must win, not lose"),
        }
        assert!(path.is_file(), "lock file must exist after Won");
    }

    #[test]
    fn second_create_loses_when_peer_alive() {
        let (_dir, path) = fresh_lock();
        // lock 记录 peer 在 7861（不同端口）：真实流中 candidate 端口已被自己 bind
        // 成功才会走到仲裁，Lost 只可能发生在 lock 记录端口 != candidate 时。
        let peer = LockEntry {
            pid: 999,
            port: 7861,
            boot_ms: 1,
            token: "peer".into(),
        };
        write_final(&path, &peer).expect("write peer lock");
        // 起一个真监听 7861 的进程，让"探活"返回 true
        let listener = std::net::TcpListener::bind(("127.0.0.1", 7861)).expect("bind 7861");
        listener.set_nonblocking(false).ok();
        // 应 Lost，指向 peer 端口 7861
        let out = try_become_daemon(&path, 7860).expect("second call");
        match out {
            Outcome::Lost { addr } => assert_eq!(addr.port(), 7861),
            Outcome::Won { .. } => panic!("must lose when lock peer alive on its own port"),
        }
    }

    /// 回归（bd lazy-spawn 自杀循环）：死 lock 记录的端口 == 自己刚 bind 成功的
    /// candidate 端口时，宽限探活的 connect 打进**自己** listener 的 backlog 必然
    /// "成功"——旧实现据此误判 Lost → daemon 自杀循环，lazy-spawn 全挂。
    /// 必须按死 lock 接管（Won）。
    #[test]
    fn same_port_dead_lock_is_taken_over_not_lost() {
        let (_dir, path) = fresh_lock();
        let dead = LockEntry {
            pid: 1,
            port: 7860,
            boot_ms: 1,
            token: "dead".into(),
        };
        write_final(&path, &dead).expect("write dead lock on 7860");
        // candidate == 7860 == lock 记录端口；本进程无任何真 daemon 在（测试环境
        // 该端口空闲），connect 的"成功"只可能来自自己（真实流中已 bind 的 listener）。
        let out = try_become_daemon(&path, 7860).expect("take over");
        assert!(
            matches!(out, Outcome::Won { port: 7860, .. }),
            "same-port dead lock must be taken over, not Lost; got {out:?}"
        );
    }

    #[test]
    fn stale_lock_is_recovered() {
        let (_dir, path) = fresh_lock();
        // 写一个 port=9999 的死 lock（无任何进程监听 9999）
        let dead = LockEntry {
            pid: 1,
            port: 9999,
            boot_ms: 1,
            token: "deadbeef".into(),
        };
        write_final(&path, &dead).expect("write dead lock");
        // 此时 try_become 应清掉死 lock 并自己 Win（端口取传入的 candidate）
        let out = try_become_daemon(&path, 7860).expect("recover");
        assert!(
            matches!(out, Outcome::Won { port: 7860, .. }),
            "stale lock must be recovered; got {out:?}"
        );
        // 新 lock 内容应不再是 dead token
        let fresh = read(&path).unwrap().unwrap();
        assert_ne!(fresh.token, "deadbeef", "lock content must be replaced");
    }

    #[test]
    fn lock_file_contains_valid_token() {
        let (_dir, path) = fresh_lock();
        let _ = try_become_daemon(&path, 7860).expect("win");
        let entry = read(&path).expect("read").expect("lock exists");
        assert_eq!(entry.token.len(), TOKEN_LEN, "token 长度应 = {TOKEN_LEN}");
        assert!(
            entry.token.chars().all(|c| c.is_ascii_hexdigit()),
            "token 应为 hex；got {:?}",
            entry.token
        );
        assert!(entry.boot_ms > 0, "boot_ms 必须 > 0");
        assert_eq!(entry.pid, std::process::id(), "pid 必须 = 自己 PID");
    }

    #[test]
    fn read_missing_returns_none() {
        let (_dir, path) = fresh_lock();
        assert!(read(&path).unwrap().is_none());
    }

    #[test]
    fn remove_owned_respects_ownership() {
        let (_dir, path) = fresh_lock();
        let out = try_become_daemon(&path, 7860).expect("win");
        let Outcome::Won { boot_ms, .. } = out else {
            panic!("first create must win")
        };
        assert!(path.is_file());

        // lock 已易主（他人 pid/boot_ms）→ 不删，返回 false。
        let other = LockEntry {
            pid: 999,
            port: 7860,
            boot_ms: 1,
            token: "other".into(),
        };
        write_final(&path, &other).expect("write other's lock");
        assert!(
            !remove_owned(&path, std::process::id(), boot_ms),
            "他人 lock 不得删除"
        );
        assert!(path.is_file(), "易主 lock 必须保留");

        // 归属匹配 → 删，返回 true。
        assert!(remove_owned(&path, 999, 1), "归属匹配应删除");
        assert!(!path.exists());

        // lock 不存在 → false，不报错。
        assert!(!remove_owned(&path, 999, 1));
    }

    /// bd y2y 根因 2 回归：主人正在启动（lock 已建、端口未 bind）时，
    /// 败者必须等宽限探活判活，不得删 lock 抢锁。
    #[test]
    fn grace_waits_for_slow_owner() {
        let (_dir, path) = fresh_lock();
        let slow = LockEntry {
            pid: 2,
            port: 7867,
            boot_ms: 2,
            token: "slow".into(),
        };
        write_final(&path, &slow).expect("write slow-owner lock");
        // 700ms 后主人才 bind（> 单次 probe 500ms，< 宽限窗 ~1.4s）。
        let binder = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(700));
            std::net::TcpListener::bind(("127.0.0.1", 7867)).expect("bind 7867")
        });
        let out = try_become_daemon(&path, 7860).expect("arbitrate");
        assert!(
            matches!(out, Outcome::Lost { .. }),
            "启动中的主人应判活 Lost，不得抢锁；got {out:?}"
        );
        assert!(path.is_file(), "宽限判活路径不得删主人的 lock");
        drop(binder.join().expect("binder thread"));
    }
}
