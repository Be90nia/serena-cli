#!/usr/bin/env python3
"""stress_report.py — 压测 job 末尾汇总（bd serena-rust-2p5）。

用法：stress_report.py <log-dir> <plat> <shard>
读 <log-dir>/cycle-*.md（smoke_one.sh 裁决行 ^PASS|FAIL|SKIP）与
sentinel-c*-*.txt（KEY=VALUE 哨兵），写
<log-dir>/stress-report-<plat>-s<shard>.md。

门清单 = smoke_shard.py 的 LPT 分片（单一事实源 import 复用，不复制装箱）；
裁决行 id 可能是 manifest id 或 lang_flag（php 门记 intelephense），两者都映射回 id。
flaky 判据 = 同一门任一轮 FAIL 之后存在 PASS 轮（跨轮稳定性信号）。
PASS 行含整门耗时 `(... <N>s via=...)`（smoke_one.sh 输出），逐门入
"Per-door wall time" 表（min/median/p95/max；bd serena-rust-8g4 性能基线）。

聚合模式：stress_report.py --aggregate <logs-root> <out.md>
扫 <logs-root>/stress-logs-<plat>-s<N>/cycle-*.md（CI artifact 下载解压形态，
目录名 .+ 非贪婪到末段 -s<N>），出 per-platform per-door 耗时表
（median/p95/min/max）——local/agent-reports/perf-baseline.md 的生成器。
"""

import os
import re
import sys

SELF_DIR = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, SELF_DIR)
import smoke_shard  # noqa: E402

VERDICT_RE = re.compile(r"^(PASS|FAIL|SKIP)\s+(\S+)")
# PASS <id> (<N>s ...) | PASS <id> (fallback:<spec> <N>s ...)
DURATION_RE = re.compile(r"^PASS\s+\S+\s+\((?:fallback:\S+ )?(\d+)s ")
SENTINEL_RE = re.compile(r"^([a-z_]+)=(.*)$")
AGG_DIR_RE = re.compile(r"stress-logs-(.+)-s(\d+)$")


def flag_to_id_map(langs_path):
    """manifest id 与 lang_flag（探针行别名）都映射回 manifest id。"""
    import tomllib

    with open(langs_path, "rb") as f:
        data = tomllib.load(f)
    m = {}
    for e in data["lang"]:
        m.setdefault(e.get("lang_flag") or e["id"], e["id"])
        m.setdefault(e["id"], e["id"])
    return m


def read_durations(cycle_files, to_id, known_doors):
    """cycle-N.md PASS 行耗时 → {door: [secs per cycle]}。"""
    durations = {d: [] for d in known_doors}
    for cyc in sorted(cycle_files):
        with open(cycle_files[cyc], encoding="utf-8", errors="replace") as f:
            for line in f:
                if not line.startswith("PASS"):
                    continue
                m = DURATION_RE.match(line)
                if not m:
                    continue
                door = to_id.get(line.split()[1])
                if door in durations:
                    durations[door].append(int(m.group(1)))
    return durations


