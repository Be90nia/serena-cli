#!/bin/bash
# pyright def/hover 坐标修正复测（hover 0:4 打在注释、def 4:9 打在右括号 → 改打标识符内）。
# 必须用 Git Bash 执行（cygpath 可用，导出的 PATH 才是 cli.exe 能用的 Windows 形态）。
set -u
cd /d/Project/serena-rust
B=./target/release/cli.exe
LOG=local/acceptance/adapter-fix-e2e2.log.md
LOCK="$LOCALAPPDATA/serena/daemon.lock"
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"

PROJ='D:/Project/serena-rust/fixtures/py_demo'
PY="$PROJ/app.py"

# 前置清场，保证 daemon 携带本 PATH。
"$B" stop-all >/dev/null 2>&1 || true
for i in 1 2 3 4 5; do [ ! -f "$LOCK" ] && break; sleep 1; done
rm -f "$LOCK" 2>/dev/null || true

t() {
  local name="$1"; shift
  echo "### $name" >>"$LOG"
  echo '```' >>"$LOG"
  "$B" --project "$PROJ" --json "$@" >>"$LOG" 2>&1
  echo "rc=$?" >>"$LOG"
  echo '```' >>"$LOG"
  echo >>"$LOG"
}

{
echo ""
echo "### py-hover-corrected (add 定义行 1:5, 干净 daemon)"
echo '```'
"$B" --project "$PROJ" --json hover "$PY" 1 5 2>&1
echo "rc=$?"
echo '```'
echo
echo "### py-def-corrected (调用点 5:9)"
echo '```'
"$B" --project "$PROJ" --json def "$PY" 5 9 2>&1
echo "rc=$?"
echo '```'
echo
echo "### py-diagnostics (干净 daemon 复核)"
echo '```'
"$B" --project "$PROJ" --json diagnostics "$PY" --wait-gen 1 2>&1
echo "rc=$?"
echo '```'
} >>"$LOG"

# 测后清场。
"$B" stop-all >>"$LOG" 2>&1
sleep 6
if [ -f "$LOCK" ]; then echo "LOCK_RESIDUE" >>"$LOG"; else echo "LOCK_CLEAN" >>"$LOG"; fi
echo "DONE3" >>"$LOG"
