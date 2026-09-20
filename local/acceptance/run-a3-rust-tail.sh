#!/bin/bash
# A3 段：rust_demo 全命令 e2e 尾部（daemon 崩溃后续跑部分）
set -u
cd /d/Project/serena-rust
B=./target/release/cli.exe
PROJ='D:/Project/serena-rust/fixtures/rust_demo'
LIB="$PROJ/lib.rs"
MAIN="$PROJ/main.rs"
LOG=local/acceptance/a3-rust-tail.log.md
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"

restore() { git checkout -- fixtures/rust_demo 2>/dev/null; }

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
echo "# A3 段 rust_demo 尾部（daemon） $(date '+%F %T')" >>"$LOG"
restore

t read-file       read-file "$LIB" --start-line 1 --end-line 2
t list-dir        list-dir "$PROJ"
t find-file       find-file main.rs
t sig-help        signature-help "$MAIN" 3 17
t doc-highlight   document-highlight "$MAIN" 3 12
t folding-range   folding-range "$LIB"
t semantic-tokens semantic-tokens "$LIB"
t inlay-hint      inlay-hint "$LIB" 0 2
t code-action     code-action "$MAIN" 3 12
t format-clean    format "$LIB"

printf '\nfn broken() { let _x = undefined_xyz_fn(); }\n' >>"$LIB"
t diagnostics-err diagnostics "$LIB" --wait-gen 2
restore

printf 'fn scratch_fn() { ad}\n' >>"$LIB"
t completion      completion "$LIB" 7 20
restore

printf 'fn ugly( ){let _x=a+b;}\n' >>"$LIB"
t format-dirty    format "$LIB"
restore

t ch-prepare      call-hierarchy prepare "$MAIN" 3 12
ITEM=$("$B" --project "$PROJ" --json call-hierarchy prepare "$MAIN" 3 12 2>/dev/null | jq -c '.[0]' 2>/dev/null)
if [ -n "$ITEM" ] && [ "$ITEM" != "null" ] && [ -n "${ITEM:-}" ]; then
  t ch-incoming   call-hierarchy incoming --item "$ITEM"
  t ch-outgoing   call-hierarchy outgoing --item "$ITEM"
else
  echo "### call-hierarchy: prepare 未返回 item，incoming/outgoing 跳过" >>"$LOG"
fi

t replace-body    replace-body --with '    a * 2' "$LIB" multiply
restore
t replace-text    replace-text-in-symbol "$LIB" multiply 'a * b' 'a * 3'
restore
t insert-before   insert-text-before-symbol "$LIB" multiply '// before'
restore
t insert-after    insert-text-after-symbol "$LIB" multiply '// after'
restore
t delete-text     delete-text-in-symbol "$LIB" multiply 1 1
restore
t safe-delete-ok  safe-delete-symbol "$LIB" multiply
restore
t safe-delete-ref safe-delete-symbol "$LIB" add
restore
t insert-at-line  insert-at-line "$LIB" 1 '// inserted'
restore
t replace-lines   replace-lines "$LIB" 5 7 'fn replaced() {}'
restore
t delete-lines    delete-lines "$LIB" 5 7
restore
t rename          rename-symbol --to adder "$LIB" 0 7
echo "RENAME_CHECK adder in main.rs: $(grep -c adder fixtures/rust_demo/main.rs 2>/dev/null)" >>"$LOG"
restore

echo DONE >>"$LOG"
echo "log: $LOG"
