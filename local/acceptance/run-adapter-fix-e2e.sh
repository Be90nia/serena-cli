#!/bin/bash
# 适配器层 3 缺陷修复 e2e（--direct 模式，不依赖 daemon）：
#   D-py : pyright --stdio（serena-rust-9wy）→ def/hover/diagnostics 真实数据
#   B-ts : percent-URI 解码（serena-rust-5z8）→ defining-symbol 真数据 + rename 落盘
#   E-go : gopls rename documentChanges 回退（serena-rust-xg4）→ rename 落盘
# 用法：bash local/acceptance/run-adapter-fix-e2e.sh
set -u
cd /d/Project/serena-rust
B=./target/release/cli.exe
LOG=local/acceptance/adapter-fix-e2e.log.md
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"

PY_PROJ='D:/Project/serena-rust/fixtures/py_demo'
PY="$PY_PROJ/app.py"
TS_PROJ='D:/Project/serena-rust/fixtures/typescript_demo'
TS="$TS_PROJ/main.ts"
GO_PROJ='D:/Project/serena-rust/fixtures/go_demo'
GO="$GO_PROJ/main.go"

t() {
  local name="$1"; local proj="$2"; shift 2
  echo "### $name" >>"$LOG"
  echo '```' >>"$LOG"
  "$B" --direct --project "$proj" --json "$@" >>"$LOG" 2>&1
  echo "rc=$?" >>"$LOG"
  echo '```'
  echo >>"$LOG"
}

: >"$LOG"
echo "# 适配器 3 缺陷修复 e2e（--direct） $(date '+%F %T')" >>"$LOG"

# ---- D-py: pyright --stdio（9wy）----
git checkout -- fixtures/py_demo 2>/dev/null
t "py-overview"   "$PY_PROJ" overview "$PY"
t "py-def"        "$PY_PROJ" def "$PY" 4 9
t "py-hover"      "$PY_PROJ" hover "$PY" 0 4
t "py-diagnostics" "$PY_PROJ" diagnostics "$PY" --wait-gen 1

# ---- B-ts: percent-URI（5z8）----
git checkout -- fixtures/typescript_demo 2>/dev/null
t "ts-defining"   "$TS_PROJ" defining-symbol "$TS" 7 22
t "ts-rename"     "$TS_PROJ" rename-symbol --to multiplier "$TS" 0 16
echo "RENAME_CHECK multiplier in main.ts: $(grep -c multiplier fixtures/typescript_demo/main.ts 2>/dev/null)" >>"$LOG"
git checkout -- fixtures/typescript_demo 2>/dev/null

# ---- E-go: gopls rename documentChanges 回退（xg4，go.mod 为本脚本前置自建）----
git checkout -- fixtures/go_demo 2>/dev/null
t "go-overview"   "$GO_PROJ" overview "$GO"
t "go-rename"     "$GO_PROJ" rename-symbol --to adder "$GO" 5 5
echo "RENAME_CHECK adder in main.go: $(grep -c adder fixtures/go_demo/main.go 2>/dev/null)" >>"$LOG"
git checkout -- fixtures/go_demo 2>/dev/null

echo DONE >>"$LOG"
echo "log: $LOG"
