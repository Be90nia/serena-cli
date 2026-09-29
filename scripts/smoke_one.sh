#!/usr/bin/env bash
# smoke_one.sh —— 单语言真机冒烟（45 门 CI 矩阵的最小执行单元）。
#
# 断言底线（PM 契约，全门统一，禁止单门私造）：
#   initialize + didOpen + documentSymbol 非空（`overview` 封装）。
#   per-language 加息只经清单 extra_assert 字段登记（hover:LINE:COL | diagnostics）。
# capability 边界降级（Main 裁决 2026-09-29，框架级不个案豁免）：LS 不实现
#   documentSymbol 的门（如 ansible，vscode-ansible#601 NOT_PLANNED）经清单
#   fallback_assert 登记（diagnostics | hover:LINE:COL）；overview 失败/空时降级跑
#   登记探针，PASS 行标 (fallback:<kind>)。触发降级的门跳过 extra_assert（同一
#   探针不跑两遍）；未登记门行为零变化。
#
# 用法：
#   smoke_one.sh <lang>        单门（调试模式，无预算看门狗）
#   smoke_one.sh --shard N     第 N 片循环；整门超预算 → SKIP BUDGET（不红 CI）
# 环境变量：
#   SERENA_CLI   serena-cli 二进制（默认 PATH 上的 serena-cli；CI 指 artifact 路径）
#   SMOKE_LANGS  清单路径（默认 scripts/smoke_langs.toml）
#   SHARDS       片数（默认 6，须与 workflow plan job 一致）
#   PROBE_TIMEOUT 单次探针超时秒（默认 240）
# 输出（每门一行）：PASS <id> (...) | FAIL <id> <stage>: <reason> | SKIP <id> <class>: ...
# 卫生（PM 契约）：每门结束 + 片收尾各一次 stop-all + 等端口真释放（≤10s 轮询，
# 非 sleep）；stderr 非空打 LOG 行（daemon 残留是"恒 pending"第一嫌疑，日志留给排障）。
# 片收尾裁决自检：片内每门恰一条裁决行，缺/多 = 片红（曾因 SKIP 门不进分片计划
# 致 7 门零裁决行，"FAIL=0" 假绿，S9 P0）。

set -u -o pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELF="$SELF_DIR/$(basename "${BASH_SOURCE[0]}")"
SERENA_CLI="${SERENA_CLI:-serena-cli}"
LANGS_FILE="${SMOKE_LANGS:-$SELF_DIR/smoke_langs.toml}"
SHARDS="${SHARDS:-6}"
PROBE_TIMEOUT="${PROBE_TIMEOUT:-240}"

log() { printf '%s\n' "$*" >&2; }

# timeout 跨平台守卫：Git Bash 可能命中 Windows timeout.exe（语法互斥）——
# 试跑通过才启用；无可用 timeout（退化直跑）仅限本机调试，CI ubuntu 必有。
has_timeout() {
    if [ -z "${_HAS_TIMEOUT:-}" ]; then
        if command -v timeout >/dev/null 2>&1 && timeout 1 true 2>/dev/null; then
            _HAS_TIMEOUT=1
        else
            _HAS_TIMEOUT=0
        fi
    fi
    [ "$_HAS_TIMEOUT" = 1 ]
}

run_with_timeout() {
    local secs="$1"
    shift
    if has_timeout; then
        timeout "$secs" "$@"
    else
        "$@"
    fi
}

# ---- python（需 tomllib，3.11+）----
find_py() {
    if [ -z "${_PY:-}" ]; then
        local c
        for c in python3 python; do
            if command -v "$c" >/dev/null 2>&1 && "$c" -c 'import tomllib' 2>/dev/null; then
                _PY="$c"
                return 0
            fi
        done
        log "FAIL smoke no-python-with-tomllib"
        return 1
    fi
}

# ---- 清单读取：每门一行，\x1f 分隔（非空白分隔符——bash read 会合并相邻
# tab/space，空字段（如省略的 install）会左移错位，unit separator 无此坑）----
manifest_rows() {
    "${_PY}" - "$LANGS_FILE" <<'PYEOF'
import sys, tomllib
with open(sys.argv[1], "rb") as f:
    data = tomllib.load(f)
for e in data["lang"]:
    keys = ("id", "via", "install", "pin", "fixture", "lang_flag",
            "budget_secs", "extra_assert", "fallback_assert", "skip_class",
            "skip_reason", "skip_evidence", "verified", "remark")
    print("\x1f".join(str(e.get(k, "")) for k in keys))
PYEOF
}

