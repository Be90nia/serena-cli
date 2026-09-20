import json, subprocess, threading, sys, time

ROOT = r"D:\Project\serena-rust\fixtures\rust_demo"
MAIN = ROOT + r"\main.rs"

proc = subprocess.Popen(
    ["rust-analyzer.exe"],
    cwd=ROOT,
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
)

lock = threading.Lock()
handlers = {}

def send(method, params, id=None):
    obj = {"jsonrpc": "2.0", "method": method, "params": params}
    if id is not None:
        obj["id"] = id
    data = json.dumps(obj).encode()
    with lock:
        proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(data) + data)
        proc.stdin.flush()

def reader():
    f = proc.stdout
    while True:
        headers = {}
        line = f.readline()
        if not line:
            break
        if line in (b"\r\n", b"\n"):
            length = int(headers.get(b"Content-Length", b"0"))
            body = f.read(length)
            try:
                msg = json.loads(body)
            except Exception:
                continue
            if "id" in msg:
                handlers[msg["id"]].append(msg)
        else:
            k, _, v = line.partition(b":")
            headers[k.strip()] = v.strip()

threading.Thread(target=reader, daemon=True).start()

def request(method, params, timeout=30):
    global next_id
    i = next_id
    next_id += 1
    box = handlers.setdefault(i, [])
    send(method, params, id=i)
    t0 = time.time()
    while not box and time.time() - t0 < timeout:
        time.sleep(0.05)
    return box[0] if box else {"TIMEOUT": method}

next_id = 100
send("initialize", {
    "processId": None,
    "rootUri": "file:///D:/Project/serena-rust/fixtures/rust_demo",
    "capabilities": {},
}, id=0)
init_resp = None
t0 = time.time()
while init_resp is None and time.time() - t0 < 30:
    b = handlers.get(0, [])
    if b:
        init_resp = b[0]
        break
    time.sleep(0.05)
print("INIT ok:", bool(init_resp), "serverCaps has definitionProvider:",
      bool(((init_resp or {}).get("result", {}).get("capabilities", {}) or {}).get("definitionProvider")))
send("initialized", {})

text = open(MAIN, encoding="utf-8").read()
send("textDocument/didOpen", {
    "textDocument": {"uri": "file:///D:/Project/serena-rust/fixtures/rust_demo/main.rs",
                     "languageId": "rust", "version": 1, "text": text},
})
time.sleep(3)  # RA indexing window

r = request("textDocument/definition", {
    "textDocument": {"uri": "file:///D:/Project/serena-rust/fixtures/rust_demo/main.rs"},
    "position": {"line": 3, "character": 12},
}, timeout=20)
print("DEF:", json.dumps(r.get("result", r), ensure_ascii=False)[:300])

r = request("textDocument/hover", {
    "textDocument": {"uri": "file:///D:/Project/serena-rust/fixtures/rust_demo/main.rs"},
    "position": {"line": 3, "character": 12},
}, timeout=20)
res = r.get("result", r)
print("HOVER:", (json.dumps(res, ensure_ascii=False)[:200] if res else "null"))

r = request("textDocument/references", {
    "textDocument": {"uri": "file:///D:/Project/serena-rust/fixtures/rust_demo/main.rs"},
    "position": {"line": 3, "character": 12},
    "context": {"includeDeclaration": True},
}, timeout=20)
res = r.get("result", r)
print("REFS:", (str(len(res)) + " hits" if res else "null"))

proc.kill()
