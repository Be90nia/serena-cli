#!/usr/bin/env bash
# A0-2 中文 fixture 端到端验证：clangd overview zh_demo.cpp，看中文符号是否无乱码输出
set -euo pipefail
cd "D:/Project/serena-rust"

# 用 inline PATH 注入（避免永久改系统）
export PATH="/d/Program Files/LLVM/bin:$PATH"

# 先 build（若 release 未产出）
cargo build --release -p cli 2>&1 | tail -2

# 跑 overview zh_demo.cpp（CLI 进程同时继承 PATH，所以 which_no_unc 找得到 clangd）
./target/release/cli.exe \
    --direct \
    --project fixtures/cpp_demo \
    overview \
    zh_demo.cpp

echo "---exit=$?---"