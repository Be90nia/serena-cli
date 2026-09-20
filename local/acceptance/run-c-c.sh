#!/bin/bash
# C 段：c_demo（clangd）核心 10 条（daemon 模式）
set -u
cd /d/Project/serena-rust
B=./target/release/cli.exe
PROJ='D:/Project/serena-rust/fixtures/c_demo'
C="$PROJ/main.c"
LOG=local/acceptance/c-c.log.md
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"

restore() { git checkout -- fixtures/c_demo 2>/dev/null; }

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
echo "# C 段 c_demo（clangd）核心 10（daemon） $(date '+%F %T')" >>"$LOG"
restore

t overview        overview "$C"
t find-symbol     find-symbol add
t def             def "$C" 6 13
t refs            refs "$C" 1 4
t hover           hover "$C" 1 4
t diagnostics     diagnostics "$C" --wait-gen 1
t completion      completion "$C" 6 18
t format          format "$C"
t rename          rename-symbol --to adder "$C" 1 4
echo "RENAME_CHECK adder in main.c: $(grep -c adder fixtures/c_demo/main.c 2>/dev/null)" >>"$LOG"
restore
t doc-highlight   document-highlight "$C" 1 4

echo DONE >>"$LOG"
echo "log: $LOG"