manifest_row() {
    manifest_rows | awk -F'\x1f' -v want="$1" '$1 == want { found=$0 } END { print found }'
}

log_stderr_if_any() {
    local id="$1" errf="$2"
    if [ -s "$errf" ]; then
        log "LOG $id stderr: $(tr '\n' ' ' <"$errf" | cut -c1-400)"
    fi
}

# ---- 门间卫生：stop-all 后等 daemon 真退净（端口释放），轮询 ≤10s 非固定 sleep ----
# 只认锁文件会漏"锁删进程未死"残留（占 7860 → 下一门 lazy-spawn bind 失败或探针
# 撞 503 DAEMON_DRAINING → documentSymbol 空恒红，run 36528495148 toml/nextflow 实锤）。
# 死/活判据 = 端口闭/开（锁路径有平台差异，不参与；linux CI 主场）。超时只 LOG 不致命
# ——真没退净时下一门探针的有界重试是兜底网。
port_busy() { (exec 3<>/dev/tcp/127.0.0.1/7860) 2>/dev/null; }

wait_daemon_gone() {
    local i=0
    while port_busy && [ "$i" -lt 20 ]; do
        i=$((i + 1))
        sleep 0.5
    done
    if port_busy; then
        log "LOG wait_daemon_gone: port 7860 still busy after 10s"
        return 1
    fi
}

# ---- uv 自举（uvx 类条目依赖；smoke 只认自装，不消费 runner 预装 ----
ensure_uv() {
    command -v uvx >/dev/null 2>&1 && return 0
    log "LOG uvx installing uv (astral.sh installer)"
    curl -LsSf https://astral.sh/uv/install.sh | sh 1>&2
    export PATH="$HOME/.local/bin:$PATH"
    command -v uvx >/dev/null 2>&1
}

# ---- 底线断言：initialize + didOpen + documentSymbol 非空 ----
# overview 输出 = 裸 JSON 数组（可能带人读前缀/后缀），raw_decode 宽容切。
# fallback_assert 非空 = capability 边界门：overview 失败/空时降级跑登记探针
# （复用 extra_assert 执行器；产出即放行并置 BOTTOMLINE_MODE 供 PASS 行标注，
# 取值 "direct" 或降级探针 spec 本身如 "diagnostics"/"hover:5:7"）。
BOTTOMLINE_MODE="direct"

assert_overview() {
    local id="$1" fx="$2" errf="$3" fallback="$4" t0="$5" budget="$6"
    local out rc ev attempt=1 transient
    # --project 恒显式：CLI 缺省 root = CWD（CI = 仓库检出根）——project-based LS
    # （csharp-ls/fsautocomplete/expert/elm-ls/ruby-ls）会在错误的 rootUri 上找工程，
    # documentSymbol 恒空/LoadedProjects 报错（run 36558034167 十门实锤）。
    local fxdir
    fxdir="$(dirname "$fx")"
    while :; do
        out=$(run_with_timeout "$PROBE_TIMEOUT" "$SERENA_CLI" --project "$fxdir" overview "$fx" --lang "$id" 2>"$errf")
        rc=$?
        # 失败证据恒留 LOG 行（CiInfra heads-up：降级路径不得吞 overview 失败现场）。
        log_stderr_if_any "$id" "$errf"
        if [ "$rc" -eq 0 ]; then
            if printf '%s' "$out" | "${_PY}" -c '
import sys, json
raw = sys.stdin.read()
pos = [p for p in (raw.find("["), raw.find("{")) if p >= 0]
if not pos:
    sys.exit("no JSON in overview output")
obj, _ = json.JSONDecoder().raw_decode(raw[min(pos):])
if not (isinstance(obj, list) and len(obj) > 0):
    sys.exit("documentSymbol empty")
'; then
                return 0
            fi
            ev="documentSymbol empty"
        else
            log "LOG $id overview exit=$rc (see LOG stderr above)"
            ev="$(tr '\n' ' ' <"$errf" | cut -c1-300)"
        fi
        # 有界重试（≤5 次 ×10s 间隔，每次完整重发 overview；断言强度不变——最终
        # 一次仍须真过同一 JSON 非空检查）。只对瞬态形态重试：空符号（工程型 LS
        # 未载完 / 503 排空窗）、单请求 30s 超时（python/pyright 实锤）、fsharp
        # LoadedProjects 工程未载完。其余 rc≠0（LS_TERMINATED 等）立即判死。
        # fallback 门不重试：登记探针即底线，恒空属设计（ansible 型），重试纯烧时。
        transient=0
        case "$ev" in
            *"documentSymbol empty"* | *"timed out"* | *"DAEMON_DRAINING"* | *"LoadedProjects"*) transient=1 ;;
        esac
        if [ "$transient" -eq 1 ] && [ -z "$fallback" ] && [ "$attempt" -lt 5 ] &&
            [ "$((SECONDS - t0))" -lt "$((budget - 60))" ]; then
            attempt=$((attempt + 1))
            log "LOG $id probe retry $attempt/5 in 10s (last: $ev)"
            sleep 10
            continue
        fi
        if [ "$transient" -eq 1 ] && [ -z "$fallback" ] && [ "$attempt" -ge 5 ] &&
            [ "$((SECONDS - t0))" -lt "$((budget - 60))" ]; then
            log "LOG $id probe: transient failure persists after $attempt attempts"
        elif [ "$transient" -eq 1 ] && [ "$((SECONDS - t0))" -ge "$((budget - 60))" ]; then
            log "LOG $id probe: door budget exhausted before further retries"
        fi
        break
    done
    if [ -n "$fallback" ]; then
        # capability 边界降级（Main 裁决 2026-09-29）：overview 任何失败形态均试
        # 登记探针（空符号 / rpc 错误含 vtsls "Cannot find provider for
        # documentSymbol"——run 36528495148 实锤的新增触发形态）。降级不洗白：
        # fallback 探针自身要求 LS 活着应答，LS_TERMINATED 类在探针处仍红。
        if run_extra_assert "$id" "$fx" "$fallback" "$errf"; then
            BOTTOMLINE_MODE="$fallback"
            return 0
        fi
        log "FAIL $id probe: overview unavailable and fallback($fallback) silent (capability-gap door; evidence in manifest remark)"
        return 1
    fi
    # daemon 的 lsp_stderr 行晚于本次读取落盘（tracing 异步 + 探测失败即返回），
    # 短暂回捞一次再判死——没有这条，LS 崩因（如 JVM 版本不符）永远不可见。
    sleep 2
    log_stderr_if_any "$id (late)" "$errf"
    log "FAIL $id probe: $ev (attempts=$attempt)"
    return 1
}

