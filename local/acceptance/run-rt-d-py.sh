#!/bin/bash
# D 段：py_demo（pyright）核心 10 条（daemon 模式）
set -u
cd /d/Project/serena-rust
B=./target/release/cli.exe
PROJ='D:/Project/serena-rust/fixtures/py_demo'
PY="$PROJ/app.py"
LOG=local/acceptance/rt-d-py.log.md
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"

restore() { git checkout -- fixtures/py_demo 2>/dev/null; }

t() {
  local name="$1"; shift
  local t0 t1 rc ms
  t0=$EPOCHREALTIME
  echo "### $name" >>"$LOG"
  echo '```' >>"$LOG"
  "$B" --project "$PROJ" --json "$@" >>"$LOG" 2>&1
  rc=$?
  t1=$EPOCHREALTIME
  ms=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%d",(b-a)*1000}')
  {
    echo "rc=$rc ms=$ms"
    echo '```'
    echo
  } >>"$LOG"
  printf '%-28s rc=%s ms=%s\n' "$name" "$rc" "$ms"
}

: >"$LOG"
echo "# D 段 py_demo（pyright）核心 10（daemon） $(date '+%F %T')" >>"$LOG"
restore

t overview        overview "$PY"
t find-symbol     find-symbol add
t def             def "$PY" 4 9
t refs            refs "$PY" 0 4
t hover           hover "$PY" 0 4
t diagnostics     diagnostics "$PY" --wait-gen 1
t completion      completion "$PY" 4 13
t inlay-hint      inlay-hint "$PY" 0 6
t rename          rename-symbol --to adder "$PY" 0 4
echo "RENAME_CHECK adder in app.py: $(grep -c adder fixtures/py_demo/app.py 2>/dev/null)" >>"$LOG"
restore
t format          format "$PY"

echo DONE >>"$LOG"
echo "log: $LOG"
