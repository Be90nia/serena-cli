# A 段 rust_demo 全命令 e2e（--direct, --json） 2026-09-21 01:50:32
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
rc=0 ms=735
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
rc=0 ms=38
```

### find-symbol
```
[]
rc=0 ms=39
```

### def
```
null
rc=0 ms=40
```

### refs
```
[]
rc=0 ms=38
```

### find-impl
```
[]
rc=0 ms=38
```

### hover
```
null
rc=0 ms=38
```

### diagnostics
```
{
  "items": []
}
rc=0 ms=38
```

### symbol-body
```
"pub fn multiply(a: i32, b: i32) -> i32 {\r\n    a * b\r\n}"
rc=0 ms=36
```

### containing
```
tool error: {"code":"RPC_ERROR","message":"rpc -1: response decode failed: data did not match any variant of untagged enum DocumentSymbolResponse","retryable":false}
rc=1 ms=43
```

### defining
```
null
rc=0 ms=43
```

### find-ref-syms
```
[]
rc=0 ms=46
```

### find-ref-snips
```
[
  {
    "col": 12,
    "file": "main.rs",
    "line": 3,
    "snippet": "fn add(a: i32, b: i32) -> i32 { a + b + 999 }\n\nfn main() {\n    let s = add(1, 2);\n    println!(\"{s}\");\n}",
    "text": "    let s = add(1, 2);"
  },
  {
    "col": 3,
    "file": "main.rs",
    "line": 0,
    "snippet": "fn add(a: i32, b: i32) -> i32 { a + b + 999 }\n\nfn main() {\n    let s = add(1, 2);",
    "text": "fn add(a: i32, b: i32) -> i32 { a + b + 999 }"
  }
]
rc=0 ms=83
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
rc=0 ms=41
```

### read-file
```
{
  "content": "pub fn add(a: i32, b: i32) -> i32 {\n    a + b",
  "end_line": 2,
  "hash": "f3f327a8c2cb3700",
  "start_line": 1,
  "total_lines": 7
}
rc=0 ms=37
```

### list-dir
```
[
  {
    "is_dir": false,
    "path": "Cargo.toml",
    "size": 209
  },
  {
    "is_dir": false,
    "path": "lib.rs",
    "size": 107
  },
  {
    "is_dir": false,
    "path": "main.rs",
    "size": 111
  }
]
rc=0 ms=40
```

### find-file
```
[
  "main.rs"
]
rc=0 ms=39
```

### sig-help
```
{
  "activeParameter": 0,
  "activeSignature": 0,
  "signatures": [
    {
      "activeParameter": 0,
      "label": "fn add(a: i32, b: i32) -> i32",
      "parameters": [
        {
          "label": "a: i32"
        },
        {
          "label": "b: i32"
        }
      ]
    }
  ]
}
rc=0 ms=41
```

### doc-highlight
```
[
  {
    "range": {
      "end": {
        "character": 15,
        "line": 3
      },
      "start": {
        "character": 12,
        "line": 3
      }
    }
  },
  {
    "range": {
      "end": {
        "character": 6,
        "line": 0
      },
      "start": {
        "character": 3,
        "line": 0
      }
    }
  }
]
rc=0 ms=38
```

### folding-range
```
[
  {
    "endCharacter": 1,
    "endLine": 2,
    "startCharacter": 34,
    "startLine": 0
  },
  {
    "endCharacter": 1,
    "endLine": 6,
    "startCharacter": 39,
    "startLine": 4
  }
]
rc=0 ms=38
```

### semantic-tokens
```
{
  "resultId": "1",
  "tokens": [
    {
      "length": 3,
      "line": 0,
      "startChar": 0,
      "tokenModifiers": 0,
      "tokenType": 6
    },
    {
      "length": 2,
      "line": 0,
      "startChar": 4,
      "tokenModifiers": 0,
      "tokenType": 6
    },
    {
      "length": 3,
      "line": 0,
      "startChar": 7,
      "tokenModifiers": 524292,
      "tokenType": 4
    },
    {
      "length": 1,
      "line": 0,
      "startChar": 11,
      "tokenModifiers": 4,
      "tokenType": 12
    },
    {
      "length": 3,
      "line": 0,
      "startChar": 14,
      "tokenModifiers": 0,
      "tokenType": 28
    },
    {
      "length": 1,
      "line": 0,
      "startChar": 19,
      "tokenModifiers": 4,
      "tokenType": 12
    },
    {
      "length": 3,
      "line": 0,
      "startChar": 22,
      "tokenModifiers": 0,
      "tokenType": 28
    },
    {
      "length": 2,
      "line": 0,
      "startChar": 27,
      "tokenModifiers": 0,
      "tokenType": 11
    },
    {
      "length": 3,
      "line": 0,
      "startChar": 30,
      "tokenModifiers": 0,
      "tokenType": 28
    },
    {
      "length": 1,
      "line": 1,
      "startChar": 4,
      "tokenModifiers": 0,
      "tokenType": 12
    },
    {
      "length": 1,
      "line": 1,
      "startChar": 6,
      "tokenModifiers": 0,
      "tokenType": 11
    },
    {
      "length": 1,
      "line": 1,
      "startChar": 8,
      "tokenModifiers": 0,
      "tokenType": 12
    },
    {
      "length": 3,
      "line": 4,
      "startChar": 0,
      "tokenModifiers": 0,
      "tokenType": 6
    },
    {
      "length": 2,
      "line": 4,
      "startChar": 4,
      "tokenModifiers": 0,
      "tokenType": 6
    },
    {
      "length": 8,
      "line": 4,
      "startChar": 7,
      "tokenModifiers": 524292,
      "tokenType": 4
    },
    {
      "length": 1,
      "line": 4,
      "startChar": 16,
      "tokenModifiers": 4,
      "tokenType": 12
    },
    {
      "length": 3,
      "line": 4,
      "startChar": 19,
      "tokenModifiers": 0,
      "tokenType": 28
    },
    {
      "length": 1,
      "line": 4,
      "startChar": 24,
      "tokenModifiers": 4,
      "tokenType": 12
    },
    {
      "length": 3,
      "line": 4,
      "startChar": 27,
      "tokenModifiers": 0,
      "tokenType": 28
    },
    {
      "length": 2,
      "line": 4,
      "startChar": 32,
      "tokenModifiers": 0,
      "tokenType": 11
    },
    {
      "length": 3,
      "line": 4,
      "startChar": 35,
      "tokenModifiers": 0,
      "tokenType": 28
    },
    {
      "length": 1,
      "line": 5,
      "startChar": 4,
      "tokenModifiers": 0,
      "tokenType": 12
    },
    {
      "length": 1,
      "line": 5,
      "startChar": 6,
      "tokenModifiers": 0,
      "tokenType": 11
    },
    {
      "length": 1,
      "line": 5,
      "startChar": 8,
      "tokenModifiers": 0,
      "tokenType": 12
    }
  ]
}
rc=0 ms=46
```

### inlay-hint
```
[]
rc=0 ms=49
```

### code-action
```
[]
rc=0 ms=41
```

### format-clean
```
[
  {
    "newText": "pub fn add(a: i32, b: i32) -> i32 {\r\n    a + b\r\n}\r\n\r\npub fn multiply(a: i32, b: i32) -> i32 {\r\n    a * b\r\n}\r\n",
    "range": {
      "end": {
        "character": 1,
        "line": 6
      },
      "start": {
        "character": 0,
        "line": 0
      }
    }
  }
]
rc=0 ms=97
```

### diagnostics-err
```
{
  "items": []
}
rc=0 ms=85
```

### completion
```
tool error: {"code":"BAD_ARGS","message":"position 7:20 out of range: position out of range","retryable":false}
rc=2 ms=41
```

### format-dirty
```
[
  {
    "newText": "pub fn add(a: i32, b: i32) -> i32 {\r\n    a + b\r\n}\r\n\r\npub fn multiply(a: i32, b: i32) -> i32 {\r\n    a * b\r\n}\r\nfn ugly() {\r\n    let _x = a + b;\r\n}\r\n",
    "range": {
      "end": {
        "character": 0,
        "line": 7
      },
      "start": {
        "character": 0,
        "line": 0
      }
    }
  }
]
rc=0 ms=101
```

### ch-prepare
```
tool error: {"code":"RPC_ERROR","message":"rpc -32801: content modified","retryable":false}
rc=1 ms=2603
```

### call-hierarchy: prepare 未返回 item，incoming/outgoing 跳过
### replace-body
```
null
rc=0 ms=40
```

### replace-text
```
null
rc=0 ms=43
```

### insert-before
```
{
  "end_col": 9,
  "end_line": 5
}
rc=0 ms=52
```

### insert-after
```
{
  "end_col": 9,
  "end_line": 7
}
rc=0 ms=39
```

### delete-text
```
tool error: {"code":"BAD_ARGS","message":"delete_text_in_symbol: bad args: line range 1..1 outside symbol body 5..7","retryable":false}
rc=2 ms=39
```

### safe-delete-ok
```
tool error: {"code":"RPC_ERROR","message":"safe-delete-symbol: symbol has 1 textual occurrence(s) outside definition but 0 semantic refs; semantic layer may be unavailable","retryable":false}
rc=1 ms=38
```

### safe-delete-ref
```
tool error: {"code":"RPC_ERROR","message":"safe-delete-symbol: symbol has 3 textual occurrence(s) outside definition but 0 semantic refs; semantic layer may be unavailable","retryable":false}
rc=1 ms=40
```

### insert-at-line
```
{
  "end_col": 1,
  "end_line": 2
}
rc=0 ms=47
```

### replace-lines
```
null
rc=0 ms=39
```

### delete-lines
```
null
rc=0 ms=39
```

### rename
```
{
  "edits_applied": 1,
  "files": [
    "lib.rs"
  ],
  "files_modified": 1
}
rc=0 ms=55
```

RENAME_CHECK adder in main.rs: 0
DONE
