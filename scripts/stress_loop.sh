#!/usr/bin/env bash
# stress_loop.sh —— 3 平台 × 5 小时压测循环（bd serena-rust-2p5）。
#
# 包住 smoke_one.sh --shard N（门执行器/分片复用，零第二份逻辑）：
#   while SECONDS < 上限: cycle++ → 哨兵 before → 片跑一轮（裁决行落 cycle-N.md，
#   含 FAIL 也继续循环——压测量的是跨轮稳定性不是单轮红绿）→ 哨兵 after。
#   上限 = cycle_secs - 2700（45min 报告/上传余量）；短周期验证（≤5400s）按 3/4
#   缩放保底至少一轮。job 末尾 stress_report.py 汇总矩阵/flaky/哨兵趋势。
# 哨兵四件（stress-logs/sentinel-c<N>-{before,after}.txt）：
#   ① 已知 LS 进程名进程数（ps comm / tasklist；node/python 为 npm/pygls 系
#     LS 宿主——趋势信号非精确计数）② daemon RSS（status --json 的 pid →
#     ps -o rss / tasklist）③ 残留 daemon.lock 文件数 ④ LS 缓存目录 MB。
# 用法：stress_loop.sh --shard N --cycle-secs SECONDS
# 环境变量：SERENA_CLI / SMOKE_LANGS / SMOKE_LOG_DIR（默认 $PWD/stress-logs）/ SHARDS
#   ——均透传语义同 smoke_one.sh。

set -u -o pipefail

# BASH_SOURCE/SECONDS/pipefail 语义依赖 bash——被 POSIX sh 拉起时立刻显式失败，
# 不留"随机行号语法错"的残局（shebang 已钉 env bash，此 guard 兜 cron/sh -c 路径）。
[ -n "${BASH_VERSION:-}" ] || {
    echo "stress_loop.sh requires bash (run via bash, not sh)" >&2
    exit 1
}

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SERENA_CLI="${SERENA_CLI:-serena-cli}"
SMOKE_LOG_DIR="${SMOKE_LOG_DIR:-$PWD/stress-logs}"

log() { printf '%s\n' "$*" >&2; }

PLAT=linux
case "$(uname -s)" in
Darwin*) PLAT=macos ;;
MING* | MSYS* | CYGWIN*) PLAT=windows ;;
esac

usage() { log "usage: $0 --shard N --cycle-secs SECONDS"; exit 2; }

SHARD=""
CYCLE_SECS=""
while [ $# -gt 0 ]; do
    case "$1" in
    --shard) SHARD="${2:-}"; shift 2 ;;
    --cycle-secs) CYCLE_SECS="${2:-}"; shift 2 ;;
    *) usage ;;
    esac
done
[ -n "$SHARD" ] && [ -n "$CYCLE_SECS" ] || usage
case "$CYCLE_SECS" in '' | *[!0-9]*) usage ;; esac

# python（tomllib 3.11+）：回退链与 smoke_one.sh find_py 同款——macos 的
# python3/python 可能落 CLT 3.9，版本化命令（python3.1x）在 PATH 深处；windows
# python3 shim 缺位 → python（3.12）/ py launcher 兜底。
PY=""
for c in python3 python py python3.14 python3.13 python3.12 python3.11; do
    if command -v "$c" >/dev/null 2>&1 && "$c" -c 'import tomllib' 2>/dev/null; then
        PY="$c"
        break
    fi
done
[ -n "$PY" ] || { log "FAIL stress no-python-with-tomllib"; exit 1; }

mkdir -p "$SMOKE_LOG_DIR"

LS_PROC_RE='rust-analyzer|gopls|pyright|basedpyright|pyrefly|fortls|clangd|jdtls|kotlin|dart|lua-language-server|zls|metals|sourcekit|omnisharp|csharp-ls|fsautocomplete|ruby-lsp|solargraph|intelephense|phpactor|phpantom|clojure-lsp|erlang_ls|haskell-language-server|ocamllsp|crystalline|pasls|texlab|marksman|taplo|regal|deno|gleam|lean|julia|node|python'

lock_dir() {
    case "$PLAT" in
    windows) cygpath -u "$LOCALAPPDATA" 2>/dev/null || echo "." ;;
    *) echo "$HOME/.serena" ;;
    esac
}

