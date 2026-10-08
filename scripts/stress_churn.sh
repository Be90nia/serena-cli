#!/usr/bin/env bash
# stress_churn.sh —— 多语言并发 churn 场景（bd serena-rust-8g4"竞锁"实测面）。
#
# 与 stress_loop.sh（分片门循环）互补：单一 workspace 预置多语言 fixture
# （python/typescript/go/rust/java/vue/html/json），N 个 CLI 进程并发循环执行
# 混合操作（读 overview/find-symbol/refs + 写 insert-at-line/replace-lines/
# rename 交错 + 周期 undo），共享同一 lazy-spawn daemon —— 量的是 daemon 并发
# 请求调度与锁行为，不是单门红绿。
#
# 无正确性断言（压测不保内容语义）：写操作互相踩/undo 回滚他人事务是测试信号，
# 全部落错误分类计数；失败条件 = 脚本机制性故障（workspace 建不起来 / 全语言
# probe 失败 / 哨兵显示 daemon 恒缺）。
#
# 用法：stress_churn.sh [--duration SECS] [--workers N] [--langs LIST] [--log-dir DIR]
#   --duration  压测窗口秒（默认 120；机制验证 30）
#   --workers   并发 CLI 进程数（默认 4；自动 clamp 到语言数）
#   --langs     逗号分隔（默认全部 8；子集如 python,typescript）
# 环境变量：SERENA_CLI（默认 serena-cli）；CHURN_POLL 哨兵间隔秒（默认 10）。
# 产物（<log-dir>/）：ops-w<N>.txt（每 op 一行 ok | err <CODE>）、sentinel-<i>.txt、
#   churn-report.md（ops 计数 + 错误按 error.code 分类 + 哨兵趋势表）。

set -u -o pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERENA_CLI="${SERENA_CLI:-serena-cli}"
POLL="${CHURN_POLL:-10}"
LOG_DIR=""
DURATION=120
WORKERS=4
LANGS="python,typescript,go,rust,java,vue,html,json"

log() { printf '%s\n' "$*" >&2; }

while [ $# -gt 0 ]; do
    case "$1" in
        --duration) DURATION="$2"; shift 2 ;;
        --workers) WORKERS="$2"; shift 2 ;;
        --langs) LANGS="$2"; shift 2 ;;
        --log-dir) LOG_DIR="$2"; shift 2 ;;
        -h | --help) sed -n '2,25p' "${BASH_SOURCE[0]}"; exit 0 ;;
        *) log "FAIL churn: unknown arg $1"; exit 2 ;;
    esac
done
case "$DURATION" in '' | *[!0-9]*) log "FAIL churn: bad --duration"; exit 2 ;; esac
case "$WORKERS" in '' | *[!0-9]*) log "FAIL churn: bad --workers"; exit 2 ;; esac

PLAT=linux
case "$(uname -s)" in
    Darwin*) PLAT=macos ;;
    MING* | MSYS* | CYGWIN*) PLAT=windows ;;
esac

# timeout 守卫同 smoke_one.sh：Git Bash 可能命中 Windows timeout.exe（语法互斥），
# 试跑通过才用；无 timeout（本机调试/macos 老版）退化直跑，CI 三平台 ubuntu 必有。
if command -v timeout >/dev/null 2>&1 && timeout 1 true 2>/dev/null; then
    run_op() { timeout 90 "$SERENA_CLI" "$@"; }
else
    run_op() { "$SERENA_CLI" "$@"; }
fi

# python（哨兵 windows tasklist CSV 解析用）：试跑验证而非仅 command -v——
# windows 的 python3 可能命中 Microsoft Store stub（打印警告零输出）。
PY=""
for c in python3 python py; do
    if command -v "$c" >/dev/null 2>&1 && "$c" -c 'print(1)' >/dev/null 2>&1; then PY="$c"; break; fi
done

# ---- 语言 spec：file|anchor_line|anchor_col|comment_prefix（col 空 = 无符号语言，
# 跳过 refs/rename；锚点行列与下方 fixture 逐字对齐）----
lang_file() {
    case "$1" in
        python) echo "main.py" ;;
        typescript) echo "main.ts" ;;
        go) echo "main.go" ;;
        rust) echo "src/main.rs" ;;
        java) echo "Main.java" ;;
        vue) echo "app.vue" ;;
        html) echo "index.html" ;;
        json) echo "data.json" ;;
        *) return 1 ;;
    esac
}
lang_anchor() { # line col
    case "$1" in
        python) echo "5 5" ;;
        typescript) echo "3 17" ;;
        go) echo "4 6" ;;
        rust) echo "2 8" ;;
        java) echo "2 17" ;;
        vue) echo "2 10" ;;
        *) echo "" ;;
    esac
}
lang_comment() {
    case "$1" in
        python) echo "#" ;;
        html) echo "<!--" ;;
        *) echo "//" ;;
    esac
}

