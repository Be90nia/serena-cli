# F 段 daemon 生命周期 2026-09-20 21:25:22
### snap[pre]
lock: ABSENT
### status-0
```
daemon: not running
rc=1 ms=38
```

### stop-all-0
```
daemon: not running
rc=0 ms=37
```

### overview-cold
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
rc=0 ms=752
```

### snap[after-cold]
```json
{
  "pid": 22444,
  "port": 7860,
  "boot_ms": 1789910722531,
  "token": "48c5239c510ad718ac57000000000000"
}```
### status-1
```
{
  "active_project": "D:\\Project\\serena-rust\\fixtures\\rust_demo",
  "draining": false,
  "loaded_ls": [
    "rust"
  ],
  "pid": 22444,
  "uptime_secs": 0
}
rc=0 ms=40
```

### overview-warm
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
rc=0 ms=42
```

### def-warm
```
null
rc=0 ms=40
```

### hover-warm
```
null
rc=0 ms=42
```

### stop-all-1
```
daemon draining (pid 22444); lock will be removed by reaper
rc=0 ms=67
```

### snap[after-stop]
```json
{
  "pid": 22444,
  "port": 7860,
  "boot_ms": 1789910722531,
  "token": "48c5239c510ad718ac57000000000000"
}```
### status-2
```
daemon: not running
rc=1 ms=543
```

### overview-race
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
rc=0 ms=1288
```

### snap[after-race]
```json
{
  "pid": 33920,
  "port": 7860,
  "boot_ms": 1789910725011,
  "token": "58caf22f520ad7188084000000000000"
}```
### status-3
```
{
  "active_project": "D:\\Project\\serena-rust\\fixtures\\rust_demo",
  "draining": false,
  "loaded_ls": [
    "rust"
  ],
  "pid": 33920,
  "uptime_secs": 0
}
rc=0 ms=38
```

### overview-regen
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
rc=0 ms=36
```

### snap[after-regen]
```json
{
  "pid": 33920,
  "port": 7860,
  "boot_ms": 1789910725011,
  "token": "58caf22f520ad7188084000000000000"
}```
### hover-regen
```
null
rc=0 ms=35
```

### stop-all-final
```
daemon draining (pid 33920); lock will be removed by reaper
rc=0 ms=34
```

### snap[final]
```json
{
  "pid": 33920,
  "port": 7860,
  "boot_ms": 1789910725011,
  "token": "58caf22f520ad7188084000000000000"
}```
### status-final
```
daemon: not running
rc=1 ms=536
```

DONE