# ---- 加息探针（清单 extra_assert 登记制）----
run_extra_assert() {
    local id="$1" fx="$2" spec="$3" errf="$4"
    local rc fxdir
    fxdir="$(dirname "$fx")"
    case "$spec" in
        hover:*)
            local loc="${spec#hover:}" line col
            line="${loc%%:*}"
            col="${loc#*:}"
            run_with_timeout "$PROBE_TIMEOUT" "$SERENA_CLI" --project "$fxdir" hover "$fx" "$line" "$col" --lang "$id" >/dev/null 2>"$errf"
            rc=$?
            ;;
        diagnostics)
            run_with_timeout "$PROBE_TIMEOUT" "$SERENA_CLI" --project "$fxdir" diagnostics "$fx" --wait-gen 1 --lang "$id" >/dev/null 2>"$errf"
            rc=$?
            ;;
        *)
            log "FAIL $id extra_assert: unknown spec \`$spec\` (allowed: hover:LINE:COL | diagnostics)"
            return 1
            ;;
    esac
    log_stderr_if_any "$id" "$errf"
    if [ "$rc" -ne 0 ]; then
        # LOG 而非 FAIL：裁决行由调用方统一发一条（fallback 路径失败时本函数
        # 被 assert_overview 调用，FAIL+FAIL 会造成一门两行，审计必红）。
        log "LOG $id extra_assert: $spec exit=$rc"
        return 1
    fi
}

