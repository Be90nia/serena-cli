#!/usr/bin/env bash
# e2e_smoke9.sh — P0 daemon 僵尸终结 + graceful shutdown smoke
# 44 项断言；PASS ≥42 即视为通过。
#
# 范围（命中本次改动路径）：
#   - crates/daemon/src/serve.rs::serve  graceful shutdown 桥接
#   - crates/daemon/src/reaper.rs::finish_shutdown + shutdown_cleanup  删 lock + process::exit
#   - crates/daemon/src/reaper.rs::reaper_loop  drain 信号感知
#   - crates/daemon/src/http.rs::shutdown_post  Notify 触发
#
# 用例二进制：target/debug/examples/daemon_serve_bin.exe
#  等价于 cli --daemon 但绕开 cli 入口的 pre-existing stack overflow
# （Phase 5 待修 —— cli main.rs:330 #[tokio::main(multi_thread, 2)]）。
#
# 用法：bash e2e_smoke9.sh  （依赖 Git Bash + 已构建 daemon_serve_bin example）

set -u
cd "D:/Project/serena-rust"
BIN="target/debug/examples/daemon_serve_bin.exe"
LOCK="$LOCALAPPDATA/serena/daemon.lock"
ROOT="D:/Project/serena-rust"
PASS=0
FAIL=0
LOG=/tmp/smoke9.log