# ---- workspace：fixture 落 TEMP（嵌 workspace cargo metadata 静默死教训）。
# windows CI 的 mktemp→$TEMP 是 8.3 短名（RUNNER~1 → pyright 系 -32602 教训），
# RUNNER_TEMP 在场时显式 cygpath 长路径。----
if [ -n "${RUNNER_TEMP:-}" ]; then
    WS="$(cygpath -u "$RUNNER_TEMP" 2>/dev/null || echo "$RUNNER_TEMP")/churn-ws-$$"
    mkdir -p "$WS" || { log "FAIL churn: mkdir $WS"; exit 1; }
else
    WS=$(mktemp -d) || { log "FAIL churn: mktemp"; exit 1; }
fi
LOG_DIR="${LOG_DIR:-$PWD/churn-logs}"
mkdir -p "$LOG_DIR" || { log "FAIL churn: mkdir log-dir"; exit 1; }

write_fixtures() {
    cat >"$WS/Cargo.toml" <<'EOF'
[package]
name = "churn-fixture"
version = "0.1.0"
edition = "2021"
EOF
    mkdir -p "$WS/src"
    cat >"$WS/src/main.rs" <<'EOF'
//! churn fixture: rust
pub fn churn_target(x: i32) -> i32 {
    x * 2
}

pub fn churn_pool() -> Vec<i32> {
    (0..3).map(churn_target).collect()
}
EOF
    cat >"$WS/main.py" <<'EOF'
# churn fixture: python
"""Module docstring."""


def churn_target(x: int) -> int:
    return x * 2


def churn_pool() -> list:
    return [churn_target(i) for i in range(3)]
EOF
    cat >"$WS/main.ts" <<'EOF'
// churn fixture: typescript
export const VERSION = 1;
export function churnTarget(x: number): number {
    return x * 2;
}
export function churnPool(): number[] {
    return [1, 2, 3].map(churnTarget);
}
EOF
    cat >"$WS/main.go" <<'EOF'
package churn

// ChurnTarget doubles x.
func ChurnTarget(x int) int {
	return x * 2
}

func ChurnPool() []int {
	out := []int{}
	for i := 0; i < 3; i++ {
		out = append(out, ChurnTarget(i))
	}
	return out
}
EOF
    cat >"$WS/Main.java" <<'EOF'
public class Main {
    static int churnTarget(int x) {
        return x * 2;
    }

    public static void main(String[] args) {
        System.out.println(churnTarget(21));
    }
}
EOF
    cat >"$WS/app.vue" <<'EOF'
<script setup lang="ts">
function churnTarget(x: number): number {
    return x * 2;
}
const n = churnTarget(2);
</script>

<template>
    <p>{{ n }}</p>
</template>
EOF
    cat >"$WS/index.html" <<'EOF'
<!DOCTYPE html>
<html>
<head><title>churn</title></head>
<body>
    <h1 id="churn-target">churn fixture</h1>
    <p class="churn-pool">html door</p>
</body>
</html>
EOF
    cat >"$WS/data.json" <<'EOF'
{
    "churn_target": 42,
    "churn_pool": [1, 2, 3],
    "meta": { "fixture": "churn" }
}
EOF
}

# ---- 哨兵（对齐 stress_loop.sh 同款实现：进程表观察，不调 status 自唤 daemon）----
# windows：tasklist 的 /FO 会被 MSYS 路径转换吃成 <Git>/FO（双层 spawn 实锚）——
# MSYS_NO_PATHCONV 禁转换；非 MSYS 环境该 env 无害被忽略。
run_tasklist() { MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL="*" tasklist /FO CSV /NH 2>/dev/null; }

LS_PROC_RE='rust-analyzer|gopls|pyright|basedpyright|pyrefly|fortls|clangd|jdtls|kotlin|dart|lua-language-server|zls|metals|sourcekit|omnisharp|csharp-ls|fsautocomplete|ruby-lsp|solargraph|intelephense|phpactor|phpantom|clojure-lsp|erlang_ls|haskell-language-server|ocamllsp|crystalline|pasls|texlab|marksman|taplo|regal|deno|gleam|lean|julia|node|python'

