#!/bin/bash
# F 段：daemon 生命周期 start→复用→stop-all→lazy 重生（验 lock/token/竞态）
set -u
cd /d/Project/serena-rust
B=./target/release/cli.exe
PROJ='D:/Project/serena-rust/fixtures/rust_demo'
LIB="$PROJ/lib.rs"
LOCK="$LOCALAPPDATA/serena/daemon.lock"
LOG=local/acceptance/rt-f-daemon.log.md
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"

t() { # t <name> <args...>  —— 转发模式（默认走 daemon，lazy-spawn）
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

snap() { # snap <label> —— lock 快照 + 端口探活
  {
    echo "### snap[$1]"
    if [ -f "$LOCK" ]; then
      echo '```json'
      sed -E 's/"token":"(..)[^"]*"/"token":"\1…(len masked)"/' "$LOCK"
      echo '```'
    else
      echo 'lock: ABSENT'
    fi
  } >>"$LOG"
}

: >"$LOG"
echo "# F 段 daemon 生命周期 $(date '+%F %T')" >>"$LOG"

echo "==0. 基线：应无 daemon"
snap pre
t status-0 status
t stop-all-0 stop-all

echo "==1. 冷启动 lazy-spawn（overview 首调）"
t overview-cold overview "$LIB"
snap after-cold
t status-1 status

echo "==2. 热态复用（同命令二调，应显著快于冷启）"
t overview-warm overview "$LIB"
t def-warm def "$LIB" 0 7
t hover-warm hover "$LIB" 0 7

echo "==3. stop-all + 竞态观察"
t stop-all-1 stop-all
snap after-stop
t status-2 status
t overview-race overview "$LIB"
snap after-race

echo "==4. lazy 重生（新 token 验证）"
t status-3 status
t overview-regen overview "$LIB"
snap after-regen
t hover-regen hover "$LIB" 0 7

echo "==5. 收尾清场"
t stop-all-final stop-all
snap final
t status-final status

echo DONE >>"$LOG"
echo "log: $LOG"
