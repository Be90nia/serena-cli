//! RPC 参数 catalog —— `execute_tool` 所有分支名 + 参数表（手工列举）。
//!
//! 这是参数表的**唯一事实源**（P1 k32）。约束：
//! - 不破坏现有 wire / args 透传 —— catalog 是 read-time 校验 / 给 caller 看 schema 的，
//!   不是 write-time 反序列化门控（保持 `serde_json::Value` 自由 args 透传）。
//! - `tests/catalog.rs` 集成测试断言：(a) catalog 与 `execute_tool` match 分支一致；
//!   (b) catalog 包含所有 44 个工具。CI 失败即人工补。
//!
//! JSON Schema 风格：`args` 是对象，键 = 参数名；值含 `type`（string/number/bool/object/array）、
//! `required`（bool）、可选 `default`（JSON 字面量）、可选 `description`。
//!
//! ponytail：若未来工具参数需要更复杂校验（如条件必填），再升级 schema 引擎；
//! 当前 flat 列表 + 触发穷举对齐已经覆盖全部已知调用方。

// crate-level `#![recursion_limit = "512"]`（lib.rs）已为 json! 44 嵌套打开余量。

/// 返回 44 个工具的 RPC 参数 catalog。`Value::Object` → 可被任何 caller 序列化。
pub fn catalog() -> serde_json::Value {
    serde_json::json!({
        "$schema": "https://json-schema.org/draft-07/schema#",
        "title": "serena-rust RPC params catalog",
        "version": 1,
        "note": "read-time schema; execute_tool args 仍是自由 serde_json::Value",
        "tools": {
            "overview": {
                "args": {
                    "file": {"type": "string", "required": true, "description": "repo-relative path"},
                    "_timeout_ms": {"type": "number", "required": false, "description": "per-call override (private)"},
                    "_index_timeout_ms": {"type": "number", "required": false, "description": "per-call override (private)"},
                    "_compact": {"type": "bool", "required": false, "default": true, "description": "compact envelope (private)"},
                    "_delta": {"type": "bool", "required": false, "default": false, "description": "incremental delta (private)"},
                    "_max_tokens": {"type": "number", "required": false, "description": "soft budget on items (private)"},
                    "_compress": {"type": "bool", "required": false, "description": "strip container/kind (private)"}
                }
            },
            "symbol-tree": {
                "args": {
                    "dir": {"type": "string", "required": true},
                    "max_files": {"type": "number", "required": false, "default": 200}
                }
            },
            "find-symbol": {
                "args": {
                    "query": {"type": "string", "required": true},
                    "limit": {"type": "number", "required": false, "default": 50},
                    "_compact": {"type": "bool", "required": false, "default": true, "description": "(private)"},
                    "_delta": {"type": "bool", "required": false, "default": false, "description": "(private)"}
                }
            },
            "signature-help": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true, "description": "0-based (LSP)"},
                    "col": {"type": "number", "required": true, "description": "0-based (LSP)"}
                }
            },
            "code-action": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true},
                    "kind": {"type": "string", "required": false, "description": "CodeActionKind filter"}
                }
            },
            "format": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "tab_size": {"type": "number", "required": false},
                    "insert_spaces": {"type": "bool", "required": false}
                }
            },
            "format-range": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "start_line": {"type": "number", "required": true, "description": "0-based"},
                    "start_col": {"type": "number", "required": true},
                    "end_line": {"type": "number", "required": true},
                    "end_col": {"type": "number", "required": true},
                    "tab_size": {"type": "number", "required": false},
                    "insert_spaces": {"type": "bool", "required": false}
                }
            },
            "inlay-hint": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "start_line": {"type": "number", "required": true, "description": "0-based"},
                    "end_line": {"type": "number", "required": true, "description": "0-based"}
                }
            },
            "document-highlight": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true}
                }
            },
            "folding-range": {
                "args": {"file": {"type": "string", "required": true}}
            },
            "semantic-tokens": {
                "args": {"file": {"type": "string", "required": true}}
            },
            "code-lens": {
                "args": {"file": {"type": "string", "required": true}}
            },
            "document-link": {
                "args": {"file": {"type": "string", "required": true}}
            },
            "call-hierarchy": {
                "args": {
                    "op": {"type": "string", "required": true, "description": "prepare | incoming | outgoing"},
                    "file": {"type": "string", "required": false, "description": "required when op=prepare"},
                    "line": {"type": "number", "required": false, "description": "required when op=prepare; 0-based"},
                    "col": {"type": "number", "required": false, "description": "required when op=prepare; 0-based"},
                    "item": {"type": "object", "required": false, "description": "CallHierarchyItem JSON (required when op=incoming|outgoing)"}
                }
            },
            "type-hierarchy": {
                "args": {
                    "op": {"type": "string", "required": true, "description": "prepare | supertypes | subtypes"},
                    "file": {"type": "string", "required": false, "description": "required when op=prepare"},
                    "line": {"type": "number", "required": false, "description": "required when op=prepare"},
                    "col": {"type": "number", "required": false, "description": "required when op=prepare"},
                    "item": {"type": "object", "required": false, "description": "TypeHierarchyItem JSON (required when op=supertypes|subtypes)"}
                }
            },
            "moniker": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true}
                }
            },
            "workspace-diagnostic": {
                "args": {}
            },
            "hover": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true}
                }
            },
            "diagnostics": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "wait_gen": {"type": "number", "required": false, "description": "block until generation >= N (5s upper bound)"}
                }
            },
            "def": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true},
                    "_compact": {"type": "bool", "required": false, "default": true, "description": "(private)"}
                }
            },
            "containing-symbol": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true}
                }
            },
            "defining-symbol": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true}
                }
            },
            "refs": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true},
                    "_compact": {"type": "bool", "required": false, "default": true, "description": "(private)"},
                    "_delta": {"type": "bool", "required": false, "default": false, "description": "(private)"}
                }
            },
            "completion": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true},
                    "limit": {"type": "number", "required": false, "default": 5},
                    "trigger": {"type": "string", "required": false, "description": "trigger character (e.g. '.', ':')"}
                }
            },
            "find-implementations": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true},
                    "_compact": {"type": "bool", "required": false, "default": true, "description": "(private)"},
                    "_delta": {"type": "bool", "required": false, "default": false, "description": "(private)"}
                }
            },
            "search": {
                "args": {
                    "pattern": {"type": "string", "required": true},
                    "path_glob": {"type": "string", "required": false},
                    "max_results": {"type": "number", "required": false, "default": 100},
                    "case_sensitive": {"type": "bool", "required": false, "default": false},
                    "comments_only": {"type": "bool", "required": false, "default": false, "description": "I: filter to comment lines only"}
                }
            },
            "symbol-body": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true}
                }
            },
            "edit-context": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true},
                    "description": "B: body + callers + doc + tests in one call"
                }
            },
            "repo-map": {
                "args": {
                    "top_n": {"type": "number", "required": false, "default": 20, "description": "E: top symbols by ref count"}
                }
            },
            "warm": {
                "args": {
                    "lang": {"type": "string", "required": true, "description": "M: pre-warm LS for language"},
                    "timeout_secs": {"type": "number", "required": false, "default": 30}
                }
            },
            "replace-body": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true},
                    "new_body": {"type": "string", "required": true}
                }
            },
            "rename-symbol": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true, "description": "0-based"},
                    "col": {"type": "number", "required": true, "description": "0-based"},
                    "new_name": {"type": "string", "required": true, "description": "non-empty, no whitespace"}
                }
            },
            "read-file": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "start_line": {"type": "number", "required": false, "description": "1-based inclusive"},
                    "end_line": {"type": "number", "required": false, "description": "1-based inclusive"}
                }
            },
            "list-dir": {
                "args": {
                    "path": {"type": "string", "required": true},
                    "max_depth": {"type": "number", "required": false},
                    "max_entries": {"type": "number", "required": false, "default": 500}
                }
            },
            "find-file": {
                "args": {
                    "name_pattern": {"type": "string", "required": true},
                    "path_glob": {"type": "string", "required": false},
                    "max_results": {"type": "number", "required": false, "default": 200}
                }
            },
            "find-referencing-symbols": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true},
                    "grouped": {"type": "bool", "required": false, "default": false},
                    "page": {"type": "number", "required": false, "default": 1, "description": "pagination, used when grouped=true"},
                    "page_size": {"type": "number", "required": false, "default": 20},
                    "_compact": {"type": "bool", "required": false, "default": true, "description": "(private)"}
                }
            },
            "find-referencing-code-snippets": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true},
                    "context_lines": {"type": "number", "required": false, "default": 3},
                    "max_results": {"type": "number", "required": false, "default": 50},
                    "_compact": {"type": "bool", "required": false, "default": true, "description": "(private)"}
                }
            },
            "replace-text-in-symbol": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true},
                    "old_text": {"type": "string", "required": true},
                    "new_text": {"type": "string", "required": true}
                }
            },
            "insert-text-after-symbol": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true},
                    "text": {"type": "string", "required": true}
                }
            },
            "insert-text-before-symbol": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true},
                    "text": {"type": "string", "required": true}
                }
            },
            "delete-text-in-symbol": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true},
                    "start_line": {"type": "number", "required": true, "description": "1-based inclusive"},
                    "end_line": {"type": "number", "required": true, "description": "1-based inclusive"}
                }
            },
            "safe-delete-symbol": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true}
                }
            },
            "insert-at-line": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true, "description": "1-based"},
                    "content": {"type": "string", "required": true},
                    "expected_hash": {"type": "string", "required": false, "description": "hash for write-conflict guard"}
                }
            },
            "replace-lines": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "start_line": {"type": "number", "required": true, "description": "1-based inclusive"},
                    "end_line": {"type": "number", "required": true, "description": "1-based inclusive"},
                    "content": {"type": "string", "required": true},
                    "expected_hash": {"type": "string", "required": false}
                }
            },
            "delete-lines": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "start_line": {"type": "number", "required": true, "description": "1-based inclusive"},
                    "end_line": {"type": "number", "required": true, "description": "1-based inclusive"},
                    "expected_hash": {"type": "string", "required": false}
                }
            }
        }
    })
}

