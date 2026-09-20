# A 段 rust_demo 全命令 e2e（--direct, --json） 2026-09-20 21:02:19
### overview | rc=0 | ms=177
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
```

### symbol-tree | rc=0 | ms=182
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
```

### find-symbol | rc=2 | ms=73
```
this subcommand is daemon-mode only in M1
```

### def | rc=0 | ms=171
```
null
```

### refs | rc=0 | ms=172
```
[]
```

### find-impl | rc=2 | ms=83
```
this subcommand is daemon-mode only in M1
```

### hover | rc=2 | ms=84
```
this subcommand is daemon-mode only in M1
```

### diagnostics | rc=2 | ms=86
```
this subcommand is daemon-mode only in M1
```

### symbol-body | rc=2 | ms=81
```
this subcommand is daemon-mode only in M1
```

### containing | rc=2 | ms=85
```
this subcommand is daemon-mode only in M1
```

### defining | rc=2 | ms=86
```
this subcommand is daemon-mode only in M1
```

### find-ref-syms | rc=2 | ms=79
```
this subcommand is daemon-mode only in M1
```

### find-ref-snips | rc=2 | ms=76
```
this subcommand is daemon-mode only in M1
```

### search | rc=2 | ms=79
```
this subcommand is daemon-mode only in M1
```

### read-file | rc=2 | ms=88
```
this subcommand is daemon-mode only in M1
```

### list-dir | rc=2 | ms=80
```
this subcommand is daemon-mode only in M1
```

### find-file | rc=2 | ms=77
```
this subcommand is daemon-mode only in M1
```

### sig-help | rc=2 | ms=65
```
this subcommand is daemon-mode only in M1
```

### doc-highlight | rc=0 | ms=158
```
[]
```

### folding-range | rc=0 | ms=156
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
```

### semantic-tokens | rc=0 | ms=160
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
```

### inlay-hint | rc=0 | ms=146
```
[]
```

### code-action | rc=0 | ms=161
```
[]
```

### format-clean | rc=0 | ms=214
```
[
  {
    "newText": "\n",
    "range": {
      "end": {
        "character": 1,
        "line": 6
      },
      "start": {
        "character": 1,
        "line": 6
      }
    }
  }
]
```

### diagnostics-err | rc=2 | ms=92
```
this subcommand is daemon-mode only in M1
```

### completion | rc=2 | ms=169
```
bad args: position 7:20 out of range: position out of range
```

### format-dirty | rc=0 | ms=267
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
```

### ch-prepare | rc=0 | ms=195
```
[]
```

### call-hierarchy: prepare 未返回 item，incoming/outgoing 跳过
### replace-body | rc=2 | ms=85
```
this subcommand is daemon-mode only in M1
```

### replace-text | rc=2 | ms=106
```
this subcommand is daemon-mode only in M1
```

### insert-before | rc=2 | ms=117
```
this subcommand is daemon-mode only in M1
```

### insert-after | rc=2 | ms=105
```
this subcommand is daemon-mode only in M1
```

### delete-text | rc=2 | ms=106
```
this subcommand is daemon-mode only in M1
```

### safe-delete-ok | rc=2 | ms=118
```
this subcommand is daemon-mode only in M1
```

### safe-delete-ref | rc=2 | ms=111
```
this subcommand is daemon-mode only in M1
```

### insert-at-line | rc=2 | ms=105
```
this subcommand is daemon-mode only in M1
```

### replace-lines | rc=2 | ms=118
```
this subcommand is daemon-mode only in M1
```

### delete-lines | rc=2 | ms=121
```
this subcommand is daemon-mode only in M1
```

### rename | rc=2 | ms=100
```
this subcommand is daemon-mode only in M1
```

RENAME_CHECK adder in main.rs: 0
DONE
