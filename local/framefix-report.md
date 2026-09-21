# FrameFix 报告：framing 层两 finding 修复

结论：两 finding 均已修复——Content-Length 无上限（巨量分配/capacity overflow panic）与帧头 O(n²) 重扫（5MB 帧累计扫 GB 级字节饿死泵 task）。lsp-core 本 crate 34 单测 + 17 集成测试全绿；仅 docsync 2 个失败为兄弟 agent（SupFix）TTL 重构中间态，与本次改动零关联（已通知其收尾对齐断言）。

## 改动文件清单

1. `crates/lsp-core/src/framing.rs`
   - 新增 `MAX_FRAME_BODY = 64 MiB` 常量（framing.rs:18）
   - `FrameError::TooLarge(usize)` 变体（framing.rs:101-102），超限报 `Content-Length {n} exceeds the 64 MiB frame limit`
   - 自由函数 `decode()` 替换为 `Decoder` 状态机（framing.rs:125-171）：`scanned` 记已扫偏移（3 字节窗口重叠防分隔符跨 chunk 裂开），`header: Option<(header_len, frame_len)>` 定位一次缓存；body 累积期零 windows 重扫；出帧后双归零
   - 帧头解析抽 `parse_content_length()`（framing.rs:175-187），语义与原实现逐字一致（重复声明最后一条生效、BadHeader/MissingContentLength 时机不变）
   - 超限检查在 `reserve` 之前 → 拒绝路径零巨量分配；`reserve` 被 64 MiB 钳制
   - 既有测试迁移到 `Decoder`（`split_across_reads`/`decode_roundtrip`）；新增 5 个测试（含 `big_request` helper）
2. `crates/lsp-core/src/transport/stdio.rs`
   - `pump` / `record_pump` 两处 stdout 泵接入 `Decoder::default()`（stdio.rs:121/249、136/262），错误处置路径不变（`tracing::error!` + abort 泵 + `on_eof`），保序分发语义不变
3. `crates/lsp-core/tests/bin/mock_ls.rs`
   - 主循环迁移到 `Decoder`（mock_ls.rs:176/184），行为不变

## 语义保持论证（修 2）

对同样字节流，`Decoder` 与原无状态 `decode` 产出相同的帧序列与错误序列：帧头字节在 buf 中不可变（只 append/整帧 split），定位一次后缓存 content_length 与逐次重 parse 等价；BadHeader/MissingContentLength/TooLarge 均在首次定位时报出；BadJson 仍在整帧消费后报出。多帧粘包靠泵既有的 `loop { decode }` 连出，Decoder 每次 decode 至多一帧。

## 测试（TDD，全部新增）

- `oversized_content_length_rejected`：声明 ~100GB Content-Length → `Err(TooLarge(99_999_999_999))`，不 panic、不巨量 reserve、帧头未消费
- `chunked_5mb_frame_boundary_in_header`：5MB 帧切在 "Content-Le|ngth" → 分片解码正确
- `chunked_5mb_frame_boundary_mid_body`：5MB 帧三片（首片切在 `\r\n\r\n` 中间，第二片切在 body 半程）→ 正确
- `chunked_frame_boundary_after_frame_end`：完整帧 + 下一帧前 5 字节粘包 → 出帧后残余留 buf、扫描状态归零
- `scan_state_resets_after_large_frame`：5MB 帧后紧跟小帧 → 两帧依次正确解码（归零路径回归）

## 验证命令 + 关键输出

```
$ cargo test -p lsp-core

running 34 tests   （lib 单测）
test framing::tests::oversized_content_length_rejected ... ok
test framing::tests::chunked_5mb_frame_boundary_in_header ... ok
test framing::tests::chunked_5mb_frame_boundary_mid_body ... ok
test framing::tests::chunked_frame_boundary_after_frame_end ... ok
test framing::tests::scan_state_resets_after_large_frame ... ok
test framing::tests::split_across_reads ... ok
test framing::tests::decode_roundtrip ... ok
（其余 27 个既有单测全绿）
test result: ok. 34 passed; 0 failed

--test client:      test result: ok. 8 passed; 0 failed
--test record_replay: test result: ok. 2 passed; 0 failed
--test session:     test result: ok. 6 passed; 0 failed
--test transport:   test result: ok. 1 passed; 0 failed
（含 server_to_client_request_e2e_with_mock_ls → mock_ls.rs 迁移 e2e 证实）

--test docsync: 7 passed; 2 FAILED  ← 非本次改动：SupFix 的 last_released_at/TTL
  重构中间态（断言仍是"归零立即 didClose"旧语义；git status 确认 docsync.rs
  的 +100/-22 改动属其未提交工作区）。已 hub 通知其对齐断言。

$ cargo clippy -p lsp-core
（除 FILE_GUARD_TTL——同为 SupFix 中间态外，无任何告警）
```

## 未做 / 边界

- 未跑 workspace 全量测试/clippy（按约束归上层统一跑）
- 未动 supervisor/daemon、未 commit
- 错误信息字符串 "exceeds the 64 MiB frame limit" 与常量数值存在文案级重复（thiserror 属性不支持引用常量表达式，取舍：常量为逻辑事实源，文案人读）