# ---- 单门（内部步骤，无看门狗）----
one_door() {
    local id="$1"
    local row
    row=$(manifest_row "$id")
    if [ -z "$row" ]; then
        log "FAIL $id not-in-manifest ($LANGS_FILE)"
        return 1
    fi
    IFS=$'\x1f' read -r id via install pin fixture lang_flag budget extra fallback skip_class \
        skip_reason skip_evidence verified remark <<<"$row"

    if [ -n "$skip_class" ]; then
        log "SKIP $id $skip_class: $skip_reason [$skip_evidence] verified=$verified"
        return 0
    fi

    local t0=$SECONDS
    local work errf
    work=$(mktemp -d) || { log "FAIL $id mktemp"; return 1; }
    errf="$work/stderr.log"

    # fixture → workspace 外（serena 纪律：workspace 内散文件语义层静默返空）。
    local src="$SELF_DIR/smoke_fixtures/$fixture"
    if [ ! -e "$src" ]; then
        log "FAIL $id fixture: missing $fixture"
        rmdir "$work" 2>/dev/null
        return 1
    fi
    local fxdir="$work/fixture" fx
    mkdir -p "$fxdir"
    if [[ "$fixture" == */* ]]; then
        # 工程组形态（首段 = 组根）：整组拷，伴生工程文件（Cargo.toml/csproj/
        # elm.json/mix.exs/Gemfile）随行；组内多级路径保留层级（elixir/lib/smoke.ex
        # ——expert 按标准 mix 布局向上找 mix.exs）。
        cp -r "$SELF_DIR/smoke_fixtures/${fixture%%/*}"/. "$fxdir"/
        fx="$fxdir/${fixture#*/}"
    else
        cp "$src" "$fxdir"/
        fx="$fxdir/$(basename "$src")"
    fi
    # install 行可用 $SERENA_FXDIR（fixture 工作目录）：工程型前置（csharp/fsharp
    # dotnet restore）在 install 阶段对它执行——copy 先于 install，次序即契约。
    export SERENA_FXDIR="$fxdir"
    # ruby-lsp 系 LS 必须有 bundle 上下文才肯活（裸启动即 stdout pump EOF，
    # run 36528495148/36561318948 实锤）：有 Gemfile 就把 bundler 指过去——env 随
    # CLI → lazy-spawn daemon → LS 进程继承。
    if [ -f "$fxdir/Gemfile" ]; then
        export BUNDLE_GEMFILE="$fxdir/Gemfile"
    fi

    # install（budget_secs 包住 install+拉起+探针的 install 侧；超预算 → SKIP BUDGET）
    local rc=0
    case "$via" in
        serena-*)
            case "$via" in
                serena-uvx) ensure_uv || { log "FAIL $id install: uv bootstrap"; rm -rf "$work"; return 1; } ;;
            esac
            if [ -n "$install" ]; then
                # 清单 install 行优先（apt runtime 前置 + $SERENA_CLI install 连写）。
                # 曾无条件走 `install <id>`，前置被静默跳过 —— bsl 跑在 runner 预装
                # JVM 17（条目要求 21）启动即死 LS_TERMINATED，即此坑。
                run_with_timeout "$budget" env SERENA_CLI="$SERENA_CLI" bash -c "$install" >"$work/install.out" 2>"$errf" || rc=$?
            else
                run_with_timeout "$budget" "$SERENA_CLI" install "$id" >"$work/install.out" 2>"$errf" || rc=$?
            fi
            ;;
        *)
            run_with_timeout "$budget" env SERENA_CLI="$SERENA_CLI" bash -c "$install" >"$work/install.out" 2>"$errf" || rc=$?
            ;;
    esac
    log_stderr_if_any "$id" "$errf"
    if [ "$rc" -ne 0 ]; then
        [ "$rc" = 124 ] && log "SKIP $id BUDGET: install exceeded ${budget}s"
        [ "$rc" = 124 ] || log "FAIL $id install: exit=$rc (tail: $(tr '\n' ' ' <"$work/install.out" | cut -c1-300))"
        rm -rf "$work"
        [ "$rc" = 124 ] && return 0
        return 1
    fi

    # 冷启动等待：wait-ready --stage symbol = 底线同判据（overview 首符号非空）的
    # 轮询档，预算吃该门 budget_secs——runner 冷启动链可超 lsp-core 单请求 30s
    # （python/pyright LS_TIMEOUT 实锤）。fallback 门跳过（其底线是登记探针，
    # overview 恒空，等 symbol 无意义——ansible 型）。不用 semantic 档：那是 hover
    # 判据（强于底线），hover 恒空的 LS 会白烧预算。
    if [ -z "$fallback" ]; then
        # 预留 210s 给底线探针的有界重试（上界 5×(30s 单请求 + 10s 间隔) = 200s）
        # ——曾只留 30s，重试链溢进片看门狗会把真 FAIL 伪装成 SKIP BUDGET。
        local remaining=$((budget - (SECONDS - t0) - 210))
        [ "$remaining" -lt 60 ] && remaining=60
        run_with_timeout $((remaining + 30)) "$SERENA_CLI" --project "$fxdir" wait-ready --file "$fx" \
            --lang "${lang_flag:-$id}" --stage symbol --timeout "$remaining" \
            >/dev/null 2>"$errf" || log_stderr_if_any "$id (wait-ready)" "$errf"
    fi

    # 底线断言（--lang 恒显式：变体门不经扩展名抢占，pgsql/mysql 先例）
    BOTTOMLINE_MODE="direct"
    assert_overview "${lang_flag:-$id}" "$fx" "$errf" "$fallback" "$t0" "$budget" || { rm -rf "$work"; return 1; }

    # 加息（登记制，默认无；降级触发的门已验过登记探针，跳过防双份噪音）
    if [ -n "$extra" ] && [ "$BOTTOMLINE_MODE" = "direct" ]; then
        run_extra_assert "${lang_flag:-$id}" "$fx" "$extra" "$errf" || { rm -rf "$work"; log "FAIL $id extra_assert: $extra"; return 1; }
    fi

    "$SERENA_CLI" stop-all >/dev/null 2>&1 || true
    wait_daemon_gone || true
    rm -rf "$work"
    if [ "$BOTTOMLINE_MODE" = "direct" ]; then
        log "PASS $id ($((SECONDS - t0))s via=$via pin=$pin)"
    else
        log "PASS $id (fallback:$BOTTOMLINE_MODE $((SECONDS - t0))s via=$via pin=$pin)"
    fi
}

