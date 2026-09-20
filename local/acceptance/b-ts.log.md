# B 段 typescript_demo 核心 20（daemon） 2026-09-20 21:29:54
### overview
```
[
  {
    "container": "Calculator",
    "kind": "Class",
    "name": "Calculator",
    "range": {
      "end": {
        "character": 1,
        "line": 12
      },
      "start": {
        "character": 0,
        "line": 4
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
  },
  {
    "container": "Calculator",
    "kind": "Method",
    "name": "compute",
    "range": {
      "end": {
        "character": 3,
        "line": 11
      },
      "start": {
        "character": 2,
        "line": 7
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
  },
  {
    "container": "compute",
    "kind": {
      "Other": 14
    },
    "name": "result",
    "range": {
      "end": {
        "character": 33,
        "line": 8
      },
      "start": {
        "character": 10,
        "line": 8
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
  },
  {
    "container": "Calculator",
    "kind": {
      "Other": 7
    },
    "name": "history",
    "range": {
      "end": {
        "character": 33,
        "line": 5
      },
      "start": {
        "character": 2,
        "line": 5
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
  },
  {
    "container": "multiply",
    "kind": "Function",
    "name": "multiply",
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
    "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
  }
]
rc=0 ms=2398
```

### symbol-tree
```
{
  "dir": "D:/Project/serena-rust/fixtures/typescript_demo",
  "entries": [
    {
      "file": "main.ts",
      "symbols": [
        {
          "container": "Calculator",
          "kind": "Class",
          "name": "Calculator",
          "range": {
            "end": {
              "character": 1,
              "line": 12
            },
            "start": {
              "character": 0,
              "line": 4
            }
          },
          "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
        },
        {
          "container": "Calculator",
          "kind": "Method",
          "name": "compute",
          "range": {
            "end": {
              "character": 3,
              "line": 11
            },
            "start": {
              "character": 2,
              "line": 7
            }
          },
          "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
        },
        {
          "container": "compute",
          "kind": {
            "Other": 14
          },
          "name": "result",
          "range": {
            "end": {
              "character": 33,
              "line": 8
            },
            "start": {
              "character": 10,
              "line": 8
            }
          },
          "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
        },
        {
          "container": "Calculator",
          "kind": {
            "Other": 7
          },
          "name": "history",
          "range": {
            "end": {
              "character": 33,
              "line": 5
            },
            "start": {
              "character": 2,
              "line": 5
            }
          },
          "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
        },
        {
          "container": "multiply",
          "kind": "Function",
          "name": "multiply",
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
          "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
        }
      ]
    }
  ],
  "errors": [],
  "files_scanned": 1,
  "truncated": false
}
rc=0 ms=53
```

### find-symbol
```
[]
rc=0 ms=63
```

### def
```
{
  "range": {
    "end": {
      "character": 22,
      "line": 7
    },
    "start": {
      "character": 21,
      "line": 7
    }
  },
  "uri": "file:///d%3A/Project/serena-rust/fixtures/typescript_demo/main.ts"
}
rc=0 ms=55
```

### refs
```
[
  {
    "range": {
      "end": {
        "character": 24,
        "line": 0
      },
      "start": {
        "character": 16,
        "line": 0
      }
    },
    "uri": "file:///d%3A/Project/serena-rust/fixtures/typescript_demo/main.ts"
  },
  {
    "range": {
      "end": {
        "character": 27,
        "line": 8
      },
      "start": {
        "character": 19,
        "line": 8
      }
    },
    "uri": "file:///d%3A/Project/serena-rust/fixtures/typescript_demo/main.ts"
  }
]
rc=0 ms=73
```

### hover
```
{
  "contents": {
    "kind": "markdown",
    "value": "\n```typescript\nfunction multiply(a: number, b: number): number\n```\n"
  },
  "range": {
    "end": {
      "character": 24,
      "line": 0
    },
    "start": {
      "character": 16,
      "line": 0
    }
  }
}
rc=0 ms=60
```

### diagnostics
```
{
  "items": []
}
rc=0 ms=5524
```

### symbol-body
```
"compute(a: number, b: number): number {\n    const result = multiply(a, b);\n    this.history.push(result);\n    return result;\n  }"
rc=0 ms=35
```

