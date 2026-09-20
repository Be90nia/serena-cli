#!/bin/bash
# 七段主执行器：F(daemon 生命周期) → A2(rust 全命令) → B(ts) → C(c) → D(py) → E(go) → 清场
cd /d/Project/serena-rust
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"

echo "===== F daemon lifecycle ====="
sh local/acceptance/run-f-daemon.sh
echo "===== A2 rust full (daemon) ====="
sh local/acceptance/run-a2-rust-daemon.sh
echo "===== B typescript ====="
sh local/acceptance/run-b-ts.sh
echo "===== C c/clangd ====="
sh local/acceptance/run-c-c.sh
echo "===== D python/pyright ====="
sh local/acceptance/run-d-py.sh
echo "===== E go/gopls ====="
sh local/acceptance/run-e-go.sh

echo "===== final cleanup ====="
./target/release/cli.exe stop-all 2>&1
sleep 1
ls "$LOCALAPPDATA/serena/" 2>&1
echo MASTER_DONE
