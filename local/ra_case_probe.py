# 参数化探针：隔离 rootUri 大小写 / didOpen URI 大小写 / languageId 三个变量
# 用法: python ra_case_probe.py <root_case: true|lower> <doc_case: true|lower> <lang: rust|cpp>
import json, subprocess, time, sys, os

root_case, doc_case, lang = sys.argv[1], sys.argv[2], sys.argv[3]

TRUE = "D:/Project/serena-rust/fixtures/rust_demo"
ROOT = TRUE.lower() if root_case == "lower" else TRUE
DOC = TRUE if doc_case == "true" else TRUE.lower()

proc = subprocess.Popen(
    ["rust-analyzer"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    cwd=ROOT,
)

def send(msg):
    body = json.dumps(msg).encode()
    proc.stdin.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    proc.stdin.flush()

def read_msg(timeout=40, want_id=None):
    result = {}
    def reader():
        while True:
            hdr = b""
            while not hdr.endswith(b"\r\n\r\n"):
                c = proc.stdout.read(1)
                if not c:
                    return
                hdr += c
            length = int([l for l in hdr.split(b"\r\n") if l.lower().startswith(b"content-length")][0].split(b":")[1])
            body = json.loads(proc.stdout.read(length))
            if want_id is None or body.get("id") == want_id:
                result["body"] = body
                return
    import threading
    t = threading.Thread(target=reader, daemon=True)
    t.start()
    t.join(timeout)
    return result.get("body")

send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
    "processId": os.getpid(),
    "rootUri": "file:///" + ROOT,
    "workspaceFolders":[{"uri": "file:///" + ROOT, "name":"rust_demo"}],
    "capabilities": {},
}})
read_msg(40, want_id=1)
send({"jsonrpc":"2.0","method":"initialized","params":{}})

uri = "file:///" + DOC + "/lib.rs"
text = open(os.path.join(TRUE, "lib.rs"), encoding="utf-8").read()
send({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{
    "uri": uri, "languageId": lang, "version":1, "text": text}}})

time.sleep(15)

send({"jsonrpc":"2.0","id":2,"method":"textDocument/definition","params":{
    "textDocument":{"uri":uri},"position":{"line":0,"character":7}}})
d = read_msg(40, want_id=2)
send({"jsonrpc":"2.0","id":3,"method":"textDocument/hover","params":{
    "textDocument":{"uri":uri},"position":{"line":0,"character":7}}})
h = read_msg(40, want_id=3)

def brief(m):
    if m is None: return "TIMEOUT"
    if "error" in m: return "ERROR:" + str(m["error"].get("message"))[:60]
    r = m.get("result")
    return "null" if r is None else ("[]" if r == [] else "DATA")

print(f"root={root_case} doc={doc_case} lang={lang}  =>  def={brief(d)} hover={brief(h)}")
proc.kill()
subprocess.run(["taskkill","/F","/IM","rust-analyzer.exe"], capture_output=True)