def median_p95(xs):
    """最近邻 p95（样本小无插值）；xs 需已排序。"""
    median = xs[len(xs) // 2] if len(xs) % 2 else (xs[len(xs) // 2 - 1] + xs[len(xs) // 2]) // 2
    p95 = xs[min(len(xs) - 1, int(round(0.95 * (len(xs) - 1))))]
    return median, p95


def time_table(durations):
    """per-door 耗时表行（n/min/median/p95/max；无 PASS 样本的门不列）。"""
    lines = []
    samples = []
    for door in sorted(durations):
        xs = sorted(durations[door])
        if not xs:
            continue
        samples.append(xs)
        median, p95 = median_p95(xs)
        lines.append(f"| {door} | {len(xs)} | {xs[0]} | {median} | {p95} | {xs[-1]} |")
    if samples:
        flat = sorted(x for xs in samples for x in xs)
        median, p95 = median_p95(flat)
        lines.append(f"| **all** | {len(flat)} | {flat[0]} | {median} | {p95} | {flat[-1]} |")
    return lines


def main():
    if len(sys.argv) == 4 and sys.argv[1] == "--aggregate":
        return aggregate(sys.argv[2], sys.argv[3])
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    log_dir, plat, shard = sys.argv[1], sys.argv[2], int(sys.argv[3])

    langs_path = os.environ.get("SMOKE_LANGS", os.path.join(SELF_DIR, "smoke_langs.toml"))
    shards = int(os.environ.get("SHARDS", "6"))
    doors = smoke_shard.load_doors(langs_path)
    bins = smoke_shard.assign(doors, shards)
    shard_ids = bins[shard - 1]
    to_id = flag_to_id_map(langs_path)

    # cycle 文件按数字序
    cycle_files = {}
    for path in os.listdir(log_dir):
        m = re.fullmatch(r"cycle-(\d+)\.md", path)
        if m:
            cycle_files[int(m.group(1))] = os.path.join(log_dir, path)
    cycle_files = dict(sorted(cycle_files.items()))

    # verdicts[id] = {cycle: PASS|FAIL|SKIP}
    verdicts = {d: {} for d in shard_ids}
    for cyc, path in cycle_files.items():
        with open(path, encoding="utf-8", errors="replace") as f:
            for line in f:
                m = VERDICT_RE.match(line)
                if not m:
                    continue
                door = to_id.get(m.group(2))
                if door in verdicts and cyc not in verdicts[door]:
                    verdicts[door][cyc] = m.group(1)

    # 哨兵趋势
    sentinels = {}
    for path in os.listdir(log_dir):
        m = re.fullmatch(r"sentinel-c(\d+)-(before|after)\.txt", path)
        if not m:
            continue
        kv = {}
        with open(os.path.join(log_dir, path), encoding="utf-8") as f:
            for line in f:
                s = SENTINEL_RE.match(line.strip())
                if s:
                    kv[s.group(1)] = s.group(2)
        sentinels[(int(m.group(1)), m.group(2))] = kv

    n_cycles = len(cycle_files)
    lines = []
    ap = lines.append
    ap(f"# LS Stress Report — {plat} shard {shard}")
    ap("")
    ap(f"- cycles: {n_cycles} (cycle files: {', '.join(f'c{c}' for c in sorted(cycle_files)) or '—'})")
    ap(f"- doors in shard: {len(shard_ids)}")
    ap("")
    ap("## Verdict matrix")
    ap("")
    header = "| door | " + " | ".join(f"c{c}" for c in sorted(cycle_files)) + " | summary |"
    ap(header)
    ap("|" + "---|" * (n_cycles + 2))
    flaky, persistent, pass_total = [], [], 0
    for d in shard_ids:
        v = verdicts[d]
        cells = [v.get(c, "-") for c in sorted(cycle_files)]
        n_pass = sum(1 for x in cells if x == "PASS")
        n_fail = sum(1 for x in cells if x == "FAIL")
        n_skip = sum(1 for x in cells if x == "SKIP")
        pass_total += n_pass
        summary = f"{n_pass}P/{n_fail}F/{n_skip}S"
        if n_pass + n_fail == 0 and n_skip == 0:
            summary += " **NO-VERDICT**"
        # flaky：任一轮 FAIL 之后存在 PASS 轮
        cycles_sorted = sorted(cycle_files)
        fail_seen = False
        for c in cycles_sorted:
            if v.get(c) == "FAIL":
                fail_seen = True
            elif v.get(c) == "PASS" and fail_seen:
                flaky.append((d, c))
                break
        if n_fail and v.get(cycles_sorted[-1]) == "FAIL":
            persistent.append(d)
        ap(f"| {d} | " + " | ".join(cells) + f" | {summary} |")
    ap("")
    ap(f"PASS cell total: {pass_total} / ({len(shard_ids)} doors × {n_cycles} cycles)")
    ap("")

    # per-door 整门耗时（PASS 行 `<N>s`；smoke_one.sh 已输出，此处解析——8g4 性能基线）
    durations = read_durations(cycle_files, to_id, shard_ids)
    time_rows = time_table(durations)
    ap("## Per-door wall time (s)")
    ap("")
    if time_rows:
        ap("| door | n | min | median | p95 | max |")
        ap("|---|---|---|---|---|---|")
        lines.extend(time_rows)
    else:
        ap("（无 PASS 样本）")
    ap("")
    ap("## Flaky (FAIL → later PASS)")
    ap("")
    if flaky:
        for d, c in flaky:
            ap(f"- {d}: FAIL before cycle {c}, PASS at cycle {c}")
    else:
        ap("（无）")
    ap("")
    ap("## Persistent FAIL (FAIL in last cycle)")
    ap("")
    if persistent:
        for d in persistent:
            ap(f"- {d}")
    else:
        ap("（无）")
    ap("")
    ap("## Sentinel trend (before → after, per cycle)")
    ap("")
    ap("| cycle | ls_procs | daemon_procs | daemon_rss_kb | lock_files | cache_mb |")
    ap("|---|---|---|---|---|---|")
    for c in sorted(cycle_files):
        b = sentinels.get((c, "before"), {})
        a = sentinels.get((c, "after"), {})

        def cell(key):
            return f"{b.get(key, '?')} → {a.get(key, '?')}"

        ap(
            f"| c{c} | {cell('ls_procs')} | {cell('daemon_procs')} | {cell('daemon_rss_kb')} "
            f"| {cell('lock_files')} | {cell('cache_mb')} |"
        )
    ap("")

    out = os.path.join(log_dir, f"stress-report-{plat}-s{shard}.md")
    with open(out, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(lines))
    print(out)


def aggregate(logs_root, out_path):
    """--aggregate：跨 shard/platform 合并 per-door 耗时 → perf 基线表。

    目录形态 = CI artifact 下载解压名 stress-logs-<plat>-s<N>（plat 含 '-'，
    贪婪到末段 -s<N>）。每平台一张表 + 门级样本说明；文件 = local/agent-reports/
    perf-baseline.md 的机器生成节。
    """
    langs_path = os.environ.get("SMOKE_LANGS", os.path.join(SELF_DIR, "smoke_langs.toml"))
    to_id = flag_to_id_map(langs_path)
    all_doors = [lid for _, lid in smoke_shard.load_doors(langs_path)]

    per_plat = {}  # plat -> {door: [secs]}
    for name in sorted(os.listdir(logs_root)):
        m = AGG_DIR_RE.fullmatch(name)
        if not m or not os.path.isdir(os.path.join(logs_root, name)):
            continue
        plat = m.group(1)
        cycle_files = {}
        d = os.path.join(logs_root, name)
        for p in os.listdir(d):
            cm = re.fullmatch(r"cycle-(\d+)\.md", p)
            if cm:
                cycle_files[int(cm.group(1))] = os.path.join(d, p)
        if not cycle_files:
            continue
        acc = per_plat.setdefault(plat, {})
        for door, xs in read_durations(cycle_files, to_id, all_doors).items():
            acc.setdefault(door, []).extend(xs)

    lines = [
        "# Per-door wall time — cross-shard aggregate (s)",
        "",
        "来源 = stress-ls 各 (platform, shard) job 的 cycle-*.md PASS 行整门耗时",
        "（install + 首启 + 探针全含）。生成："
        "`stress_report.py --aggregate <artifacts-root> <out.md>`。",
        "",
    ]
    for plat in sorted(per_plat):
        ap = lines.append
        ap(f"## {plat}")
        ap("")
        rows = time_table(per_plat[plat])
        if rows:
            ap("| door | n | min | median | p95 | max |")
            ap("|---|---|---|---|---|---|")
            lines.extend(rows)
        else:
            ap("（无 PASS 样本）")
        ap("")
    if not per_plat:
        lines.append("（logs-root 下无 stress-logs-<plat>-s<N>/cycle-*.md）")
        lines.append("")
    with open(out_path, "w", encoding="utf-8", newline="\n") as f:
        f.write("\n".join(lines))
    print(out_path)
    return 0


if __name__ == "__main__":
    main()