cache_dir() {
    case "$PLAT" in
    windows)
        local base
        base=$(cygpath -u "$LOCALAPPDATA" 2>/dev/null) || base="."
        echo "$base/serena/ls"
        ;;
    *) echo "$HOME/.local/share/serena/ls" ;;
    esac
}

count_ls_procs() {
    case "$PLAT" in
    windows) tasklist /FO CSV /NH 2>/dev/null | grep -icE "\"(${LS_PROC_RE})" ;;
    *) ps -axo comm= 2>/dev/null | grep -icE "(${LS_PROC_RE})" ;;
    esac
}

# daemon 哨兵 = 纯进程表观察（按名数进程 + RSS 合计）。不得调 `status`——它会
# lazy-spawn daemon（本机实测 pid 6189 即 status 自唤），把"无残留"基线污染成
# "恒有 daemon"。门间 stop-all 卫生后应为 0；>0 即残留（泄漏哨兵要抓的信号）。
daemon_procs_and_rss() {
    case "$PLAT" in
    windows)
        tasklist /FO CSV /NH 2>/dev/null | "$PY" -c 'import csv, sys
n, kb = 0, 0
for row in csv.reader(sys.stdin):
    if len(row) >= 4 and row[0].lower().startswith("serena-cli"):
        n += 1
        mem = row[-1].strip()
        if mem.endswith("K"):
            kb += int(mem.replace(",", "").replace(" K", "").strip() or 0)
print(f"daemon_procs={n} daemon_rss_kb={kb}")'
        ;;
    *)
        ps -axo comm=,rss= 2>/dev/null | awk '$1 ~ /serena-cli/ { n++; kb += $2 } END { printf "daemon_procs=%d daemon_rss_kb=%d\n", n+0, kb+0 }'
        ;;
    esac
}

lock_count() {
    ls "$(lock_dir)"/daemon.lock* 2>/dev/null | wc -l
}

cache_mb() {
    du -sm "$(cache_dir)" 2>/dev/null | cut -f1
}

collect_sentinel() { # $1 = tag，如 c1-before
    local f="$SMOKE_LOG_DIR/sentinel-$1.txt"
    {
        printf 'ts=%s\n' "$(date +%s)"
        printf 'ls_procs=%s\n' "$(count_ls_procs)"
        daemon_procs_and_rss
        printf 'lock_files=%s\n' "$(lock_count)"
        printf 'cache_mb=%s\n' "$(cache_mb)"
    } >"$f"
    log "LOG sentinel $1: $(tr '\n' ' ' <"$f")"
}

# 循环上限：全量（>5400s）留 45min 上传余量；短周期验证按 3/4 缩放（600s → 450s，
# 保底 ≥1 轮；cycle 检查在轮首，最后一轮可越过上限——单轮时长 = 片预算，有界）。
if [ "$CYCLE_SECS" -gt 5400 ]; then
    LIMIT=$((CYCLE_SECS - 2700))
else
    LIMIT=$((CYCLE_SECS * 3 / 4))
fi

cycle=0
while [ "$SECONDS" -lt "$LIMIT" ]; do
    cycle=$((cycle + 1))
    # 上轮 windows 门失败路径可能残留锁住的 fixture 目录（node LS 持目录锁）——
    # 轮首 best-effort 清，防 .smoke-tmp 跨 cycle 累积（|| true：仍被锁则容忍）。
    [ -n "${GITHUB_WORKSPACE:-}" ] && rm -rf "$GITHUB_WORKSPACE/.smoke-tmp" 2>/dev/null || true
    log "== stress cycle $cycle start (plat=$PLAT shard=$SHARD SECONDS=$SECONDS limit=$LIMIT)"
    collect_sentinel "c${cycle}-before"
    "$SELF_DIR/smoke_one.sh" --shard "$SHARD" 2>&1 | tee "$SMOKE_LOG_DIR/cycle-${cycle}.md" || true
    collect_sentinel "c${cycle}-after"
    log "== stress cycle $cycle done (SECONDS=$SECONDS)"
done

if [ "$cycle" -eq 0 ]; then
    log "FAIL stress: cycle_secs=$CYCLE_SECS too small for one cycle"
    exit 1
fi

"$PY" "$SELF_DIR/stress_report.py" "$SMOKE_LOG_DIR" "$PLAT" "$SHARD" || exit 1
log "DONE stress: $cycle cycles on $PLAT shard $SHARD (report: $SMOKE_LOG_DIR/stress-report-$PLAT-s$SHARD.md)"
