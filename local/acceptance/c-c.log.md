# C 段 c_demo（clangd）核心 10（daemon） 2026-09-20 21:30:09
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
        "line": 3
      },
      "start": {
        "character": 0,
        "line": 1
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/c_demo/main.c"
  },
  {
    "container": "main",
    "kind": "Function",
    "name": "main",
    "range": {
      "end": {
        "character": 1,
        "line": 8
      },
      "start": {
        "character": 0,
        "line": 5
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/c_demo/main.c"
  }
]
rc=0 ms=1310
```

### find-symbol
```
[
  {
    "container": "",
    "kind": "Function",
    "name": "add",
    "range": {
      "end": {
        "character": 7,
        "line": 1
      },
      "start": {
        "character": 4,
        "line": 1
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/c_demo/main.c"
  }
]
rc=0 ms=38
```

### def
```
{
  "range": {
    "end": {
      "character": 7,
      "line": 1
    },
    "start": {
      "character": 4,
      "line": 1
    }
  },
  "uri": "file:///D:/Project/serena-rust/fixtures/c_demo/main.c"
}
rc=0 ms=99
```

### refs
```
[
  {
    "range": {
      "end": {
        "character": 7,
        "line": 1
      },
      "start": {
        "character": 4,
        "line": 1
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/c_demo/main.c"
  },
  {
    "range": {
      "end": {
        "character": 15,
        "line": 6
      },
      "start": {
        "character": 12,
        "line": 6
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/c_demo/main.c"
  }
]
rc=0 ms=93
```

### hover
```
{
  "contents": {
    "kind": "plaintext",
    "value": "function add\n\n→ int\n\nParameters:\n\n- int a\n- int b\n\nclangd C fixture (M3 verify) — same simple `add` shape as cpp_demo/main.cpp.\n\nint add(int a, int b)"
  },
  "range": {
    "end": {
      "character": 7,
      "line": 1
    },
    "start": {
      "character": 4,
      "line": 1
    }
  }
}
rc=0 ms=109
```

### diagnostics
```
{
  "items": []
}
rc=0 ms=40
```

### completion
```
{
  "items": [],
  "truncated": null
}
rc=0 ms=40
```

### format
```
[
  {
    "newText": " ",
    "range": {
      "end": {
        "character": 4,
        "line": 2
      },
      "start": {
        "character": 23,
        "line": 1
      }
    }
  },
  {
    "newText": " ",
    "range": {
      "end": {
        "character": 0,
        "line": 3
      },
      "start": {
        "character": 17,
        "line": 2
      }
    }
  },
  {
    "newText": "\n  ",
    "range": {
      "end": {
        "character": 4,
        "line": 6
      },
      "start": {
        "character": 16,
        "line": 5
      }
    }
  },
  {
    "newText": "\n  ",
    "range": {
      "end": {
        "character": 4,
        "line": 7
      },
      "start": {
        "character": 22,
        "line": 6
      }
    }
  }
]
rc=0 ms=46
```

### rename
```
{
  "edits_applied": 2,
  "files": [
    "main.c"
  ],
  "files_modified": 1
}
rc=0 ms=93
```

RENAME_CHECK adder in main.c: 2
### doc-highlight
```
[
  {
    "kind": 1,
    "range": {
      "end": {
        "character": 7,
        "line": 1
      },
      "start": {
        "character": 4,
        "line": 1
      }
    }
  },
  {
    "kind": 1,
    "range": {
      "end": {
        "character": 15,
        "line": 6
      },
      "start": {
        "character": 12,
        "line": 6
      }
    }
  }
]
rc=0 ms=98
```

DONE
