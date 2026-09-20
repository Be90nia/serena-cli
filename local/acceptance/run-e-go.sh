#!/bin/bash
# E 段：go_demo（gopls）核心 10 条（daemon 模式）
set -u
cd /d/Project/serena-rust
B=./target/release/cli.exe
PROJ='D:/Project/serena-rust/fixtures/go_demo'
GO="$PROJ/main.go"
LOG=local/acceptance/e-go.log.md
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"

restore() { git checkout -- fixtures/go_demo 2>/dev/null; }

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
echo "# E 段 go_demo（gopls）核心 10（daemon） $(date '+%F %T')" >>"$LOG"
restore

t overview        overview "$GO"
t find-symbol     find-symbol add
t def             def "$GO" 9 5
t refs            refs "$GO" 5 5
t hover           hover "$GO" 5 5
t diagnostics     diagnostics "$GO" --wait-gen 1
t completion      completion "$GO" 9 12
t doc-highlight   document-highlight "$GO" 5 5
t rename          rename-symbol --to adder "$GO" 5 5
echo "RENAME_CHECK adder in main.go: $(grep -c adder fixtures/go_demo/main.go 2>/dev/null)" >>"$LOG"
restore
t format          format "$GO"

echo DONE >>"$LOG"
echo "log: $LOG"
