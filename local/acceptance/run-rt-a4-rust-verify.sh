#!/bin/bash
# A4 补测：replace-body 全文件对比 + in-tree 新文件 E0425 + safe-delete 门计数源
set -u
cd /d/Project/serena-rust
B=./target/release/cli.exe
PROJ='D:/Project/serena-rust/fixtures/rust_demo'
LIB="$PROJ/lib.rs"
MAIN="$PROJ/main.rs"
LOG=local/acceptance/rt-a4-rust-verify.log.md
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"
restore() { git checkout -- fixtures/rust_demo 2>/dev/null; }

: >"$LOG"
echo "# A4 replace-body/diagnostics/safe-delete 复核 $(date '+%F %T')" >>"$LOG"

# 1) replace-body 全文件对比
restore
{
  echo '### lib.rs BEFORE replace-body'
  echo '```'
  cat "$LIB"
  echo '```'
} >>"$LOG"
"$B" --project "$PROJ" --json replace-body --with '    a * 2' "$LIB" multiply >>"$LOG" 2>&1
{
  echo '### lib.rs AFTER replace-body multiply --with "    a * 2"'
  echo '```'
  cat "$LIB"
  echo '```'
} >>"$LOG"
restore

# 2) in-tree 新文件 E0425（模块树内、未打开）
printf 'fn scratch_broken() { let _y = undefined_abc_fn(); }\n' >"$PROJ/scratch_broken.rs"
printf 'mod scratch_broken;\n' >>"$MAIN"
{
  echo '### diagnostics in-tree fresh file (E0425 expected)'
  echo '```'
  "$B" --project "$PROJ" --json diagnostics "$PROJ/scratch_broken.rs" --wait-gen 3 2>&1
  echo '```'
} >>"$LOG"
rm -f "$PROJ/scratch_broken.rs"
restore

# 3) rename 落盘复核（lib.rs add → adder；main.rs 有自己的 add，不受影响属正确）
restore
{
  echo '### rename lib.rs add→adder'
  echo '```'
  "$B" --project "$PROJ" --json rename-symbol --to adder "$LIB" 0 7 2>&1
  echo '```'
  echo '### lib.rs after rename'
  echo '```'
  sed -n '1,3p' "$LIB"
  echo '```'
} >>"$LOG"
restore

echo DONE >>"$LOG"
