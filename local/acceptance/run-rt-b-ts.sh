#!/bin/bash
# B 段：typescript_demo 核心 20 条（daemon 模式）rename/completion/diagnostics 必测
set -u
cd /d/Project/serena-rust
B=./target/release/cli.exe
PROJ='D:/Project/serena-rust/fixtures/typescript_demo'
TS="$PROJ/main.ts"
LOG=local/acceptance/rt-b-ts.log.md
export PATH="/c/Users/Begonia/.local/bin:/d/go-workspace/bin:$(cygpath -p "$PATH")"

restore() { git checkout -- fixtures/typescript_demo 2>/dev/null; }

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
echo "# B 段 typescript_demo 核心 20（daemon） $(date '+%F %T')" >>"$LOG"
restore

t overview        overview "$TS"
t symbol-tree     symbol-tree "$PROJ" --max-files 5
t find-symbol     find-symbol Calculator
t def             def "$TS" 7 22
t refs            refs "$TS" 0 16
t hover           hover "$TS" 0 16
t diagnostics     diagnostics "$TS" --wait-gen 1
t symbol-body     symbol-body "$TS" compute
t containing      containing-symbol "$TS" 8 10
t defining        defining-symbol "$TS" 7 22
t find-ref-syms   find-referencing-symbols "$TS" 7 22
t find-ref-snips  find-referencing-code-snippets "$TS" 7 22
t completion      completion "$TS" 7 26
t sig-help        signature-help "$TS" 7 25
t doc-highlight   document-highlight "$TS" 0 16
t semantic-tokens semantic-tokens "$TS"
t folding-range   folding-range "$TS"
t code-action     code-action "$TS" 7 22
t format          format "$TS"
t search          search "Calculator"
t rename          rename-symbol --to multiplier "$TS" 0 16
echo "RENAME_CHECK multiplier in main.ts: $(grep -c multiplier fixtures/typescript_demo/main.ts 2>/dev/null)" >>"$LOG"
restore
t safe-delete     safe-delete-symbol "$TS" multiply
restore

echo DONE >>"$LOG"
echo "log: $LOG"
