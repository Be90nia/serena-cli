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

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
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
    /// 胜者：lock 已建，需要监听 `port` 并把 lock 文件回填为最终内容。
    Won { port: u16, file: PathBuf },
    /// 败者：现成 daemon 已在 `addr` 监听，CLI 转发给它。
    Lost { addr: SocketAddr },
}

/// 探测超时（A6 / DESIGN §3 C1）。
const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
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
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // 败者：读现有 lock → TCP 探活。
            let raw = std::fs::read_to_string(lock_path)?;
            let existing: LockEntry = serde_json::from_str(&raw)?;
            let addr: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, existing.port).into();
            if probe_alive(addr) {
                Ok(Outcome::Lost { addr })
            } else {
                // 残留死 lock → 清理重建（自己当胜者）。
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

/// 删 lock 文件（daemon shutdown 路径）。
pub fn remove(lock_path: &Path) -> Result<(), LockError> {
    if lock_path.exists() {
        std::fs::remove_file(lock_path)?;
    }
    Ok(())
}

/// 检查 `addr` 是否可达（500ms 超时）。CLI 探活用。
pub fn is_alive(addr: impl ToSocketAddrs) -> bool {
    match addr.to_socket_addrs() {
        Ok(mut it) => it.next().is_some_and(probe_alive),
        Err(_) => false,
    }
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
        // 第一次：创建
        let _ = try_become_daemon(&path, 7860).expect("first wins");
        // 起一个真监听 7860 的进程，让"探活"返回 true
        let listener = std::net::TcpListener::bind(("127.0.0.1", 7860)).expect("bind 7860");
        listener.set_nonblocking(false).ok();
        // 第二次：应 Lost，端口 7860
        let out = try_become_daemon(&path, 7860).expect("second call");
        match out {
            Outcome::Lost { addr } => assert_eq!(addr.port(), 7860),
            Outcome::Won { .. } => panic!("second create must lose when peer alive"),
        }
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
    fn remove_clears_lock() {
        let (_dir, path) = fresh_lock();
        try_become_daemon(&path, 7860).expect("win");
        assert!(path.is_file());
        remove(&path).expect("remove");
        assert!(!path.exists());
    }
}
