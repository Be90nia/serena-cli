#!/usr/bin/env bash
# 一次性端到端验证：ToolError 三变体 → wire 码 → CLI exit 码（LaunchReclass 任务）
# 产物：stdout 全文（含每步 exit code）。用后即删。
set -u
cd "D:/Project/serena-rust"
BIN=target/debug/cli.exe
ROOT="D:/Project/serena-rust"
LOCK="$LOCALAPPDATA/serena/daemon.lock"

echo "== [0] cargo build -p cli =="
cargo build -p cli 2>&1 | tail -2
echo "build exit=${PIPESTATUS[0]}"

echo "== [1] stop pre-existing daemon (graceful) =="
"$BIN" stop-all; echo "stop-all exit=$?"
sleep 2
rm -f "$LOCK"

echo "== [2] start fresh daemon (new binary, with Serialize/Protocol variants) =="
"$BIN" --daemon &
DAEMON_PID=$!

# 就绪轮询：lock 出现 + /status 可达
UP=0
for i in $(seq 1 60); do
  if [ -f "$LOCK" ]; then
    T=$(sed -n 's/.*"token": *"\([^"]*\)".*/\1/p' "$LOCK" 2>/dev/null); P=$(sed -n 's/.*"port": *\([0-9]*\).*/\1/p' "$LOCK" 2>/dev/null)
    if [ -n "$T" ] && [ "$T" != "null" ] && curl -s -o /dev/null -H "X-Serena-Token: $T" "http://127.0.0.1:$P/status"; then
      UP=1; break
    fi
  fi
  sleep 0.25
done
echo "daemon up=$UP port=$P"
[ "$UP" = "1" ] || { echo "FATAL: daemon not ready"; kill $DAEMON_PID 2>/dev/null; exit 1; }

echo "== [3] curl POST /tools/find-symbol 非法 args（缺 query）=="
curl -s -w '\nHTTP_STATUS:%{http_code}\n' -X POST "http://127.0.0.1:$P/tools/find-symbol" \
  -H "X-Serena-Token: $T" -H "Content-Type: application/json" \
  -d "{\"project_root\":\"$ROOT\",\"args\":{},\"lang\":null}"
echo "curl exit=$?"

echo "== [4] curl POST /tools/find-symbol 非法 args（query 非字符串）=="
curl -s -w '\nHTTP_STATUS:%{http_code}\n' -X POST "http://127.0.0.1:$P/tools/find-symbol" \
  -H "X-Serena-Token: $T" -H "Content-Type: application/json" \
  -d "{\"project_root\":\"$ROOT\",\"args\":{\"query\":123},\"lang\":null}"

echo "== [5] CLI read-file 不存在文件 → wire BAD_ARGS → 期望 exit 2（旧版硬编码恒 exit 1）=="
"$BIN" --project "$ROOT" read-file "definitely-not-there-$$.rs" 2>&1
echo "cli exit=$?"

echo "== [6] CLI find-symbol 缺参（clap 层，对照组）=="
"$BIN" --project "$ROOT" find-symbol 2>&1 | tail -1
echo "cli exit=${PIPESTATUS[0]}"

echo "== [7] cleanup =="
"$BIN" stop-all; echo "stop-all exit=$?"
sleep 1
kill $DAEMON_PID 2>/dev/null
echo "== done =="