### containing
```
[
  {
    "container": null,
    "kind": "Class",
    "name": "Calculator",
    "range": {
      "end": {
        "character": 1,
        "line": 12
      },
      "start": {
        "character": 0,
        "line": 4
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
  },
  {
    "container": "Calculator",
    "kind": "Method",
    "name": "compute",
    "range": {
      "end": {
        "character": 3,
        "line": 11
      },
      "start": {
        "character": 2,
        "line": 7
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
  },
  {
    "container": "compute",
    "kind": {
      "Other": 14
    },
    "name": "result",
    "range": {
      "end": {
        "character": 33,
        "line": 8
      },
      "start": {
        "character": 10,
        "line": 8
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/typescript_demo/main.ts"
  }
]
rc=0 ms=40
```

### defining
```
tool error: {"code":"BAD_ARGS","message":"definition at /d%3A/Project/serena-rust/fixtures/typescript_demo/main.ts is outside workspace root \"D:\\\\Project\\\\serena-rust\\\\fixtures\\\\typescript_demo\"","retryable":false}
rc=2 ms=46
```

### find-ref-syms
```
[
  {
    "col": 21,
    "container_name": "",
    "file": "d%3A/Project/serena-rust/fixtures/typescript_demo/main.ts",
    "line": 7
  },
  {
    "col": 31,
    "container_name": "",
    "file": "d%3A/Project/serena-rust/fixtures/typescript_demo/main.ts",
    "line": 8
  }
]
rc=0 ms=47
```

### find-ref-snips
```
[]
rc=0 ms=42
```

### completion
```
{
  "items": [
    {
      "additional_text_edits": [],
      "deprecated": false,
      "detail": null,
      "doc": null,
      "insert": "Calculator",
      "kind": "class",
      "label": "Calculator"
    },
    {
      "additional_text_edits": [],
      "deprecated": false,
      "detail": null,
      "doc": null,
      "insert": "AbortController",
      "kind": "variable",
      "label": "AbortController"
    },
    {
      "additional_text_edits": [],
      "deprecated": false,
      "detail": null,
      "doc": null,
      "insert": "AbortSignal",
      "kind": "variable",
      "label": "AbortSignal"
    },
    {
      "additional_text_edits": [],
      "deprecated": false,
      "detail": null,
      "doc": null,
      "insert": "AbortSignalEventMap",
      "kind": "interface",
      "label": "AbortSignalEventMap"
    },
    {
      "additional_text_edits": [],
      "deprecated": false,
      "detail": null,
      "doc": null,
      "insert": "AbstractRange",
      "kind": "variable",
      "label": "AbstractRange"
    }
  ],
  "truncated": "5 of 2526"
}
rc=0 ms=183
```

### sig-help
```
null
rc=0 ms=36
```

### doc-highlight
```
[
  {
    "kind": 2,
    "range": {
      "end": {
        "character": 24,
        "line": 0
      },
      "start": {
        "character": 16,
        "line": 0
      }
    }
  },
  {
    "kind": 2,
    "range": {
      "end": {
        "character": 27,
        "line": 8
      },
      "start": {
        "character": 19,
        "line": 8
      }
    }
  }
]
rc=0 ms=45
```

### semantic-tokens
```
{
  "resultId": null,
  "tokens": [
    {
      "length": 8,
      "line": 0,
      "startChar": 16,
      "tokenModifiers": 1,
      "tokenType": 10
    },
    {
      "length": 1,
      "line": 0,
      "startChar": 25,
      "tokenModifiers": 1,
      "tokenType": 6
    },
    {
      "length": 1,
      "line": 0,
      "startChar": 36,
      "tokenModifiers": 1,
      "tokenType": 6
    },
    {
      "length": 1,
      "line": 1,
      "startChar": 9,
      "tokenModifiers": 0,
      "tokenType": 6
    },
    {
      "length": 1,
      "line": 1,
      "startChar": 13,
      "tokenModifiers": 0,
      "tokenType": 6
    },
    {
      "length": 10,
      "line": 4,
      "startChar": 13,
      "tokenModifiers": 1,
      "tokenType": 0
    },
    {
      "length": 7,
      "line": 5,
      "startChar": 10,
      "tokenModifiers": 1,
      "tokenType": 9
    },
    {
      "length": 7,
      "line": 7,
      "startChar": 2,
      "tokenModifiers": 1,
      "tokenType": 11
    },
    {
      "length": 1,
      "line": 7,
      "startChar": 10,
      "tokenModifiers": 1,
      "tokenType": 6
    },
    {
      "length": 1,
      "line": 7,
      "startChar": 21,
      "tokenModifiers": 1,
      "tokenType": 6
    },
    {
      "length": 6,
      "line": 8,
      "startChar": 10,
      "tokenModifiers": 41,
      "tokenType": 7
    },
    {
      "length": 8,
      "line": 8,
      "startChar": 19,
      "tokenModifiers": 0,
      "tokenType": 10
    },
    {
      "length": 1,
      "line": 8,
      "startChar": 28,
      "tokenModifiers": 0,
      "tokenType": 6
    },
    {
      "length": 1,
      "line": 8,
      "startChar": 31,
      "tokenModifiers": 0,
      "tokenType": 6
    },
    {
      "length": 7,
      "line": 9,
      "startChar": 9,
      "tokenModifiers": 0,
      "tokenType": 9
    },
    {
      "length": 4,
      "line": 9,
      "startChar": 17,
      "tokenModifiers": 16,
      "tokenType": 11
    },
    {
      "length": 6,
      "line": 9,
      "startChar": 22,
      "tokenModifiers": 40,
      "tokenType": 7
    },
    {
      "length": 6,
      "line": 10,
      "startChar": 11,
      "tokenModifiers": 40,
      "tokenType": 7
    }
  ]
}
rc=0 ms=44
```

