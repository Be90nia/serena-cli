# E 段 go_demo（gopls）核心 10（daemon） 2026-09-20 21:30:34
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
rc=0 ms=2061
```

### find-symbol
```
[
  {
    "container": "_/D_/Project/serena-rust/fixtures/go_demo",
    "kind": "Function",
    "name": "add",
    "range": {
      "end": {
        "character": 8,
        "line": 5
      },
      "start": {
        "character": 5,
        "line": 5
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/go_demo/main.go"
  },
  {
    "container": "math/bits",
    "kind": "Function",
    "name": "Add",
    "range": {
      "end": {
        "character": 8,
        "line": 359
      },
      "start": {
        "character": 5,
        "line": 359
      }
    },
    "uri": "file:///D:/Go/src/math/bits/bits.go"
  },
  {
    "container": "internal/syscall/windows/sysdll",
    "kind": "Function",
    "name": "Add",
    "range": {
      "end": {
        "character": 8,
        "line": 26
      },
      "start": {
        "character": 5,
        "line": 26
      }
    },
    "uri": "file:///D:/Go/src/internal/syscall/windows/sysdll/sysdll.go"
  },
  {
    "container": "unsafe",
    "kind": "Function",
    "name": "Add",
    "range": {
      "end": {
        "character": 8,
        "line": 226
      },
      "start": {
        "character": 5,
        "line": 226
      }
    },
    "uri": "file:///D:/Go/src/unsafe/unsafe.go"
  },
  {
    "container": "internal/runtime/exithook",
    "kind": "Function",
    "name": "Add",
    "range": {
      "end": {
        "character": 8,
        "line": 42
      },
      "start": {
        "character": 5,
        "line": 42
      }
    },
    "uri": "file:///D:/Go/src/internal/runtime/exithook/hooks.go"
  },
  {
    "container": "time",
    "kind": "Method",
    "name": "Time.Add",
    "range": {
      "end": {
        "character": 17,
        "line": 1165
      },
      "start": {
        "character": 14,
        "line": 1165
      }
    },
    "uri": "file:///D:/Go/src/time/time.go"
  },
  {
    "container": "internal/runtime/atomic",
    "kind": "Method",
    "name": "Int32.Add",
    "range": {
      "end": {
        "character": 19,
        "line": 54
      },
      "start": {
        "character": 16,
        "line": 54
      }
    },
    "uri": "file:///D:/Go/src/internal/runtime/atomic/types.go"
  },
  {
    "container": "sync/atomic",
    "kind": "Method",
    "name": "Int32.Add",
    "range": {
      "end": {
        "character": 19,
        "line": 93
      },
      "start": {
        "character": 16,
        "line": 93
      }
    },
    "uri": "file:///D:/Go/src/sync/atomic/type.go"
  },
  {
    "container": "internal/runtime/atomic",
    "kind": "Method",
    "name": "Int64.Add",
    "range": {
      "end": {
        "character": 19,
        "line": 107
      },
      "start": {
        "character": 16,
        "line": 107
      }
    },
    "uri": "file:///D:/Go/src/internal/runtime/atomic/types.go"
  },
  {
    "container": "sync/atomic",
    "kind": "Method",
    "name": "Int64.Add",
    "range": {
      "end": {
        "character": 19,
        "line": 127
      },
      "start": {
        "character": 16,
        "line": 127
      }
    },
    "uri": "file:///D:/Go/src/sync/atomic/type.go"
  },
  {
    "container": "internal/runtime/atomic",
    "kind": "Method",
    "name": "Uint32.Add",
    "range": {
      "end": {
        "character": 20,
        "line": 289
      },
      "start": {
        "character": 17,
        "line": 289
      }
    },
    "uri": "file:///D:/Go/src/internal/runtime/atomic/types.go"
  },
  {
    "container": "sync/atomic",
    "kind": "Method",
    "name": "Uint32.Add",
    "range": {
      "end": {
        "character": 20,
        "line": 160
      },
      "start": {
        "character": 17,
        "line": 160
      }
    },
    "uri": "file:///D:/Go/src/sync/atomic/type.go"
  },
  {
    "container": "internal/runtime/atomic",
    "kind": "Method",
    "name": "Uint64.Add",
    "range": {
      "end": {
        "character": 20,
        "line": 342
      },
      "start": {
        "character": 17,
        "line": 342
      }
    },
    "uri": "file:///D:/Go/src/internal/runtime/atomic/types.go"
  },
  {
    "container": "sync/atomic",
    "kind": "Method",
    "name": "Uint64.Add",
    "range": {
      "end": {
        "character": 20,
        "line": 194
      },
      "start": {
        "character": 17,
        "line": 194
      }
    },
    "uri": "file:///D:/Go/src/sync/atomic/type.go"
  },
  {
    "container": "internal/runtime/atomic",
    "kind": "Method",
    "name": "Uintptr.Add",
    "range": {
      "end": {
        "character": 21,
        "line": 418
      },
      "start": {
        "character": 18,
        "line": 418
      }
    },
    "uri": "file:///D:/Go/src/internal/runtime/atomic/types.go"
  },
  {
    "container": "sync/atomic",
    "kind": "Method",
    "name": "Uintptr.Add",
    "range": {
      "end": {
        "character": 21,
        "line": 227
      },
      "start": {
        "character": 18,
        "line": 227
      }
    },
    "uri": "file:///D:/Go/src/sync/atomic/type.go"
  },
  {
    "container": "sync",
    "kind": "Method",
    "name": "WaitGroup.Add",
    "range": {
      "end": {
        "character": 24,
        "line": 76
      },
      "start": {
        "character": 21,
        "line": 76
      }
    },
    "uri": "file:///D:/Go/src/sync/waitgroup.go"
  },
  {
    "container": "internal/coverage/rtcov",
    "kind": "Function",
    "name": "AddMeta",
    "range": {
      "end": {
        "character": 12,
        "line": 62
      },
      "start": {
        "character": 5,
        "line": 62
      }
    },
    "uri": "file:///D:/Go/src/internal/coverage/rtcov/rtcov.go"
  },
  {
    "container": "sync/atomic",
    "kind": "Function",
    "name": "AddInt32",
    "range": {
      "end": {
        "character": 13,
        "line": 114
      },
      "start": {
        "character": 5,
        "line": 114
      }
    },
    "uri": "file:///D:/Go/src/sync/atomic/doc.go"
  },
  {
    "container": "sync/atomic",
    "kind": "Function",
    "name": "AddInt64",
    "range": {
      "end": {
        "character": 13,
        "line": 41
      },
      "start": {
        "character": 5,
        "line": 41
      }
    },
    "uri": "file:///D:/Go/src/sync/atomic/doc_64.go"
  },
  {
    "container": "sync/atomic",
    "kind": "Function",
    "name": "AddUint32",
    "range": {
      "end": {
        "character": 14,
        "line": 122
      },
      "start": {
        "character": 5,
        "line": 122
      }
    },
    "uri": "file:///D:/Go/src/sync/atomic/doc.go"
  },
  {
    "container": "sync/atomic",
    "kind": "Function",
    "name": "AddUint64",
    "range": {
      "end": {
        "character": 14,
        "line": 50
      },
      "start": {
        "character": 5,
        "line": 50
      }
    },
    "uri": "file:///D:/Go/src/sync/atomic/doc_64.go"
  },
  {
    "container": "runtime",
    "kind": "Function",
    "name": "AddCleanup",
    "range": {
      "end": {
        "character": 15,
        "line": 73
      },
      "start": {
        "character": 5,
        "line": 73
      }
    },
    "uri": "file:///D:/Go/src/runtime/mcleanup.go"
  },
  {
    "container": "sync/atomic",
    "kind": "Function",
    "name": "AddUintptr",
    "range": {
      "end": {
        "character": 15,
        "line": 128
      },
      "start": {
        "character": 5,
        "line": 128
      }
    },
    "uri": "file:///D:/Go/src/sync/atomic/doc.go"
  },
  {
    "container": "internal/syscall/windows",
    "kind": "Function",
    "name": "NetUserAdd",
    "range": {
      "end": {
        "character": 15,
        "line": 522
      },
      "start": {
        "character": 5,
        "line": 522
      }
    },
    "uri": "file:///D:/Go/src/internal/syscall/windows/zsyscall_windows.go"
  },
  {
    "container": "internal/syscall/windows",
    "kind": "Function",
    "name": "NetShareAdd",
    "range": {
      "end": {
        "character": 16,
        "line": 506
      },
      "start": {
        "character": 5,
        "line": 506
      }
    },
    "uri": "file:///D:/Go/src/internal/syscall/windows/zsyscall_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 14
    },
    "name": "TOKEN_ADJUST_DEFAULT",
    "range": {
      "end": {
        "character": 21,
        "line": 222
      },
      "start": {
        "character": 1,
        "line": 222
      }
    },
    "uri": "file:///D:/Go/src/syscall/security_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 14
    },
    "name": "MAX_ADAPTER_DESCRIPTION_LENGTH",
    "range": {
      "end": {
        "character": 36,
        "line": 842
      },
      "start": {
        "character": 6,
        "line": 842
      }
    },
    "uri": "file:///D:/Go/src/syscall/types_windows.go"
  },
  {
    "container": "time",
    "kind": "Method",
    "name": "Time.AddDate",
    "range": {
      "end": {
        "character": 21,
        "line": 1257
      },
      "start": {
        "character": 14,
        "line": 1257
      }
    },
    "uri": "file:///D:/Go/src/time/time.go"
  },
  {
    "container": "syscall",
    "kind": "Function",
    "name": "CertAddCertificateContextToStore",
    "range": {
      "end": {
        "character": 37,
        "line": 339
      },
      "start": {
        "character": 5,
        "line": 339
      }
    },
    "uri": "file:///D:/Go/src/syscall/zsyscall_windows.go"
  },
  {
    "container": "math/bits",
    "kind": "Function",
    "name": "Add32",
    "range": {
      "end": {
        "character": 10,
        "line": 373
      },
      "start": {
        "character": 5,
        "line": 373
      }
    },
    "uri": "file:///D:/Go/src/math/bits/bits.go"
  },
  {
    "container": "math/bits",
    "kind": "Function",
    "name": "Add64",
    "range": {
      "end": {
        "character": 10,
        "line": 385
      },
      "start": {
        "character": 5,
        "line": 385
      }
    },
    "uri": "file:///D:/Go/src/math/bits/bits.go"
  },
  {
    "container": "internal/runtime/math",
    "kind": "Function",
    "name": "Add64",
    "range": {
      "end": {
        "character": 10,
        "line": 53
      },
      "start": {
        "character": 5,
        "line": 53
      }
    },
    "uri": "file:///D:/Go/src/internal/runtime/math/math.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 23
    },
    "name": "AddrinfoW",
    "range": {
      "end": {
        "character": 14,
        "line": 1046
      },
      "start": {
        "character": 5,
        "line": 1046
      }
    },
    "uri": "file:///D:/Go/src/syscall/types_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "IpAdapterInfo.DhcpServer",
    "range": {
      "end": {
        "character": 11,
        "line": 858
      },
      "start": {
        "character": 1,
        "line": 858
      }
    },
    "uri": "file:///D:/Go/src/syscall/types_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "IpAdapterInfo.Description",
    "range": {
      "end": {
        "character": 12,
        "line": 849
      },
      "start": {
        "character": 1,
        "line": 849
      }
    },
    "uri": "file:///D:/Go/src/syscall/types_windows.go"
  },
  {
    "container": "syscall",
    "kind": "Method",
    "name": "Proc.Addr",
    "range": {
      "end": {
        "character": 19,
        "line": 145
      },
      "start": {
        "character": 15,
        "line": 145
      }
    },
    "uri": "file:///D:/Go/src/syscall/dll_windows.go"
  },
  {
    "container": "reflect",
    "kind": "Method",
    "name": "Value.Addr",
    "range": {
      "end": {
        "character": 19,
        "line": 265
      },
      "start": {
        "character": 15,
        "line": 265
      }
    },
    "uri": "file:///D:/Go/src/reflect/value.go"
  },
  {
    "container": "syscall",
    "kind": "Method",
    "name": "LazyProc.Addr",
    "range": {
      "end": {
        "character": 23,
        "line": 275
      },
      "start": {
        "character": 19,
        "line": 275
      }
    },
    "uri": "file:///D:/Go/src/syscall/dll_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "AddrinfoW.Addr",
    "range": {
      "end": {
        "character": 5,
        "line": 1053
      },
      "start": {
        "character": 1,
        "line": 1053
      }
    },
    "uri": "file:///D:/Go/src/syscall/types_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "Hostent.AddrList",
    "range": {
      "end": {
        "character": 9,
        "line": 670
      },
      "start": {
        "character": 1,
        "line": 670
      }
    },
    "uri": "file:///D:/Go/src/syscall/types_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "Hostent.AddrType",
    "range": {
      "end": {
        "character": 9,
        "line": 668
      },
      "start": {
        "character": 1,
        "line": 668
      }
    },
    "uri": "file:///D:/Go/src/syscall/types_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "AddrinfoW.Addrlen",
    "range": {
      "end": {
        "character": 8,
        "line": 1051
      },
      "start": {
        "character": 1,
        "line": 1051
      }
    },
    "uri": "file:///D:/Go/src/syscall/types_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "SockaddrInet4.Addr",
    "range": {
      "end": {
        "character": 5,
        "line": 837
      },
      "start": {
        "character": 1,
        "line": 837
      }
    },
    "uri": "file:///D:/Go/src/syscall/syscall_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "SockaddrInet6.Addr",
    "range": {
      "end": {
        "character": 5,
        "line": 856
      },
      "start": {
        "character": 1,
        "line": 856
      }
    },
    "uri": "file:///D:/Go/src/syscall/syscall_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "RawSockaddrAny.Addr",
    "range": {
      "end": {
        "character": 5,
        "line": 827
      },
      "start": {
        "character": 1,
        "line": 827
      }
    },
    "uri": "file:///D:/Go/src/syscall/syscall_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "InterfaceInfo.Address",
    "range": {
      "end": {
        "character": 8,
        "line": 823
      },
      "start": {
        "character": 1,
        "line": 823
      }
    },
    "uri": "file:///D:/Go/src/syscall/types_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "IpAdapterInfo.Address",
    "range": {
      "end": {
        "character": 8,
        "line": 851
      },
      "start": {
        "character": 1,
        "line": 851
      }
    },
    "uri": "file:///D:/Go/src/syscall/types_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "RawSockaddrInet4.Addr",
    "range": {
      "end": {
        "character": 5,
        "line": 809
      },
      "start": {
        "character": 1,
        "line": 809
      }
    },
    "uri": "file:///D:/Go/src/syscall/syscall_windows.go"
  },
  {
    "container": "syscall",
    "kind": {
      "Other": 8
    },
    "name": "RawSockaddrInet6.Addr",
    "range": {
      "end": {
        "character": 5,
        "line": 817
      },
      "start": {
        "character": 1,
        "line": 817
      }
    },
    "uri": "file:///D:/Go/src/syscall/syscall_windows.go"
  }
]
rc=0 ms=63
```

### def
```
{
  "range": {
    "end": {
      "character": 9,
      "line": 9
    },
    "start": {
      "character": 5,
      "line": 9
    }
  },
  "uri": "file:///D:/Project/serena-rust/fixtures/go_demo/main.go"
}
rc=0 ms=169
```

### refs
```
[
  {
    "range": {
      "end": {
        "character": 8,
        "line": 5
      },
      "start": {
        "character": 5,
        "line": 5
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/go_demo/main.go"
  },
  {
    "range": {
      "end": {
        "character": 9,
        "line": 10
      },
      "start": {
        "character": 6,
        "line": 10
      }
    },
    "uri": "file:///D:/Project/serena-rust/fixtures/go_demo/main.go"
  }
]
rc=0 ms=44
```

### hover
```
{
  "contents": {
    "kind": "markdown",
    "value": "```go\nfunc add(a int, b int) int\n```\n\n---\n\ngopls fixture (M3 verify).\n"
  },
  "range": {
    "end": {
      "character": 8,
      "line": 5
    },
    "start": {
      "character": 5,
      "line": 5
    }
  }
}
rc=0 ms=32
```

### diagnostics
```
{
  "items": []
}
rc=0 ms=28
```

### completion
```
{
  "items": [
    {
      "additional_text_edits": [],
      "deprecated": false,
      "detail": "\"fmt\"",
      "doc": null,
      "insert": "fmt",
      "kind": "module",
      "label": "fmt"
    },
    {
      "additional_text_edits": [],
      "deprecated": false,
      "detail": "func(a int, b int) int",
      "doc": "gopls fixture (M3 verify).\n",
      "insert": "add",
      "kind": "function",
      "label": "add"
    },
    {
      "additional_text_edits": [],
      "deprecated": false,
      "detail": "func()",
      "doc": null,
      "insert": "main",
      "kind": "function",
      "label": "main"
    },
    {
      "additional_text_edits": [],
      "deprecated": false,
      "detail": null,
      "doc": null,
      "insert": "const",
      "kind": "keyword",
      "label": "const"
    },
    {
      "additional_text_edits": [],
      "deprecated": false,
      "detail": null,
      "doc": null,
      "insert": "func",
      "kind": "keyword",
      "label": "func"
    }
  ],
  "truncated": "5 of 54"
}
rc=0 ms=34
```

### doc-highlight
```
[
  {
    "kind": 1,
    "range": {
      "end": {
        "character": 9,
        "line": 10
      },
      "start": {
        "character": 6,
        "line": 10
      }
    }
  },
  {
    "kind": 1,
    "range": {
      "end": {
        "character": 8,
        "line": 5
      },
      "start": {
        "character": 5,
        "line": 5
      }
    }
  }
]
rc=0 ms=29
```

### rename
```
tool error: {"code":"RPC_ERROR","message":"rename_symbol: rename response has no `changes` map (M2 only supports changes, not documentChanges)","retryable":false}
rc=1 ms=29
```

RENAME_CHECK adder in main.go: 0
### format
```
[]
rc=0 ms=41
```

DONE
