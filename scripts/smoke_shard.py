#!/usr/bin/env python3
"""smoke_shard.py — 45 门冒烟矩阵的预算加权分片（单一事实源）。

两用（同一装箱算法，workflow 与 smoke_one.sh 共享，分片结果必然一致）：
  smoke_shard.py --matrix [LANGS]   # stdout: GitHub matrix JSON {"shard":[1..N]}
  smoke_shard.py --ids N [LANGS]    # stdout: 第 N 片的 id 列表（每行一个）

装箱：LPT（budget_secs 降序，逐个放入当前总预算最轻的片）。SKIP 门以 0 预算
参与装箱（不占预算，但必须落到某一片输出裁决行——曾被排除在计划外，7 门零
裁决行 = "FAIL=0" 假绿，S9 P0）。片数 SHARDS 环境变量（默认 6）。同清单 →
同分片（稳定）。--selfcheck 离线自检：全部门恰落在一片、无遗漏无重复。
"""

import json
import os
import sys

DEFAULT_LANGS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "smoke_langs.toml")
ACTIVE_FIELDS = ("id", "budget_secs", "skip_class")


def load_doors(langs_path):
    """返回全部门 [(budget, id)]（budget 降序稳定排序）。

    SKIP 门 budget 取 0 参与 LPT：排序自然落在所有真门之后，装箱不占预算，
    但保证分片计划覆盖清单里每一门（smoke_one.sh 据此输出 SKIP 裁决行）。

    ci_only 门（`ci_only = true`）默认从本地矩阵排除；CI workflow 设
    `SERENA_CI_ONLY_INCL=1` 时纳入全集（r8.5 adopt：PHP Devsense 等实验门
    本机不跑，CI 才跑；保持本地 smoke 矩阵绿色不污染）。
    """
    import tomllib

    with open(langs_path, "rb") as f:
        data = tomllib.load(f)
    doors = []
    for e in data["lang"]:
        if e.get("ci_only") and os.environ.get("SERENA_CI_ONLY_INCL") != "1":
            continue  # ci_only 门本地排除；CI workflow 设 env 启用
        doors.append((int(e.get("budget_secs", 0)), e["id"]))
    return sorted(doors, key=lambda t: (-t[0], t[1]))


def assign(active, shards):
    """LPT 装箱：返回 shard_no(1-based) -> [id]。"""
    bins = [[] for _ in range(shards)]
    loads = [0] * shards
    for budget, lid in active:
        i = loads.index(min(loads))
        bins[i].append(lid)
        loads[i] += budget
    return bins


def main():
    args = sys.argv[1:]
    if not args or args[0] not in ("--matrix", "--ids", "--selfcheck"):
        sys.exit(__doc__)
    mode = args[0]
    if mode == "--ids" and len(args) < 2:
        sys.exit("--ids requires a shard number")
    # Windows 文本模式会把 \n 转 \r\n，污染 bash 侧 id 词分割（id 尾带 \r 匹配失败）。
    sys.stdout.reconfigure(newline="\n")
    langs = args[2] if len(args) > 2 else DEFAULT_LANGS
    shards = int(os.environ.get("SHARDS", "6"))

    doors = load_doors(langs)
    bins = assign(doors, shards)

    if mode == "--selfcheck":
        from collections import Counter

        expected = Counter(lid for _, lid in doors)
        placed = Counter(lid for b in bins for lid in b)
        if placed != expected:
            missing = expected - placed
            extra = placed - expected
            sys.exit(f"selfcheck FAILED: missing={dict(missing)} extra={dict(extra)}")
        print(
            f"selfcheck ok: {sum(expected.values())} doors across {shards} shards, partition exact"
        )
        return
    if mode == "--matrix":
        print(json.dumps({"shard": list(range(1, shards + 1))}))
    else:
        n = int(args[1])
        if not 1 <= n <= shards:
            sys.exit(f"shard {n} outside 1..{shards}")
        print("\n".join(bins[n - 1]))


if __name__ == "__main__":
    main()