# ---- 片循环（预算看门狗；任何非 BUDGET 失败 → 片红）----
shard_run() {
    local shard="$1" fail=0 id rc
    local ids
    ids=$("${_PY}" "$SELF_DIR/smoke_shard.py" --ids "$shard" "$LANGS_FILE" | tr -d '\r') || {
        log "FAIL shard shard-plan: smoke_shard.py --ids $shard failed"
        return 1
    }
    local logf
    logf=$(mktemp) || { log "FAIL shard $shard: mktemp"; return 1; }
    for id in $ids; do
        budget=$(manifest_row "$id" | awk -F'\x1f' '{print $7}')
        if has_timeout; then
            timeout $((budget + 120)) "$SELF" _one "$id" 2>&1 | tee -a "$logf"
        else
            "$SELF" _one "$id" 2>&1 | tee -a "$logf"
        fi
        rc=$?
        if [ "$rc" = 124 ]; then
            log "SKIP $id BUDGET: door exceeded $((budget + 120))s watchdog"
        elif [ "$rc" -ne 0 ]; then
            fail=1
        fi
    done
    "$SERENA_CLI" stop-all >/dev/null 2>&1 || true
    wait_daemon_gone || true
    # ── 裁决自检（S9 P0）：片内每门恰一条裁决行（PASS|FAIL|SKIP <id> ...）──
    # 历史缺陷：SKIP 门不进分片计划 → 7 门零裁决行，"FAIL=0" 假绿。行数 != 门数
    # 或某门 0/多行 = 片红（缺行最常见于看门狗掐死在裁决行输出前）。
    # 探针行 id 可为 manifest id 或 lang_flag（php 门 verdict 记 intelephense），
    # 两者都认；每个 id 仍必须恰一条。
    local bad="" n flag pat
    for id in $ids; do
        # manifest_row 字段序：id(1) via(2) install(3) pin(4) fixture(5) lang_flag(6)。
        flag=$(manifest_row "$id" | awk -F'\x1f' '{print $6}')
        pat="${flag:-$id}"
        n=$(grep -cE "^(PASS|FAIL|SKIP) (${id}|${pat})( |$)" "$logf")
        [ "$n" -eq 1 ] || bad="$bad ${id}x${n}"
    done
    rm -f "$logf"
    if [ -n "$bad" ]; then
        log "FAIL shard $shard: verdict audit failed (exactly 1 PASS/FAIL/SKIP line per door):$bad"
        return 1
    fi
    if [ "$fail" -ne 0 ]; then
        log "FAIL shard $shard: at least one door failed (see FAIL lines above)"
        return 1
    fi
    log "DONE shard $shard: all doors PASS/SKIP (verdict lines = doors = $(printf '%s\n' "$ids" | wc -l))"
}

main() {
    find_py || return 1
    # 工具链 bin 补齐：GITHUB_PATH 追加对同 step 不生效（仅跨 step），go 门 gopls
    # 落 $HOME/go/bin 而 runner 默认 PATH 无它 → LS_NOT_INSTALLED（CI 首跑实锤）。
    export PATH="$HOME/.local/bin:$HOME/go/bin:$HOME/.dotnet/tools:$PATH"
    case "${1:-}" in
        _one) one_door "$2" ;;
        --shard)
            [ -n "${2:-}" ] || { log "usage: $0 --shard N"; return 2; }
            shard_run "$2"
            ;;
        -h | --help | "")
            sed -n '2,20p' "${BASH_SOURCE[0]}"
            ;;
        *) one_door "$1" ;;
    esac
}

main "$@"
