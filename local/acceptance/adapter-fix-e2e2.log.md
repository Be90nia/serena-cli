# 适配器 3 缺陷修复 e2e（daemon 模式） 2026-09-20 23:33:50
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
```

### py-def
```
null
rc=0
```

### py-hover
```
null
rc=0
```

### py-diagnostics
```
{
  "items": []
}
rc=0
```

### ts-defining
```
[
  {
    "source": {
      "col": 21,
      "file": "main.ts",
      "line": 7
    },
    "symbol": {
      "body": "export class Calculator {\r\n  private history: number[] = [];\r\n\r\n  compute(a: number, b: number): number {\r\n    const result = multiply(a, b);\r\n    this.history.push(result);\r\n    return result;\r\n  }\r\n}",
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
      }
    }
  },
  {
    "source": {
      "col": 21,
      "file": "main.ts",
      "line": 7
    },
    "symbol": {
      "body": "compute(a: number, b: number): number {\r\n    const result = multiply(a, b);\r\n    this.history.push(result);\r\n    return result;\r\n  }",
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
      }
    }
  }
]
rc=0
```

### ts-rename
```
{
  "edits_applied": 2,
  "files": [
    "main.ts"
  ],
  "files_modified": 1
}
rc=0
```

RENAME_CHECK multiplier in main.ts: 2
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
```

### go-rename
```
{
  "edits_applied": 2,
  "files": [
    "main.go"
  ],
  "files_modified": 1
}
rc=0
```

RENAME_CHECK adder in main.go: 2

## 坐标修正复测（pyright def/hover）

初测 hover (0,4)/def (4,9) 沿用验收脚本坐标，但 (0,4) 落在文件首行注释、(4,9) 落在
`main()` 的右括号上——null 属 LS 正确行为而非管道故障。改打标识符内部复测：
hover → (1,5)（`def add` 的 add 内），def → (5,9)（调用点 `add(1, 2)` 的 add 内）。

中间若干段 "LS_NOT_INSTALLED / daemon not ready" 假阴性系测量方法伪影（非 Git Bash
环境下 cygpath 缺失 → 导出 MSYS 风格 PATH 对 cli.exe 无效；及孤儿进程占 7860），
已从本日志裁剪，方法教训见汇报。

注：py-def 响应中的 uri 原样呈现 `file:///d%3A/...`——pyright 真实返回 percent-encode
小写盘符 URI，本工具输出层不转码（透传 LS 值）；消费侧（defining-symbol/rename/
refs 落盘路径）统一经 uri_to_path 解码，即本轮修复点。

### py-hover-corrected (add 定义行 1:5, 干净 daemon)
```
{
  "contents": {
    "kind": "plaintext",
    "value": "(function) def add(\n    a: int,\n    b: int\n) -> int"
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
rc=0
```

### py-def-corrected (调用点 5:9)
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
  "uri": "file:///d%3A/Project/serena-rust/fixtures/py_demo/app.py"
}
rc=0
```

### py-diagnostics (干净 daemon 复核)
```
{
  "items": []
}
rc=0
```
daemon draining (pid 24488); lock will be removed by reaper
LOCK_CLEAN
DONE3
