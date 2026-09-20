# F 段 daemon 生命周期 2026-09-21 01:47:06
### snap[pre]
lock: ABSENT
### status-0
```
daemon: not running
rc=1 ms=32
```

### stop-all-0
```
daemon: not running
rc=0 ms=30
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
rc=0 ms=742
```

### snap[after-cold]
```json
{
  "pid": 24364,
  "port": 7860,
  "boot_ms": 1789926426264,
  "token": "485b92eb9918d7182c5f000000000000"
}```
### status-1
```
{
  "active_project": "D:\\Project\\serena-rust\\fixtures\\rust_demo",
  "draining": false,
  "loaded_ls": [
    "rust"
  ],
  "pid": 24364,
  "uptime_secs": 0
}
rc=0 ms=36
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
rc=0 ms=36
```

### def-warm
```
null
rc=0 ms=39
```

### hover-warm
```
null
rc=0 ms=39
```

### stop-all-1
```
daemon draining (pid 24364); lock will be removed by reaper
rc=0 ms=38
```

### snap[after-stop]
```json
{
  "pid": 24364,
  "port": 7860,
  "boot_ms": 1789926426264,
  "token": "485b92eb9918d7182c5f000000000000"
}```
### status-2
```
daemon: not running
rc=1 ms=550
```

### overview-race
```
daemon on :7860 not ready within 10s
rc=3 ms=12717
```

### snap[after-race]
lock: ABSENT
### status-3
```
daemon: not running
rc=1 ms=30
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
rc=0 ms=743
```

### snap[after-regen]
```json
{
  "pid": 36240,
  "port": 7860,
  "boot_ms": 1789926440836,
  "token": "dc8025509d18d718908d000000000000"
}```
### hover-regen
```
null
rc=0 ms=49
```

### stop-all-final
```
daemon draining (pid 36240); lock will be removed by reaper
rc=0 ms=35
```

### snap[final]
```json
{
  "pid": 36240,
  "port": 7860,
  "boot_ms": 1789926440836,
  "token": "dc8025509d18d718908d000000000000"
}```
### status-final
```
daemon: not running
rc=1 ms=535
```

DONE