/// 工具名 → 参数表对象（catalog().tools 子集）。catalog.rs 测试用。
pub fn catalog_tool_names() -> Vec<String> {
    let v = catalog();
    let tools = v.get("tools").and_then(|t| t.as_object()).expect("tools object");
    let mut names: Vec<String> = tools.keys().cloned().collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_valid_json_object() {
        let v = catalog();
        assert!(v.is_object(), "catalog must be JSON object");
        assert!(v.get("tools").and_then(|t| t.as_object()).is_some());
    }

    #[test]
    fn catalog_has_at_least_24_tools() {
        // 上游 32 个 wrapper 当前 supervisor 已实现 ~24+，catalog 必须 ≥24。
        let names = catalog_tool_names();
        assert!(
            names.len() >= 24,
            "catalog too small: {} tools (need ≥24); names = {names:?}",
            names.len()
        );
    }

    #[test]
    fn every_required_arg_has_no_default_and_every_default_arg_is_optional() {
        let v = catalog();
        let tools = v.get("tools").and_then(|t| t.as_object()).unwrap();
        for (tool_name, tool_def) in tools {
            let args = tool_def.get("args").and_then(|a| a.as_object()).expect("args object");
            for (arg_name, arg_def) in args {
                let required = arg_def.get("required").and_then(|r| r.as_bool()).unwrap_or(false);
                let has_default = arg_def.get("default").is_some();
                if required {
                    assert!(
                        !has_default,
                        "tool `{tool_name}` arg `{arg_name}`: required=true must not carry a default"
                    );
                } else if has_default {
                    // optional + default 合法；确保 default 是合法 JSON。
                    let default = arg_def.get("default").unwrap();
                    assert!(
                        !default.is_null(),
                        "tool `{tool_name}` arg `{arg_name}`: default=null is meaningless"
                    );
                }
            }
        }
    }
}