# A 段 rust_demo 全命令 e2e（--direct, --json） 2026-09-20 21:25:26
### overview
```
[
  {
    "container": "add",
    "kind": "Function",
    "name": "add",
    "range": {
      "end": {
        "character": 1,
        "line": 2
      },
      "start": {
        "character": 0,
        "line": 0
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/rust_demo/lib.rs"
  },
  {
    "container": "multiply",
    "kind": "Function",
    "name": "multiply",
    "range": {
      "end": {
        "character": 1,
        "line": 6
      },
      "start": {
        "character": 0,
        "line": 4
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/rust_demo/lib.rs"
  }
]
rc=0 ms=1286
```

### symbol-tree
```
{
  "dir": "D:/Project/serena-rust/fixtures/rust_demo",
  "entries": [
    {
      "file": "lib.rs",
      "symbols": [
        {
          "container": "add",
          "kind": "Function",
          "name": "add",
          "range": {
            "end": {
              "character": 1,
              "line": 2
            },
            "start": {
              "character": 0,
              "line": 0
            }
          },
          "uri": "file:///D:/Project/serena-rust/fixtures/rust_demo/lib.rs"
        },
        {
          "container": "multiply",
          "kind": "Function",
          "name": "multiply",
          "range": {
            "end": {
              "character": 1,
              "line": 6
            },
            "start": {
              "character": 0,
              "line": 4
            }
          },
          "uri": "file:///D:/Project/serena-rust/fixtures/rust_demo/lib.rs"
        }
      ]
    },
    {
      "file": "main.rs",
      "symbols": [
        {
          "container": "add",
          "kind": "Function",
          "name": "add",
          "range": {
            "end": {
              "character": 45,
              "line": 0
            },
            "start": {
              "character": 0,
              "line": 0
            }
          },
          "uri": "file:///D:/Project/serena-rust/fixtures/rust_demo/main.rs"
        },
        {
          "container": "main",
          "kind": "Function",
          "name": "main",
          "range": {
            "end": {
              "character": 1,
              "line": 5
            },
            "start": {
              "character": 0,
              "line": 2
            }
          },
          "uri": "file:///D:/Project/serena-rust/fixtures/rust_demo/main.rs"
        }
      ]
    }
  ],
  "errors": [],
  "files_scanned": 2,
  "truncated": false
}
rc=0 ms=50
```

### find-symbol
```
[]
rc=0 ms=57
```

### def
```
null
rc=0 ms=58
```

### refs
```
[]
rc=0 ms=51
```

### find-impl
```
[]
rc=0 ms=89
```

### hover
```
null
rc=0 ms=45
```

### diagnostics
```
{
  "items": []
}
rc=0 ms=44
```

### symbol-body
```
"pub fn multiply(a: i32, b: i32) -> i32 {\r\n    a * b\r\n}"
rc=0 ms=43
```

### containing
```
tool error: {"code":"RPC_ERROR","message":"rpc -1: response decode failed: data did not match any variant of untagged enum DocumentSymbolResponse","retryable":false}
rc=1 ms=50
```

### defining
```
null
rc=0 ms=49
```

### find-ref-syms
```
[]
rc=0 ms=46
```

### find-ref-snips
```
[]
rc=0 ms=44
```

### search
```
{
  "files_scanned": 3,
  "hits": [
    {
      "col": 5,
      "file": "lib.rs",
      "line": 1,
      "match_end": 10,
      "match_start": 4,
      "text": "pub fn add(a: i32, b: i32) -> i32 {"
    },
    {
      "col": 1,
      "file": "main.rs",
      "line": 1,
      "match_end": 6,
      "match_start": 0,
      "text": "fn add(a: i32, b: i32) -> i32 { a + b + 999 }"
    }
  ],
  "truncated": false
}
rc=0 ms=69
```

### read-file
```
daemon on :7860 not ready within 10s
rc=3 ms=10086
```

### list-dir
```
daemon on :7860 not ready within 10s
rc=3 ms=10142
```

### find-file
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=40
```

### sig-help
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=42
```

### doc-highlight
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=40
```

### folding-range
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=38
```

### semantic-tokens
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=42
```

### inlay-hint
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=45
```

### code-action
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=40
```

### format-clean
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=48
```

### diagnostics-err
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=41
```

### completion
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=87
```

### format-dirty
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=42
```

### ch-prepare
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=41
```

### call-hierarchy: prepare 未返回 item，incoming/outgoing 跳过
### replace-body
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=39
```

### replace-text
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=45
```

### insert-before
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=41
```

### insert-after
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=42
```

### delete-text
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=42
```

### safe-delete-ok
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=44
```

### safe-delete-ref
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=45
```

### insert-at-line
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=42
```

### replace-lines
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=40
```

### delete-lines
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=42
```

### rename
```
daemon transport error 403 Forbidden: {"error":{"code":"FORBIDDEN","message":"missing or invalid X-Serena-Token"},"ok":false}
rc=3 ms=52
```

RENAME_CHECK adder in main.rs: 0
DONE