count_ls_procs() {
    case "$PLAT" in
    windows) run_tasklist | grep -icE "\"(${LS_PROC_RE})" ;;
    *) ps -axo comm= 2>/dev/null | grep -icE "(${LS_PROC_RE})" ;;
    esac
}

daemon_procs_and_rss() {
    if [ "$PLAT" = windows ] && [ -n "$PY" ]; then
        run_tasklist | "$PY" -c 'import csv, sys
n, kb = 0, 0
for row in csv.reader(sys.stdin):
    if len(row) >= 4 and row[0].lower().startswith("serena-cli"):
        n += 1
        mem = row[-1].strip()
        if mem.endswith("K"):
            kb += int(mem.replace(",", "").replace(" K", "").strip() or 0)
print(f"daemon_procs={n} daemon_rss_kb={kb}")'
    elif [ "$PLAT" = windows ]; then
        echo "daemon_procs=? daemon_rss_kb=?"
    else
        ps -axo comm=,rss= 2>/dev/null | awk '$1 ~ /serena-cli/ { n++; kb += $2 } END { printf "daemon_procs=%d daemon_rss_kb=%d\n", n+0, kb+0 }'
    fi
}

lock_dir() {
    case "$PLAT" in
    windows) cygpath -u "$LOCALAPPDATA" 2>/dev/null || echo "." ;;
    *) echo "$HOME/.serena" ;;
    esac
}

collect_sentinel() { # $1 = seq
    {
        printf 'ts=%s\n' "$(date +%s)"
        printf 'ls_procs=%s\n' "$(count_ls_procs)"
        daemon_procs_and_rss
        printf 'lock_files=%s\n' "$(ls "$(lock_dir)"/daemon.lock* 2>/dev/null | wc -l)"
    } >"$LOG_DIR/sentinel-$1.txt"
}

# ---- 单 op：错误分类计数（"tool error: {\"code\":...}" 落 stderr，rc=2）----
err_code() { printf '%s' "$1" | grep -oE '"code":"[A-Z_]+"' | head -1 | cut -d'"' -f4; }

op() { # worker_no args...
    local w="$1"
    shift
    local err rc code
    err=$(run_op --project "$WS" "$@" 2>&1 >/dev/null)
    rc=$?
    if [ "$rc" -eq 0 ]; then
        echo "ok" >>"$LOG_DIR/ops-w$w.txt"
    else
        code=$(err_code "$err")
        echo "err ${code:-RC$rc}" >>"$LOG_DIR/ops-w$w.txt"
        # 原始错误留痕（报告出每 code 首条样本——仅 code 分类不足以排障）
        printf '%s\n' "$err" >>"$LOG_DIR/errs-w$w.log"
    fi
}

