# 适配器 3 缺陷修复 e2e（--direct） 2026-09-20 23:31:02
### py-overview
```
[
  {
    "container": "add",
    "kind": "Function",
    "name": "add",
    "range": {
      "end": {
        "character": 16,
        "line": 2
      },
      "start": {
        "character": 0,
        "line": 1
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/py_demo/app.py"
  },
  {
    "container": "add",
    "kind": {
      "Other": 13
    },
    "name": "a",
    "range": {
      "end": {
        "character": 14,
        "line": 1
      },
      "start": {
        "character": 8,
        "line": 1
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/py_demo/app.py"
  },
  {
    "container": "add",
    "kind": {
      "Other": 13
    },
    "name": "b",
    "range": {
      "end": {
        "character": 22,
        "line": 1
      },
      "start": {
        "character": 16,
        "line": 1
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/py_demo/app.py"
  },
  {
    "container": "main",
    "kind": "Function",
    "name": "main",
    "range": {
      "end": {
        "character": 12,
        "line": 6
      },
      "start": {
        "character": 0,
        "line": 4
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/py_demo/app.py"
  },
  {
    "container": "main",
    "kind": {
      "Other": 13
    },
    "name": "s",
    "range": {
      "end": {
        "character": 5,
        "line": 5
      },
      "start": {
        "character": 4,
        "line": 5
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/py_demo/app.py"
  }
]
rc=0

### py-def
```
null
rc=0

### py-hover
```
this subcommand is daemon-mode only in M1
rc=2

### py-diagnostics
```
this subcommand is daemon-mode only in M1
rc=2

### ts-defining
```
this subcommand is daemon-mode only in M1
rc=2

### ts-rename
```
this subcommand is daemon-mode only in M1
rc=2

RENAME_CHECK multiplier in main.ts: 0
### go-overview
```
[
  {
    "container": "add",
    "kind": "Function",
    "name": "add",
    "range": {
      "end": {
        "character": 1,
        "line": 7
      },
      "start": {
        "character": 0,
        "line": 5
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/go_demo/main.go"
  },
  {
    "container": "main",
    "kind": "Function",
    "name": "main",
    "range": {
      "end": {
        "character": 1,
        "line": 12
      },
      "start": {
        "character": 0,
        "line": 9
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/go_demo/main.go"
  }
]
rc=0

### go-rename
```
this subcommand is daemon-mode only in M1
rc=2

RENAME_CHECK adder in main.go: 0
DONE
