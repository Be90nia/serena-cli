#!/bin/bash
# runner2：分段重启 daemon 后续跑 A3 尾部 + B/C/D/E（隔离 daemon 死亡传染）
cd /d/Project/serena-rust
B=./target/release/cli.exe
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"

restart() {
  "$B" stop-all >/dev/null 2>&1
  sleep 1
  powershell -NoProfile -Command "Get-Process cli -ErrorAction SilentlyContinue | Stop-Process -Force" 2>/dev/null
  sleep 1
}

echo "===== A3 rust tail ====="
restart
sh local/acceptance/run-a3-rust-tail.sh

echo "===== B typescript ====="
restart
sh local/acceptance/run-b-ts.sh

echo "===== C c/clangd ====="
restart
sh local/acceptance/run-c-c.sh

echo "===== D python/pyright ====="
restart
sh local/acceptance/run-d-py.sh

echo "===== E go/gopls ====="
restart
sh local/acceptance/run-e-go.sh

echo "===== final cleanup ====="
restart
echo "cli processes left: $(powershell -NoProfile -Command "(Get-Process cli -ErrorAction SilentlyContinue | Measure-Object).Count" 2>/dev/null)"
echo RUNNER2_DONE