worker() { # w lang1 lang2...
    local w="$1"
    shift
    local seq=0 t file f line col comment lines
    while [ "$(date +%s)" -lt "$DEADLINE" ]; do
        seq=$((seq + 1))
        for f in "$@"; do
            file="$WS/$f"
            [ -f "$file" ] || continue
            op "$w" overview "$file"
            op "$w" find-symbol churn --limit 5
            read -r line col <<<"$(lang_anchor "${f##*.}")"
            if [ -n "$line" ]; then
                op "$w" refs "$file" "$line" "$col"
                op "$w" rename-symbol --to "churn_w${w}_x" "$file" "$line" "$col"
                op "$w" rename-symbol --to churn_target "$file" "$line" "$col"
            fi
            comment="$(lang_comment "${f##*.}")"
            lines=$(wc -l <"$file" | tr -d ' ')
            op "$w" insert-at-line "$file" "$lines" "$comment churn marker w$w s$seq"
            op "$w" replace-lines "$file" "$lines" "$lines" "$comment churn w$w s$seq updated"
        done
        # undo 交错：回滚全局最近事务（可能是他人写）——竞锁主测点
        [ $((seq % 3)) -eq 0 ] && op "$w" undo
    done
    echo "done" >>"$LOG_DIR/ops-w$w.txt"
}

# ---- 主流程 ----
write_fixtures

# 语言 probe：LS 未装/装法不可用的语言剔除（probe 失败照记错误分类）
ACTIVE=""
for l in $(echo "$LANGS" | tr ',' ' '); do
    lang_file "$l" || { log "LOG churn: unknown lang $l skipped"; continue; }
    f="$(lang_file "$l")"
    err=$(run_op --project "$WS" overview "$WS/$f" 2>&1 >/dev/null)
    rc=$?
    if [ "$rc" -ne 0 ]; then
        log "LOG churn: lang $l probe failed ($(err_code "$err")/RC$rc) — excluded"
        continue
    fi
    ACTIVE="$ACTIVE $l"
done
[ -n "$ACTIVE" ] || { log "FAIL churn: all lang probes failed (daemon/LS unavailable)"; rm -rf "$WS"; exit 1; }

# worker 分配：语言轮流分给 min(WORKERS, n_langs) 个 worker
NWORKERS=0
set -- $ACTIVE
[ "$WORKERS" -lt "$#" ] && NWORKERS=$WORKERS || NWORKERS=$#
# 统一 epoch：SECONDS 是 shell 相对秒，与 date +%s 混比恒假（worker 立即退出实锚）
DEADLINE=$(( $(date +%s) + DURATION ))

log "churn: plat=$PLAT duration=${DURATION}s workers=$NWORKERS langs:$ACTIVE ws=$WS"
i=0
assign=""
declare -a ASSIGN
for l in $ACTIVE; do
    ASSIGN[$((i % NWORKERS))]="${ASSIGN[$((i % NWORKERS))]:-} $(lang_file "$l")"
    i=$((i + 1))
done
for w in $(seq 1 "$NWORKERS"); do
    # shellcheck disable=SC2086
    worker "$w" ${ASSIGN[$((w - 1))]} &
done

sn=0
while [ "$(date +%s)" -lt "$DEADLINE" ]; do
    sleep "$POLL"
    sn=$((sn + 1))
    collect_sentinel "$sn"
done
wait

# ---- 报告 ----
n_ok=$(grep -h '^ok$' "$LOG_DIR"/ops-w*.txt 2>/dev/null | wc -l)
n_err=$(grep -h '^err ' "$LOG_DIR"/ops-w*.txt 2>/dev/null | wc -l)
{
    echo "# Churn Report — $PLAT ${DURATION}s workers=$NWORKERS langs:$(echo $ACTIVE | tr ' ' ',')"
    echo ""
    echo "- ops: ok=$n_ok err=$n_err ($((n_ok + n_err)) total)"
    echo ""
    echo "## Errors by code"
    echo ""
    echo "| code | count |"
    echo "|---|---|"
    grep -h '^err ' "$LOG_DIR"/ops-w*.txt 2>/dev/null | awk '{print $2}' | sort | uniq -c |
        while read -r n code; do echo "| $code | $n |"; done
    echo ""
    echo "## Error samples"
    echo ""
    echo '```'
    head -c 3000 "$LOG_DIR"/errs-w*.log 2>/dev/null | head -20
    echo '```'
    echo ""
    echo "## Per worker"
    echo ""
    echo "| worker | ok | err |"
    echo "|---|---|---|"
    for w in $(seq 1 "$NWORKERS"); do
        o=$(grep -c '^ok$' "$LOG_DIR/ops-w$w.txt" 2>/dev/null || true)
        e=$(grep -c '^err ' "$LOG_DIR/ops-w$w.txt" 2>/dev/null || true)
        echo "| w$w | $o | $e |"
    done
    echo ""
    echo "## Sentinel trend"
    echo ""
    echo "| t | ls_procs | daemon_procs | daemon_rss_kb | lock_files |"
    echo "|---|---|---|---|---|"
    for s in $(seq 1 "$sn"); do
        f="$LOG_DIR/sentinel-$s.txt"
        [ -f "$f" ] || continue
        kv=$(tr '\n' ' ' <"$f")
        lsp=$(printf '%s' "$kv" | grep -oE 'ls_procs=[0-9]+' | cut -d= -f2)
        dp=$(printf '%s' "$kv" | grep -oE 'daemon_procs=[0-9]+' | cut -d= -f2)
        dr=$(printf '%s' "$kv" | grep -oE 'daemon_rss_kb=[0-9]+' | cut -d= -f2)
        lf=$(printf '%s' "$kv" | grep -oE 'lock_files=[0-9]+' | cut -d= -f2)
        echo "| s$s | ${lsp:-?} | ${dp:-?} | ${dr:-?} | ${lf:-?} |"
    done
} >"$LOG_DIR/churn-report.md"

# 先 stop-all（LS 进程持 fixture 目录锁，windows rm 会 Device busy）再清 workspace
"$SERENA_CLI" stop-all >/dev/null 2>&1 || true
rm -rf "$WS" 2>/dev/null || true
log "DONE churn: ok=$n_ok err=$n_err (report: $LOG_DIR/churn-report.md)"
[ "$n_ok" -gt 0 ] || { log "FAIL churn: zero successful ops"; exit 1; }