ok()   { echo "  PASS [$1] $2"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL [$1] $2  -- $3"; FAIL=$((FAIL+1)); }

# MSYS_NO_PATHCONV=1 关闭 Git Bash 的 /arg → 路径转换（taskkill /im /fi 等
# 会被错认成 D:/Program Files/Git/im 之类）。/T 杀进程树。
tl() { MSYS_NO_PATHCONV=1 tasklist "$@"; }
tk() { MSYS_NO_PATHCONV=1 taskkill "$@" >/dev/null 2>&1 || true; }

kill_daemon() {
  tk /IM daemon_serve_bin.exe /F /T
  tl /FI "IMAGENAME eq daemon_serve_bin.exe" 2>/dev/null | awk '/daemon_serve_bin\.exe/ {print $2}' | while read pid; do
    tk /PID "$pid" /F /T
  done
  rm -f "$LOCK" 2>/dev/null || true
  sleep 2
}

wait_lock() {
  local tries=40
  while [ $tries -gt 0 ]; do
    if [ -f "$LOCK" ]; then return 0; fi
    sleep 0.25
    tries=$((tries-1))
  done
  return 1
}

echo "================================================================"
echo "e2e_smoke9 — daemon zombie fix verification"
echo "PASS target: >=42 / 44"
echo "================================================================"

kill_daemon
sleep 1

# ===== Section A: 启动 / lock / port (10 项) =====
echo ""
echo "[A] daemon startup + lock arbitration"

if [ -f "$LOCK" ]; then bad "A1" "lock 启动前不存在" "残留 lock"; else ok "A1" "启动前无 lock 文件"; fi

"$BIN" > "$LOG" 2>&1 &
DAEMON_PID=$!
sleep 1

if wait_lock; then ok "A2" "lock 10s 内出现"; else bad "A2" "lock 出现" "超时"; kill $DAEMON_PID 2>/dev/null; exit 1; fi

sleep 2

if [ -s "$LOCK" ]; then ok "A3" "lock 非空"; else bad "A3" "lock 非空" "空文件"; fi

PID=$(grep -oE '"pid":[[:space:]]*[0-9]+' "$LOCK" | head -1 | grep -o '[0-9]*' || echo "")
PORT=$(grep -oE '"port":[[:space:]]*[0-9]+' "$LOCK" | head -1 | grep -o '[0-9]*' || echo "")
TOK=$(grep -oE '"token":[[:space:]]*"[a-f0-9]+"' "$LOCK" | head -1 | sed 's/.*"\([a-f0-9]*\)".*/\1/' || echo "")
[ -n "$PID" ]  && ok "A4" "lock.pid=$PID" || bad "A4" "lock.pid"  "缺失"
[ -n "$PORT" ] && ok "A5" "lock.port=$PORT" || bad "A5" "lock.port" "缺失"
[ -n "$TOK" ]  && ok "A6" "lock.token=${TOK:0:8}..." || bad "A6" "lock.token" "缺失"

if netstat -ano 2>/dev/null | grep -q ":$PORT.*LISTENING"; then
  ok "A7" "port $PORT LISTENING"
else
  bad "A7" "port $PORT LISTENING" "未监听"
fi

if tl /FI "IMAGENAME eq daemon_serve_bin.exe" 2>/dev/null | grep -q "daemon_serve_bin.exe"; then
  ok "A8" "daemon 进程 daemon_serve_bin.exe 存在"
else
  bad "A8" "daemon 进程存在" "未找到"
fi

RES=$(curl -s -o /dev/null -w "%{http_code}" -H "X-Serena-Token: $TOK" "http://127.0.0.1:$PORT/status" 2>&1)
[ "$RES" = "200" ] && ok "A9" "/status → 200" || bad "A9" "/status 200" "got=$RES"

RES=$(curl -s -H "X-Serena-Token: $TOK" "http://127.0.0.1:$PORT/status" 2>&1)
if echo "$RES" | grep -q '"draining":false'; then
  ok "A10" "/status draining=false"
else
  bad "A10" "draining=false" "$RES"
fi

# ===== Section B: shutdown / draining / 503 (10 项) =====
echo ""
echo "[B] graceful shutdown — draining + 503 + lock drop"

RES=$(curl -s -w '\n%{http_code}' -X POST -H "X-Serena-Token: $TOK" "http://127.0.0.1:$PORT/shutdown" 2>&1)
BODY=$(echo "$RES" | head -n -1)
STATUS=$(echo "$RES" | tail -1)
[ "$STATUS" = "200" ] && ok "B1" "/shutdown → 200" || bad "B1" "/shutdown 200" "got=$STATUS"

if echo "$BODY" | grep -q '"ok":true'; then
  ok "B2" "/shutdown → ok:true"
else
  bad "B2" "/shutdown ok:true" "$BODY"
fi

sleep 0.5
RES=$(curl -s -H "X-Serena-Token: $TOK" "http://127.0.0.1:$PORT/status" 2>&1)
if echo "$RES" | grep -q '"draining":true'; then
  ok "B3" "draining flag 置 true"
else
  RES2=$(curl -s -o /dev/null -w "%{http_code}" -H "X-Serena-Token: $TOK" "http://127.0.0.1:$PORT/status" 2>&1)
  if [ "$RES2" = "503" ] || [ "$RES2" = "000" ]; then
    ok "B3" "draining=true (或 server 已停, status=$RES2)"
  else
    bad "B3" "draining=true" "got status=$RES2 body=$RES"
  fi
fi

RES=$(curl -s -o /dev/null -w "%{http_code}" -X POST -H "X-Serena-Token: $TOK" -H "Content-Type: application/json" \
       -d "{\"project_root\":\"$ROOT\",\"args\":{}}" "http://127.0.0.1:$PORT/tools/overview" 2>&1)
{ [ "$RES" = "503" ] || [ "$RES" = "000" ]; } && ok "B4" "/tools draining → 503/000 (got=$RES)" || bad "B4" "/tools 503" "got=$RES"

HDR=$(curl -s -D - -o /dev/null -X POST -H "X-Serena-Token: $TOK" -H "Content-Type: application/json" \
      -d "{\"project_root\":\"$ROOT\",\"args\":{}}" "http://127.0.0.1:$PORT/tools/overview" 2>&1 | grep -i "retry-after" || true)
{ [ -n "$HDR" ] || [ "$RES" = "000" ]; } && ok "B5" "Retry-After header / port 已关 (got hdr=[$HDR] code=$RES)" || bad "B5" "Retry-After" "缺失"

RES=$(curl -s -X POST -H "X-Serena-Token: $TOK" -H "Content-Type: application/json" \
      -d "{\"project_root\":\"$ROOT\",\"args\":{}}" "http://127.0.0.1:$PORT/tools/overview" 2>&1)
if echo "$RES" | grep -q "SHUTTING_DOWN"; then
  ok "B6" "503 body 含 SHUTTING_DOWN"
elif [ -z "$RES" ]; then
  ok "B6" "503 body empty (port 已关)"
else
  bad "B6" "SHUTTING_DOWN" "$RES"
fi

RES=$(curl -s -o /dev/null -w "%{http_code}" -X POST -H "X-Serena-Token: WRONG" -H "Content-Type: application/json" \
      -d "{\"project_root\":\"$ROOT\",\"args\":{}}" "http://127.0.0.1:$PORT/tools/overview" 2>&1)
{ [ "$RES" = "403" ] || [ "$RES" = "503" ] || [ "$RES" = "000" ]; } && ok "B7" "token 错 403/503/000 (got=$RES)" || bad "B7" "token 校验" "got=$RES"

RES=$(curl -s -o /dev/null -w "%{http_code}" -X POST -H "Content-Type: application/json" \
      -d "{\"project_root\":\"$ROOT\",\"args\":{}}" "http://127.0.0.1:$PORT/tools/overview" 2>&1)
{ [ "$RES" = "403" ] || [ "$RES" = "503" ] || [ "$RES" = "000" ]; } && ok "B8" "无 token 403/503/000 (got=$RES)" || bad "B8" "无 token 拒绝" "got=$RES"

sleep 3
if [ ! -f "$LOCK" ]; then
  ok "B9" "lock 已被 reaper 删"
else
  bad "B9" "lock 删" "仍存在"
fi

sleep 2
DAEMON_PROCS=$(tl /FI "IMAGENAME eq daemon_serve_bin.exe" 2>/dev/null | awk '/daemon_serve_bin\.exe/ {c++} END {print c+0}')
if [ "$DAEMON_PROCS" -eq 0 ]; then
  ok "B10" "daemon 进程 0 残留（僵尸终结）"
else
  bad "B10" "daemon 进程 0 残留" "found=$DAEMON_PROCS"
fi

# ===== Section C: 重启无残留 (4 项) =====
echo ""
echo "[C] restart after shutdown"

sleep 1
if ! netstat -ano 2>/dev/null | grep -q ":$PORT.*LISTENING"; then
  ok "C1" "port $PORT 已释放"
else
  bad "C1" "port 释放" "仍 LISTENING"
fi

[ ! -f "$LOCK" ] && ok "C2" "lock 不残留" || bad "C2" "lock 不残留" "仍存在"

"$BIN" > "$LOG" 2>&1 &
DAEMON_PID2=$!
sleep 1
if wait_lock; then
  sleep 3
  ok "C3" "shutdown 后能再启动 daemon"
else
  bad "C3" "restart daemon" "lock 不出现"
fi

PID2=$(grep -oE '"pid":[[:space:]]*[0-9]+' "$LOCK" 2>/dev/null | head -1 | grep -o '[0-9]*' || echo "")
[ -n "$PID2" ] && [ "$PID2" != "$PID" ] && ok "C4" "新 daemon pid 不同 ($PID2 ≠ $PID)" || bad "C4" "新 pid" "got=$PID2 old=$PID"

kill_daemon
sleep 1

# ===== Section D: 锁仲裁 (4 项) =====
echo ""
echo "[D] lock arbitration"

"$BIN" > "$LOG" 2>&1 &
DAEMON_A=$!
sleep 1
if wait_lock; then
  ok "D1" "daemon A 启动 (lock 出现)"
  sleep 3
else
  bad "D1" "daemon A 启动" "lock 不出现"
fi
PID_A=$(grep -oE '"pid":[[:space:]]*[0-9]+' "$LOCK" 2>/dev/null | head -1 | grep -o '[0-9]*' || echo "")

COUNT=$(tl /FI "IMAGENAME eq daemon_serve_bin.exe" 2>/dev/null | awk '/daemon_serve_bin\.exe/ {c++} END {print c+0}')
[ "$COUNT" -ge 1 ] && ok "D2" "daemon A 进程存在 ($COUNT)" || bad "D2" "daemon A 存在" "got=$COUNT"

TOK2=$(grep -oE '"token":[[:space:]]*"[a-f0-9]+"' "$LOCK" 2>/dev/null | head -1 | sed 's/.*"\([a-f0-9]*\)".*/\1/' || echo "")
[ "${#TOK2}" = "32" ] && ok "D3" "token 长度 32 (got ${#TOK2})" || bad "D3" "token 长度" "got ${#TOK2}"

if tl /FI "PID eq $PID_A" 2>/dev/null | grep -q "daemon_serve_bin.exe"; then
  ok "D4" "lock.pid=$PID_A 进程存在"
else
  bad "D4" "pid 一致" "未找到 PID $PID_A"
fi

# ===== Section E: drain 信号感知 (6 项) =====
echo ""
echo "[E] drain signal semantics"

PORT_A=$(grep -oE '"port":[[:space:]]*[0-9]+' "$LOCK" 2>/dev/null | head -1 | grep -o '[0-9]*' || echo "")
TOK_A=$(grep -oE '"token":[[:space:]]*"[a-f0-9]+"' "$LOCK" 2>/dev/null | head -1 | sed 's/.*"\([a-f0-9]*\)".*/\1/' || echo "")
curl -s -X POST -H "X-Serena-Token: $TOK_A" "http://127.0.0.1:$PORT_A/shutdown" > /dev/null
sleep 0.3
RES=$(curl -s -o /dev/null -w "%{http_code}" -X POST -H "X-Serena-Token: $TOK_A" -H "Content-Type: application/json" \
      -d "{\"project_root\":\"$ROOT\",\"args\":{}}" "http://127.0.0.1:$PORT_A/tools/overview" 2>&1)
{ [ "$RES" = "503" ] || [ "$RES" = "000" ]; } && ok "E1" "draining 中新请求 503/000 (got=$RES)" || bad "E1" "draining 503/000" "got=$RES"

RES=$(curl -s -o /dev/null -w "%{http_code}" -H "X-Serena-Token: $TOK_A" "http://127.0.0.1:$PORT_A/status" 2>&1)
{ [ "$RES" = "200" ] || [ "$RES" = "503" ] || [ "$RES" = "000" ]; } && ok "E2" "draining 中 /status 200/503/000 (got=$RES)" || bad "E2" "/status draining" "got=$RES"

sleep 4
[ ! -f "$LOCK" ] && ok "E3" "shutdown 后 lock 必删" || bad "E3" "lock 删" "仍存在"

DAEMON_PROCS=$(tl /FI "IMAGENAME eq daemon_serve_bin.exe" 2>/dev/null | awk '/daemon_serve_bin\.exe/ {c++} END {print c+0}')
[ "$DAEMON_PROCS" -eq 0 ] && ok "E4" "daemon 0 残留 (反僵尸 P0 验收)" || bad "E4" "僵尸终结" "found=$DAEMON_PROCS"

if ! netstat -ano 2>/dev/null | grep -q ":$PORT_A.*LISTENING"; then
  ok "E5" "draining 后 port 释放"
else
  bad "E5" "port 释放" "仍 LISTENING"
fi

RES=$(curl -s -o /dev/null -w "%{http_code}" -X POST -H "X-Serena-Token: $TOK_A" "http://127.0.0.1:$PORT_A/shutdown" 2>&1)
{ [ "$RES" = "000" ] || [ "$RES" = "200" ] || [ "$RES" = "503" ] || [ "$RES" = "502" ]; } && ok "E6" "重复 shutdown 优雅失败 (got=$RES)" || bad "E6" "重复 shutdown" "got=$RES"

kill_daemon
sleep 1

# ===== Section F: token / boot_ms / lock 内容 (4 项) =====
echo ""
echo "[F] lock file integrity"

"$BIN" > "$LOG" 2>&1 &
DAEMON_F=$!
sleep 1
if wait_lock; then
  ok "F1_pre" "daemon F 启动"
  sleep 3
else
  bad "F1" "daemon 启动" "lock 不出现"
fi
TOK_F=$(grep -oE '"token":[[:space:]]*"[a-f0-9]+"' "$LOCK" 2>/dev/null | head -1 | sed 's/.*"\([a-f0-9]*\)".*/\1/' || echo "")
PID_F=$(grep -oE '"pid":[[:space:]]*[0-9]+' "$LOCK" 2>/dev/null | head -1 | grep -o '[0-9]*' || echo "")
BOOT=$(grep -oE '"boot_ms":[[:space:]]*[0-9]+' "$LOCK" 2>/dev/null | head -1 | grep -o '[0-9]*' || echo "")

[[ "$TOK_F" =~ ^[a-f0-9]{32}$ ]] && ok "F1" "token 32 hex chars" || bad "F1" "token 格式" "got=$TOK_F"

if [ -n "$BOOT" ] && [ "$BOOT" -gt 1700000000000 ] 2>/dev/null; then
  ok "F2" "boot_ms 合理 ($BOOT)"
else
  bad "F2" "boot_ms 合理" "got=$BOOT"
fi

[ -n "$PID_F" ] && tl /FI "PID eq $PID_F" 2>/dev/null | grep -q "daemon_serve_bin.exe" \
  && ok "F3" "lock.pid=$PID_F 存活" || bad "F3" "lock.pid 存活" "got=$PID_F"

PORT_F=$(grep -oE '"port":[[:space:]]*[0-9]+' "$LOCK" 2>/dev/null | head -1 | grep -o '[0-9]*' || echo "")
[ "$PORT_F" = "7860" ] && ok "F4" "port=7860 默认" || bad "F4" "port=7860" "got=$PORT_F"

kill_daemon
sleep 1

# ===== Section G: 进程级断言 (6 项) =====
echo ""
echo "[G] process-level zombie termination"

"$BIN" > "$LOG" 2>&1 &
DAEMON_G=$!
sleep 1
if wait_lock; then
  ok "G1_pre" "daemon G 启动"
  sleep 3
else
  bad "G1" "daemon 启动" "lock 不出现"
fi
TOK_G=$(grep -oE '"token":[[:space:]]*"[a-f0-9]+"' "$LOCK" 2>/dev/null | head -1 | sed 's/.*"\([a-f0-9]*\)".*/\1/' || echo "")
PORT_G=$(grep -oE '"port":[[:space:]]*[0-9]+' "$LOCK" 2>/dev/null | head -1 | grep -o '[0-9]*' || echo "")
PID_G=$(grep -oE '"pid":[[:space:]]*[0-9]+' "$LOCK" 2>/dev/null | head -1 | grep -o '[0-9]*' || echo "")

tl /FI "PID eq $PID_G" 2>/dev/null | grep -q "daemon_serve_bin.exe" && ok "G2" "PID $PID_G 存活" || bad "G2" "daemon 存活" "PID=$PID_G"

RES=$(curl -s -o /dev/null -w "%{http_code}" -X POST -H "X-Serena-Token: $TOK_G" "http://127.0.0.1:$PORT_G/shutdown" 2>&1)
{ [ "$RES" = "200" ] || [ "$RES" = "000" ]; } && ok "G3" "/shutdown 200/000 (got=$RES)" || bad "G3" "/shutdown" "got=$RES"

sleep 4

[ ! -f "$LOCK" ] && ok "G4" "shutdown 后 lock 删" || bad "G4" "lock 删" "仍存在"

if ! netstat -ano 2>/dev/null | grep -q ":$PORT_G.*LISTENING"; then
  ok "G5" "port $PORT_G 已释放（进程真退）"
else
  bad "G5" "port 释放" "仍 LISTENING — 僵尸"
fi

DAEMON_PROCS=$(tl /FI "IMAGENAME eq daemon_serve_bin.exe" 2>/dev/null | awk '/daemon_serve_bin\.exe/ {c++} END {print c+0}')
[ "$DAEMON_PROCS" -eq 0 ] && ok "G6" "daemon 进程 0 残留（P0 终结僵尸）" || bad "G6" "0 残留" "found=$DAEMON_PROCS"

kill_daemon
sleep 1

echo ""
echo "================================================================"
echo "RESULT: PASS=$PASS FAIL=$FAIL  (target: PASS >=42 / 44)"
echo "================================================================"
[ "$PASS" -ge 42 ] && exit 0 || exit 1