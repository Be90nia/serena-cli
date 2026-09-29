#!/usr/bin/env python3
"""smoke_shard.py — 45 门冒烟矩阵的预算加权分片（单一事实源）。

两用（同一装箱算法，workflow 与 smoke_one.sh 共享，分片结果必然一致）：
  smoke_shard.py --matrix [LANGS]   # stdout: GitHub matrix JSON {"shard":[1..N]}
  smoke_shard.py --ids N [LANGS]    # stdout: 第 N 片的 id 列表（每行一个）

装箱：LPT（budget_secs 降序，逐个放入当前总预算最轻的片）。SKIP 门不占片。
片数 SHARDS 环境变量（默认 6）。同清单 → 同分片（稳定）。
"""

import json
import os
import sys

DEFAULT_LANGS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "smoke_langs.toml")
ACTIVE_FIELDS = ("id", "budget_secs", "skip_class")


def load_active(langs_path):
    """返回非 SKIP 门 [(budget, id)]（budget 降序稳定排序）。"""
    import tomllib

    with open(langs_path, "rb") as f:
        data = tomllib.load(f)
    active = [
        (int(e["budget_secs"]), e["id"]) for e in data["lang"] if not e.get("skip_class")
    ]
    return sorted(active, key=lambda t: (-t[0], t[1]))


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
    if not args or args[0] not in ("--matrix", "--ids"):
        sys.exit(__doc__)
    mode = args[0]
    if mode == "--ids" and len(args) < 2:
        sys.exit("--ids requires a shard number")
    # Windows 文本模式会把 \n 转 \r\n，污染 bash 侧 id 词分割（id 尾带 \r 匹配失败）。
    sys.stdout.reconfigure(newline="\n")
    langs = args[2] if len(args) > 2 else DEFAULT_LANGS
    shards = int(os.environ.get("SHARDS", "6"))

    active = load_active(langs)
    bins = assign(active, shards)

    if mode == "--matrix":
        print(json.dumps({"shard": list(range(1, shards + 1))}))
    else:
        n = int(args[1])
        if not 1 <= n <= shards:
            sys.exit(f"shard {n} outside 1..{shards}")
        print("\n".join(bins[n - 1]))


if __name__ == "__main__":
    main()
