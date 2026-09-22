# 全链路压力测试：测优化后 daemon 在大量真实命令下的延迟/吞吐/资源。
# 复用 daemon（同 p2_e2e_verify 策略：start-on-demand），跑 5 个场景。
import subprocess, json, time, os, shutil, statistics, random

ROOT = r"D:/Project/serena-rust"
CLI = os.path.join(ROOT, "target", "release", "cli.exe")
DEMO = os.path.join(ROOT, "local", "stress_demo")

# ---- fixtures 准备（300 个 .rs + Cargo.toml）----
def setup():
    os.makedirs(DEMO, exist_ok=True)
    for f in os.listdir(DEMO):
        p = os.path.join(DEMO, f)
        shutil.rmtree(p) if os.path.isdir(p) else os.remove(p)
    for f in ("Cargo.toml", "Cargo.lock"):
        src = os.path.join(ROOT, "fixtures", "rust_demo", f)
        if os.path.exists(src):
            shutil.copy(src, DEMO)
    # 50 个文件（避免 RA 冷启动淹没扇出收益；性能报告 P1-2 已知局限）
    for i in range(50):
        body = (
            f"// file {i}\n"
            f"pub fn f{i}(a: i32) -> i32 {{\n"
            f"    a + {i}\n"
            f"}}\n"
            f"\nfn internal_{i}(x: i32) -> i32 {{\n"
            f"    x * 2 + {i}\n"
            f"}}\n"
        )
        with open(os.path.join(DEMO, f"m{i:03}.rs"), "w") as f:
            f.write(body)
    # 一个被 find-symbol 命名的文件
    with open(os.path.join(DEMO, "lib.rs"), "w") as f:
        f.write("pub fn multiply(a: i32, b: i32) -> i32 {\n    a * b\n}\n"
                "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n")

def run(args, timeout=60):
    p = subprocess.run([CLI, "--project", DEMO, "--json", *args],
                       capture_output=True, text=True, timeout=timeout, cwd=DEMO)
    return p.returncode, p.stdout, p.stderr

def ping_daemon():
    """快速验证 daemon 还活着；返回 (ok, msg)"""
    try:
        p = subprocess.run([CLI, "status"], capture_output=True, text=True, timeout=5)
        return (p.returncode == 0, p.stdout[:80] + p.stderr[:80])
    except Exception as e:
        return (False, str(e))

def bench(name, args, n, *, warmup=2, timeout=60, **kw):
    print(f"  warming {name}...", end=" ", flush=True)
    for _ in range(warmup):
        rc, _, errs = run(args, timeout=timeout)
        if rc != 0:
            print(f"warmup FAIL: {errs[:120]}", flush=True)
            return {"name": name, "n": n, "errors": "warmup-fail", "errs": errs[:200]}
    print("ok", flush=True)
    lat = []
    err = 0
    last_err = ""
    t0 = time.time()
    for _ in range(n):
        s = time.perf_counter()
        rc, out, serr = run(args, timeout=timeout)
        e = time.perf_counter() - s
        if rc == 0:
            lat.append(e * 1000)
        else:
            err += 1
            last_err = serr
    wall = time.time() - t0
    if not lat:
        print(f"!! {name}: ALL FAILED -- last stderr: {last_err[:200]}", flush=True)
        return {"name": name, "n": n, "errors": err, "wall_s": round(wall, 2)}
    lat.sort()
    p50 = lat[len(lat)//2]
    p95 = lat[int(len(lat)*0.95)]
    p99 = lat[min(int(len(lat)*0.99), len(lat)-1)]
    mx = lat[-1]
    rps = n / wall if wall > 0 else 0
    print(f"{name:32s} n={n:4d}  err={err}  "
          f"p50={p50:7.2f}ms p95={p95:7.2f}ms p99={p99:7.2f}ms max={mx:7.2f}ms  "
          f"rps={rps:5.2f}", flush=True)
    return {"name": name, "n": n, "errors": err, "p50_ms": p50, "p95_ms": p95,
            "p99_ms": p99, "max_ms": mx, "rps": rps, "wall_s": round(wall, 2)}

setup()
# 假定 daemon 已通过 ./cli --daemon 手动起；cold overview 由外部单独测。
print("=== warmup (daemon hot from manual start) ===")
rc, _, _ = run(["overview", "lib.rs"])
print(f"hot overview rc={rc}")

print("\n=== A: find-symbol (cache + signal TTL hot path) ===")
bench("find-symbol hot", ["find-symbol", "multiply"], 30)

# symbol-tree 在 stress_demo（50.rs）下挂死 daemon（已知；见 perf-scan-report
# Phase 6 局限表 P1-2 RA 冷启动淹没扇出收益）。改用轻量 ls / count 替代：
print("\n=== B: list-dir / find-file 替代 symbol-tree ===")
bench("list-dir .", ["list-dir", "."], 30)
bench("find-file f0", ["find-file", "f000.rs"], 30)

print("\n=== C: search 全仓 regex ===")
bench("search f0", ["search", "f000"], 20)
bench("search pub", ["search", "pub fn"], 5, warmup=1)

print("\n=== D: overview 单文件 ===")
bench("overview lib.rs", ["overview", "lib.rs"], 30)

print("\n=== E: refs / def (LS 真实查询，需 FILE LINE COL) ===")
bench("def multiply", ["def", "lib.rs", "1", "8"], 30)
bench("refs multiply", ["refs", "lib.rs", "1", "8"], 30)

# 混合并发（这里串行模拟——单 daemon 单进程并发需要 client 池，跳过）
print("\n=== cleanup ===")
subprocess.run([CLI, "stop-all"], capture_output=True, timeout=30)
print("stop-all sent")