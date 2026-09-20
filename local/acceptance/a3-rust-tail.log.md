# A3 段 rust_demo 尾部（daemon） 2026-09-20 21:29:40
### read-file
```
{
  "content": "pub fn add(a: i32, b: i32) -> i32 {\n    a + b",
  "end_line": 2,
  "hash": "f3f327a8c2cb3700",
  "start_line": 1,
  "total_lines": 7
}
rc=0 ms=650
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
rc=0 ms=37
```

### find-file
```
[
  "main.rs"
]
rc=0 ms=45
```

### sig-help
```
null
rc=0 ms=146
```

### doc-highlight
```
[]
rc=0 ms=42
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
rc=0 ms=50
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
      "tokenModifiers": 4,
      "tokenType": 4
    },
    {
      "length": 1,
      "line": 0,
      "startChar": 11,
      "tokenModifiers": 4,
      "tokenType": 17
    },
    {
      "length": 3,
      "line": 0,
      "startChar": 14,
      "tokenModifiers": 0,
      "tokenType": 9
    },
    {
      "length": 1,
      "line": 0,
      "startChar": 19,
      "tokenModifiers": 4,
      "tokenType": 17
    },
    {
      "length": 3,
      "line": 0,
      "startChar": 22,
      "tokenModifiers": 0,
      "tokenType": 9
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
      "tokenType": 9
    },
    {
      "length": 1,
      "line": 1,
      "startChar": 4,
      "tokenModifiers": 0,
      "tokenType": 34
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
      "tokenType": 34
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
      "tokenModifiers": 4,
      "tokenType": 4
    },
    {
      "length": 1,
      "line": 4,
      "startChar": 16,
      "tokenModifiers": 4,
      "tokenType": 17
    },
    {
      "length": 3,
      "line": 4,
      "startChar": 19,
      "tokenModifiers": 0,
      "tokenType": 9
    },
    {
      "length": 1,
      "line": 4,
      "startChar": 24,
      "tokenModifiers": 4,
      "tokenType": 17
    },
    {
      "length": 3,
      "line": 4,
      "startChar": 27,
      "tokenModifiers": 0,
      "tokenType": 9
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
      "tokenType": 9
    },
    {
      "length": 1,
      "line": 5,
      "startChar": 4,
      "tokenModifiers": 0,
      "tokenType": 34
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
      "tokenType": 34
    }
  ]
}
rc=0 ms=38
```

### inlay-hint
```
[]
rc=0 ms=42
```

### code-action
```
[]
rc=0 ms=77
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
rc=0 ms=112
```

### diagnostics-err
```
{
  "items": []
}
rc=0 ms=62
```

### completion
```
tool error: {"code":"BAD_ARGS","message":"position 7:20 out of range: position out of range","retryable":false}
rc=2 ms=70
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
rc=0 ms=116
```

### ch-prepare
```
[]
rc=0 ms=47
```

### call-hierarchy: prepare 未返回 item，incoming/outgoing 跳过
### replace-body
```
tool error: {"code":"RPC_ERROR","message":"rpc -1: response decode failed: data did not match any variant of untagged enum DocumentSymbolResponse","retryable":false}
rc=1 ms=39
```

### replace-text
```
null
rc=0 ms=98
```

### insert-before
```
{
  "end_col": 9,
  "end_line": 5
}
rc=0 ms=54
```

### insert-after
```
{
  "end_col": 9,
  "end_line": 7
}
rc=0 ms=54
```

### delete-text
```
tool error: {"code":"BAD_ARGS","message":"delete_text_in_symbol: bad args: line range 1..1 outside symbol body 5..7","retryable":false}
rc=2 ms=44
```

### safe-delete-ok
```
{
  "deleted": true,
  "references": [],
  "symbol": "multiply"
}
rc=0 ms=46
```

### safe-delete-ref
```
{
  "deleted": true,
  "references": [],
  "symbol": "add"
}
rc=0 ms=219
```

### insert-at-line
```
{
  "end_col": 1,
  "end_line": 2
}
rc=0 ms=40
```

### replace-lines
```
null
rc=0 ms=57
```

### delete-lines
```
null
rc=0 ms=40
```

### rename
```
tool error: {"code":"RPC_ERROR","message":"rpc -32602: No references found at position","retryable":false}
rc=1 ms=55
```

RENAME_CHECK adder in main.rs: 0
DONE
