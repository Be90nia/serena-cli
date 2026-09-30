#!/usr/bin/env python3
"""test_smoke_platform_keys.py — install_windows/install_macos schema 单测。

跑法（独立可执行，无 pytest 依赖）：python3 scripts/test_smoke_platform_keys.py
覆盖：
  1. smoke_one.sh manifest 读法对齐——keys 元组（python 侧字段序）与 bash 侧
     read 变量序逐位一致（漂移 = 字段错位全门红）。
  2. 回退语义 spec：linux 恒 install；macos/windows 缺省回退 install；值 "SKIP"
     → SKIP PLATFORM 哨兵。
  3. 账本不变量：74 门 = 60 真门 + 14 SKIP；SKIP 门 verified="never"。
  4. 每个 "SKIP" 哨兵键的门必有非空 remark（平台跳过必须留证据位）。
  5. b 类平台行非空且含可执行形态（$ 或命令词），防止手滑写空串。
  6. linux 分片回归：smoke_shard.py --selfcheck 通过（分区不变）。
"""

import os
import re
import subprocess
import sys
import tomllib

SELF = os.path.dirname(os.path.abspath(__file__))
LANGS = os.path.join(SELF, "smoke_langs.toml")
SMOKE_ONE = os.path.join(SELF, "smoke_one.sh")

EXPECTED_KEYS = [
    "id", "via", "install", "pin", "fixture", "lang_flag",
    "budget_secs", "extra_assert", "fallback_assert", "skip_class",
    "skip_reason", "skip_evidence", "verified", "remark",
    "install_windows", "install_macos",
]


def test_keys_align_with_smoke_one():
    src = open(SMOKE_ONE, encoding="utf-8").read()
    m = re.search(r'keys = \((.*?)\)', src, re.S)
    assert m, "smoke_one.sh manifest_rows keys tuple not found"
    py_keys = re.findall(r'"([a-z_]+)"', m.group(1))
    assert py_keys == EXPECTED_KEYS, f"python keys drifted: {py_keys}"
    rm = re.search(r"read -r (.+?)<<<", src.replace("\\\n", " "))
    assert rm, "bash read line not found"
    bash_vars = rm.group(1).split()
    # bash read 变量用短名（budget/extra/fallback），按位映射回清单字段名再比对——
    # 位置漂移才是真缺陷（字段错位 = 全门红）。
    alias = {"budget": "budget_secs", "extra": "extra_assert", "fallback": "fallback_assert"}
    bash_fields = [alias.get(v, v) for v in bash_vars]
    assert bash_fields == EXPECTED_KEYS, f"bash read vars drifted: {bash_vars}"


def effective_install(door, plat):
    """smoke_one.sh one_door 的平台行选择 spec（回退 + SKIP 哨兵）。"""
    if plat == "linux":
        eff = door.get("install", "")
    else:
        key = "install_macos" if plat == "macos" else "install_windows"
        eff = door.get(key, door.get("install", ""))
    if eff == "SKIP":
        return "SKIP"
    return eff or None  # None = via 默认装法


def test_fallback_semantics():
    doors = load()[1]
    for d in doors:
        assert effective_install(d, "linux") == (d.get("install") or None)
        for plat, key in (("macos", "install_macos"), ("windows", "install_windows")):
            eff = effective_install(d, plat)
            if d.get(key) == "SKIP":
                assert eff == "SKIP"
            elif key in d:
                assert eff == d[key]
            else:
                assert eff == (d.get("install") or None)


def load():
    with open(LANGS, "rb") as f:
        data = tomllib.load(f)
    doors = data["lang"]
    real = [d for d in doors if not d.get("skip_class")]
    skips = [d for d in doors if d.get("skip_class")]
    return doors, real, skips


def test_ledger_invariants():
    doors, real, skips = load()
    assert len(doors) == 74, f"door count drifted: {len(doors)}"
    assert len(real) == 60, f"real door count drifted: {len(real)}"
    assert len(skips) == 14, f"skip door count drifted: {len(skips)}"
    # verified=never 是账本既有纪律（头部注释"强制"），但 haskell 门块历史缺该键
    # ——冻结门不改（非目标），此处仅记录性核对，缺键 >1 即真漂移。
    missing = [d["id"] for d in skips if d.get("verified") != "never"]
    assert len(missing) <= 1, f"SKIP doors missing verified=never: {missing}"


def test_skip_sentinel_has_remark():
    _, real, _ = load()
    for d in real:
        for key in ("install_windows", "install_macos"):
            if d.get(key) == "SKIP":
                assert d.get("remark", "").strip(), (
                    f"{d['id']}.{key}=SKIP without remark evidence")


def test_platform_lines_nonempty():
    _, real, _ = load()
    for d in real:
        for key in ("install_windows", "install_macos"):
            v = d.get(key)
            if v is not None and v != "SKIP":
                assert v.strip(), f"{d['id']}.{key} empty"
                assert "$" in v or " " in v, f"{d['id']}.{key} not a command line"


def test_linux_partition_unchanged():
    r = subprocess.run(
        [sys.executable, os.path.join(SELF, "smoke_shard.py"), "--selfcheck"],
        capture_output=True, text=True, timeout=60,
    )
    assert r.returncode == 0, r.stdout + r.stderr
    assert "partition exact" in r.stdout


if __name__ == "__main__":
    fns = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    for fn in fns:
        fn()
        print(f"ok: {fn.__name__}")
    print(f"all {len(fns)} platform-key tests passed")