### folding-range
```
[
  {
    "endLine": 1,
    "startLine": 0
  },
  {
    "endLine": 11,
    "startLine": 4
  },
  {
    "endLine": 5,
    "startLine": 5
  },
  {
    "endLine": 10,
    "startLine": 7
  }
]
rc=0 ms=39
```

### code-action
```
[
  {
    "command": {
      "arguments": [
        {
          "action": "Move to a new file",
          "endLine": 8,
          "endOffset": 23,
          "file": "d:\\Project\\serena-rust\\fixtures\\typescript_demo\\main.ts",
          "refactor": "Move to a new file",
          "startLine": 8,
          "startOffset": 23
        }
      ],
      "command": "_typescript.applyRefactoring",
      "title": "Move to a new file"
    },
    "kind": "refactor.move.newFile",
    "title": "Move to a new file"
  },
  {
    "command": {
      "arguments": [
        {
          "action": "Convert parameters to destructured object",
          "endLine": 8,
          "endOffset": 23,
          "file": "d:\\Project\\serena-rust\\fixtures\\typescript_demo\\main.ts",
          "refactor": "Convert parameters to destructured object",
          "startLine": 8,
          "startOffset": 23
        }
      ],
      "command": "_typescript.applyRefactoring",
      "title": "Convert parameters to destructured object"
    },
    "kind": "refactor.rewrite.parameters.toDestructured",
    "title": "Convert parameters to destructured object"
  }
]
rc=0 ms=558
```

### format
```
[
  {
    "newText": "    ",
    "range": {
      "end": {
        "character": 2,
        "line": 1
      },
      "start": {
        "character": 0,
        "line": 1
      }
    }
  },
  {
    "newText": "    ",
    "range": {
      "end": {
        "character": 2,
        "line": 5
      },
      "start": {
        "character": 0,
        "line": 5
      }
    }
  },
  {
    "newText": "    ",
    "range": {
      "end": {
        "character": 2,
        "line": 7
      },
      "start": {
        "character": 0,
        "line": 7
      }
    }
  },
  {
    "newText": "        ",
    "range": {
      "end": {
        "character": 4,
        "line": 8
      },
      "start": {
        "character": 0,
        "line": 8
      }
    }
  },
  {
    "newText": "        ",
    "range": {
      "end": {
        "character": 4,
        "line": 9
      },
      "start": {
        "character": 0,
        "line": 9
      }
    }
  },
  {
    "newText": "        ",
    "range": {
      "end": {
        "character": 4,
        "line": 10
      },
      "start": {
        "character": 0,
        "line": 10
      }
    }
  },
  {
    "newText": "    ",
    "range": {
      "end": {
        "character": 2,
        "line": 11
      },
      "start": {
        "character": 0,
        "line": 11
      }
    }
  }
]
rc=0 ms=76
```

### search
```
{
  "files_scanned": 4,
  "hits": [
    {
      "col": 14,
      "file": "main.ts",
      "line": 5,
      "match_end": 23,
      "match_start": 13,
      "text": "export class Calculator {"
    }
  ],
  "truncated": false
}
rc=0 ms=44
```

### rename
```
{
  "edits_applied": 0,
  "files": [],
  "files_modified": 0
}
rc=0 ms=50
```

RENAME_CHECK multiplier in main.ts: 0
### safe-delete
```
{
  "deleted": false,
  "references": [
    {
      "file": "d%3A/Project/serena-rust/fixtures/typescript_demo/main.ts",
      "line": 9
    }
  ],
  "symbol": "multiply"
}
rc=0 ms=47
```

DONE
