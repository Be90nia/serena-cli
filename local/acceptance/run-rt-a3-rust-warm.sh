#!/bin/bash
# A 段补测：warm daemon 上的语义复核（diagnostics-err / containing / replace-body / safe-delete 门）
set -u
cd /d/Project/serena-rust
B=./target/release/cli.exe
PROJ='D:/Project/serena-rust/fixtures/rust_demo'
LIB="$PROJ/lib.rs"
MAIN="$PROJ/main.rs"
LOG=local/acceptance/rt-a3-rust-warm.log.md
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"

restore() { git checkout -- fixtures/rust_demo 2>/dev/null; }

: >"$LOG"
echo "# A3 rust warm-daemon 语义复核 $(date '+%F %T')" >>"$LOG"

run() { # run <label> <args...>
  local label="$1"; shift
  {
    echo "### $label"
    echo '```'
    "$B" --project "$PROJ" --json "$@" 2>&1
    echo '```'
    echo
  } >>"$LOG"
}

echo "== status (daemon alive?)"; "$B" status 2>&1 | tr -d '\n '; echo

# 1) diagnostics-err：注入 E0425 到已打开的 lib.rs
restore
printf '\nfn broken() { let _x = undefined_xyz_fn(); }\n' >>"$LIB"
run diagnostics-err-opened diagnostics "$LIB" --wait-gen 2
restore

# 2) diagnostics-err 变体：注入到尚未打开的新文件
printf 'fn scratch_broken() { let _y = undefined_abc_fn(); }\n' >"$PROJ/scratch_broken.rs"
run diagnostics-err-fresh diagnostics "$PROJ/scratch_broken.rs" --wait-gen 2
rm -f "$PROJ/scratch_broken.rs"

# 3) containing-symbol（warm 复测解码）
run containing-warm containing-symbol "$LIB" 1 6

# 4) replace-body 落盘验证
run replace-body replace-body --with '    a * 2' "$LIB" multiply
{
  echo '### replace-body-after'
  echo '```'
  sed -n '4,7p' "$LIB"
  echo '```'
  echo
} >>"$LOG"
restore

# 5) safe-delete：语义存活下（a）真无引用 multiply（b）被引用 main.rs add
run safe-delete-norefs safe-delete-symbol "$LIB" multiply
restore
run safe-delete-referenced safe-delete-symbol "$MAIN" add
restore

echo DONE >>"$LOG"
echo "log: $LOG"
