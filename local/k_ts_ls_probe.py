"""裸 LSP 探针：实锤 typescript-language-server 的 workspace/symbol 是否返空。
initialize → initialized → workspace/symbol("calc") → 打印响应帧 → exit。
"""
import json
import subprocess
import threading
import sys

proc = subprocess.Popen(
    [r"C:\Users\Begonia\AppData\Roaming\npm\typescript-language-server.cmd", "--stdio"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.DEVNULL,
    cwd="fixtures/typescript_demo",
)
frames = []
lock = threading.Lock()


def reader():
    while True:
        # 标准 LSP 分帧：逐行读 header 直到空行，再按 Content-Length 读 body。
        content_length = None
        while True:
            line = proc.stdout.readline()
            if not line:
                return
            if line in (b"\r\n", b"\n"):
                if content_length is not None:
                    break
                continue
            if b":" in line:
                k, v = line.decode(errors="replace").split(":", 1)
                if k.strip().lower() == "content-length":
                    content_length = int(v.strip())
        if content_length is None:
            return
        body = proc.stdout.read(content_length)
        with lock:
            frames.append(json.loads(body))


threading.Thread(target=reader, daemon=True).start()


def send(msg):
    data = json.dumps(msg).encode()
    proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
    proc.stdin.flush()


def wait_id(rid, timeout=25):
    import time
    t0 = time.time()
    while time.time() - t0 < timeout:
        with lock:
            for f in frames:
                if f.get("id") == rid and ("result" in f or "error" in f):
                    return f
        time.sleep(0.1)
    return None


send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
    "processId": None, "rootUri": "file:///D:/Project/serena-rust/fixtures/typescript_demo",
    "capabilities": {}}})
wait_id(1)
send({"jsonrpc": "2.0", "method": "initialized", "params": {}})
import time
time.sleep(3)  # 等 TS project load
send({"jsonrpc": "2.0", "id": 2, "method": "workspace/symbol", "params": {"query": "calc"}})
r = wait_id(2)
if r is None:
    print("RESULT: no response within 60s")
elif r.get("error"):
    print("RESULT: error:", r["error"])
else:
    res = r["result"]
    print("RESULT: workspace/symbol returned", len(res) if isinstance(res, list) else res)
    if isinstance(res, list):
        for s in res[:5]:
            print("  -", s.get("name"), s.get("location", {}).get("uri"))
send({"jsonrpc": "2.0", "id": 3, "method": "shutdown"})
wait_id(3)
send({"jsonrpc": "2.0", "method": "exit"})
proc.wait(timeout=10)
