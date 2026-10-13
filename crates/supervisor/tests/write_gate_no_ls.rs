//! TG-H4：CI 形态（SERENA_SKIP_LS_E2E=1）下三个真 LS 并发写 e2e
//! （e2e_concurrency / e2e_write / edit_tools）全被 has_clangd() 静默 skip，
//! 写互斥并发覆盖归零。本文件把「N 写串行化 + 读写不撕裂」降到 supervisor 层
//! 无 LS 版本：`create-text-file` 是唯一核心写路径不依赖 LS 的写工具
//! （write_gate → 门内 exists 双检 → recorded_write；session_for 失败仅转
//! warning），走真实进程级写门（supervisor::write_gate），CI 可常跑。
//!
//! 对照：undo_and_write_gate_serialize 只测门原语；本测试测「工具真的持门」
//! ——删掉 tool_create_text_file 里的 write_gate::acquire 必红（门内双检失效，
//! 并发 create 穿透 exists 检查双双成功）。

use std::path::PathBuf;
use std::sync::Arc;

use supervisor::{Supervisor, ToolError};

fn scratch_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("serena-tgh4-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir scratch");
    dir
}

/// N 并发 create 同一文件：写门 + 门内双检 ⇒ 恰好一胜；盘上内容必为某一方的
/// 完整写入（不撕裂、不拼接）；并发读者的每次成功读也必须是完整 payload。
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_text_file_serializes_via_write_gate() {
    let n = 16usize;
    let root = scratch_root("create_gate");
    let file = root.join("gated.txt");
    let payloads: Vec<String> = (0..n).map(|i| format!("payload {i}\n")).collect();

    let sup = Arc::new(Supervisor::direct().await.expect("supervisor"));

    // 并发读者：写风暴期间反复读盘，任何成功读到的内容必须是完整 payload
    //（atomic_write + 写门 ⇒ 读侧只见「旧（不存在）/ 某一方完整写入」两态）。
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_file = file.clone();
    let reader_stop = Arc::clone(&stop);
    let reader_payloads = payloads.clone();
    let reader = tokio::spawn(async move {
        while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
            if let Ok(content) = std::fs::read_to_string(&reader_file) {
                assert!(
                    reader_payloads.iter().any(|p| p == &content),
                    "读侧观测到撕裂/拼接内容: {content:?}"
                );
            }
            tokio::task::yield_now().await;
        }
    });

    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let mut handles = Vec::with_capacity(n);
    for payload in &payloads {
        let sup = Arc::clone(&sup);
        let root = root.clone();
        let barrier = Arc::clone(&barrier);
        let payload = payload.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            sup.tool_create_text_file(&root, "gated.txt", &payload, None)
                .await
        }));
    }

    let mut ok = 0usize;
    let mut already_exists = 0usize;
    for h in handles {
        let res = h.await.expect("writer join");
        match res {
            Ok(_) => ok += 1,
            Err(ToolError::BadArgs { detail }) => {
                assert!(
                    detail.contains("already exists"),
                    "并发败者应报已存在，实际: {detail}"
                );
                already_exists += 1;
            }
            Err(e) => panic!("非预期错误形态: {e}"),
        }
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    reader.await.expect("reader join");

    assert_eq!(
        ok, 1,
        "写门串行化 + 门内 exists 双检：并发 create 恰好一胜（门被删/失效时穿透者 >1）"
    );
    assert_eq!(already_exists, n - 1);

    let content = std::fs::read_to_string(&file).expect("winner write");
    assert!(
        payloads.iter().any(|p| p == &content),
        "盘上必须是某一方的完整写入，实际: {content:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}
