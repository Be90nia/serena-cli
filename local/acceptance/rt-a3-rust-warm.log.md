# A3 rust warm-daemon 语义复核 2026-09-21 01:56:45
### diagnostics-err-opened
```
{
  "items": []
}
```

### diagnostics-err-fresh
```
{
  "items": [
    {
      "code": "unlinked-file",
      "codeDescription": {
        "href": "https://rust-analyzer.github.io/book/diagnostics.html#unlinked-file"
      },
      "message": "This file is not included anywhere in the module tree, so rust-analyzer can't offer IDE services.\n\nIf you're intentionally working on unowned files, you can silence this warning by adding \"unlinked-file\" to rust-analyzer.diagnostics.disabled in your settings.",
      "range": {
        "end": {
          "character": 0,
          "line": 1
        },
        "start": {
          "character": 0,
          "line": 0
        }
      },
      "severity": 4,
      "source": "rust-analyzer",
      "tags": [
        1
      ]
    }
  ]
}
```

### containing-warm
```
[
  {
    "container": null,
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
  }
]
```

### replace-body
```
null
```

### replace-body-after
```

    a * 2```

### safe-delete-norefs
```
tool error: {"code":"RPC_ERROR","message":"safe-delete-symbol: symbol has 1 textual occurrence(s) outside definition but 0 semantic refs; semantic layer may be unavailable","retryable":false}
```

### safe-delete-referenced
```
{
  "deleted": false,
  "references": [
    {
      "file": "main.rs",
      "line": 4
    }
  ],
  "symbol": "add"
}
```

DONE
