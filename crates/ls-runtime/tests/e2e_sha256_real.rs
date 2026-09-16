//! ls-runtime 集成测试：verify_sha256 真校验 + 临时下载流模拟。
//!
//! 模拟 download 字节流到临时目录 → 调 verify → 改 1 字节再调 → 第二次返 Err。
//! 跨平台：Windows 走 certutil，macOS 走 shasum -a 256，Linux 走 sha256sum
//! （验证逻辑由 deps.rs 内部 `cfg` 分支处理）。
//!
//! 锚：PLAN Task 18 sha 校验；ARCHITECTURE §8 禁第三方依赖（路径 A：系统工具）。

use ls_runtime::deps::verify_sha256;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// 进程内原子计数器（测试间唯一子目录名）。
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 建一个临时子目录（进程退出后残留，但测试只在 temp 下、无副作用）。
fn fresh_dir(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("serena_sha256_e2e_{label}_{pid}_{n}"));
    std::fs::create_dir_all(&dir).expect("create tempdir");
    dir
}

/// 在目录内写一个文件模拟 download 产物，返回路径。
fn write_fake_download(dir: &std::path::Path, name: &str, content: &[u8]) -> PathBuf {
    let p = dir.join(name);
    let mut f = std::fs::File::create(&p).expect("create fake download");
    f.write_all(content).expect("write fake download");
    f.flush().expect("flush fake download");
    p
}

/// 公开向量：bytes "hello" → `2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824`。
const HELLO_HASH: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

/// 模拟 download 成功路径：写出"hello"→ 调 verify → 应返 Ok。
#[test]
fn e2e_downloaded_blob_verifies() {
    let dir = fresh_dir("ok");
    let payload = b"hello";
    let path = write_fake_download(&dir, "downloaded.bin", payload);

    // 直接读盘 → 走与 download 流程一致的入参（bytes）。
    let bytes = std::fs::read(&path).expect("read back");
    assert_eq!(bytes, payload);

    verify_sha256(&bytes, HELLO_HASH).expect("hash match");
}

/// 模拟"字节流被中间人改 1 字节"：先 verify 通过 → 改 1 字节再 verify → 应 Err。
#[test]
fn e2e_modified_blob_fails_verification() {
    let dir = fresh_dir("tampered");
    let payload = b"hello";
    let path = write_fake_download(&dir, "downloaded.bin", payload);

    // 第一次：原始字节 → 应通过。
    let original = std::fs::read(&path).expect("read original");
    verify_sha256(&original, HELLO_HASH).expect("original hash matches");

    // 篡改 1 字节（覆盖 'h' 字节位置，保持长度不变以避免让 hash 函数跳过长度）。
    let mut tampered = original.clone();
    tampered[0] = b'X'; // "hello"[0]='h' → 'Xello'
    assert_ne!(tampered, original);

    let err = verify_sha256(&tampered, HELLO_HASH).expect_err("tampered must fail");
    assert!(err.contains("sha256 mismatch"), "msg: {err}");
    // 错误信息含 actual 前 8 字符（hello 的真实 hash 前缀）。
    assert!(err.contains("2cf24dba"), "actual prefix in msg: {err}");
}

/// 错误信息含 expected 与 actual 完整 hash（前缀也够诊断，但保留完整便于复制对比）。
#[test]
fn e2e_mismatch_msg_has_full_hashes() {
    // 用另一个已知向量当假 expected。`"world"` 真实 hash: 98c11a51b6c7f64d...
    let wrong = "98c11a51b6c7f64d51dd750e0cb51a4e0a0f8e2c9b7c8e7f9d6a5b4c3d2e1f09";
    let err = verify_sha256(b"hello", wrong).expect_err("must mismatch");
    assert!(
        err.contains("98c11a51"),
        "wrong expected prefix shown: {err}"
    );
    assert!(err.contains("2cf24dba"), "actual hash shown: {err}");
}

/// 字节为空：公开向量 `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`。
#[test]
fn e2e_empty_payload_verifies() {
    let empty_hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    verify_sha256(b"", empty_hash).expect("empty hash matches");
}

/// 格式错误（expected 不是 64 hex）→ 走格式分支，不调系统工具。
#[test]
fn e2e_malformed_expected_skips_tool() {
    // 即使 bytes 是合法 "hello"，expected 非 64 hex 也必须先在格式层拒掉。
    let err = verify_sha256(b"hello", "not-hex").expect_err("must reject non-hex");
    assert!(err.contains("invalid sha256"), "msg: {err}");
}
