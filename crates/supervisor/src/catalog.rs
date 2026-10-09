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
                    "_max_tokens": {"type": "number", "required": false, "description": "soft budget on list output (private)"},
                    "_compress": {"type": "bool", "required": false, "description": "strip container/kind (private)"},
                    "summary": {"type": "bool", "required": false, "default": false, "description": "bd kq6e: true → {summary, symbols} object with one-line kind histogram; default keeps bare array wire"}
                }
            },
            "symbol-tree": {
                "args": {
                    "dir": {"type": "string", "required": true},
                    "max_files": {"type": "number", "required": false, "default": 200, "description": "bd z0kg: absent → SERENA_DEFAULT_MAX_ITEMS env when set"},
                    "top_level": {"type": "bool", "required": false, "default": false, "description": "bd vro3: true → top-level symbols only per file (range-containment filter)"},
                    "grep": {"type": "string", "required": false, "description": "bd 6ooi: keep symbols whose name contains this substring (case-insensitive); entries with no matches omitted"},
                    "max_depth": {"type": "number", "required": false, "description": "bd 6ooi: keep symbols with containment-chain depth < N (top level = 0)"},
                    "files_only": {"type": "bool", "required": false, "default": false, "description": "bd 6ooi: true → file list only, zero LS calls"}
                }
            },
            "find-symbol": {
                "args": {
                    "query": {"type": "string", "required": true},
                    "limit": {"type": "number", "required": false, "default": 50, "description": "bd z0kg: absent → SERENA_DEFAULT_MAX_ITEMS env when set"},
                    "kind": {"type": "string", "required": false, "description": "bd w2b5: comma-separated kind filter (fn, method, class, struct, enum, interface, module, namespace, package, field, property, variable, constant, file)"},
                    "format": {"type": "string", "required": false, "default": "full", "description": "bd 51ib: brief = 'name file:line:col' strings (cheapest); full = default compact wire; json = full-fidelity"},
                    "_compact": {"type": "bool", "required": false, "default": true, "description": "(private)"},
                    "_delta": {"type": "bool", "required": false, "default": false, "description": "(private)"}
                }
            },
            "batch-read": {
                "args": {
                    "files": {"type": "array", "required": true, "description": "bd qre0: array of repo-relative paths to read in one call (cap 50)"},
                    "budget_tokens": {"type": "number", "required": false, "default": 2000, "description": "soft aggregate output budget; reading stops and marks truncated/skipped"}
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
                    "wait_gen": {"type": "number", "required": false, "description": "block until generation >= N (5s upper bound); N=0 returns the current snapshot immediately (a cold start's first pull still needs an LS round trip)"}
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
                    "max_results": {"type": "number", "required": false, "default": 100, "description": "bd z0kg: absent → SERENA_DEFAULT_MAX_ITEMS env when set"},
                    "case_sensitive": {"type": "bool", "required": false, "default": false},
                    "comments_only": {"type": "bool", "required": false, "default": false, "description": "I: filter to comment lines only"},
                    "distinct_symbols": {"type": "bool", "required": false, "default": false, "description": "bd zpzw: keep first hit per enriched symbol name (symbol=null rows kept)"},
                    "exclude": {"type": "array[string]", "required": false, "default": [], "description": "glob list to skip (same syntax as path_glob), e.g. ['*_measure.py']"},
                    "no_ignore": {"type": "bool", "required": false, "default": false, "description": "escape hatch: include .gitignore'd files (.git/ and built-in ignore dirs still excluded)"},
                    "format": {"type": "string", "required": false, "default": "full", "description": "bd 51ib: brief = 'file:line:col: text' strings; full/json = default response (adds bd rsqq summary header)"}
                }
            },
            "symbol-body": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true},
                    "meta": {"type": "bool", "required": false, "default": false, "description": "bd b72k: true → aggregate object (doc/signature/location/prev_line/next_line); default keeps bare body string"}
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
                    "top_n": {"type": "number", "required": false, "default": 20, "description": "E: per-file top-level symbol map (top N); bd z0kg: absent → SERENA_DEFAULT_MAX_ITEMS env when set"}
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
                    "end_line": {"type": "number", "required": false, "description": "1-based inclusive"},
                    "no_clamp": {"type": "bool", "required": false, "default": false, "description": "bd 66al: true → strict bounds (end_line beyond EOF = BAD_ARGS); default clamps to EOF (bd mfxg)"},
                    "max_tokens": {"type": "number", "required": false, "description": "bd a14g: soft limit, content 截断按 4B/T 估算（预算 = n*4-32 字节，留余 metadata），超按整行砍（留半行丢），写 truncated:true/total_bytes/total_tokens；0 = BAD_ARGS rc=2（与 07u5 同形）"}
                }
            },
            "list-dir": {
                "args": {
                    "path": {"type": "string", "required": true},
                    "max_depth": {"type": "number", "required": false},
                    "max_entries": {"type": "number", "required": false, "default": 500, "description": "bd z0kg: absent → SERENA_DEFAULT_MAX_ITEMS env when set"}
                }
            },
            "find-file": {
                "args": {
                    "name_pattern": {"type": "string", "required": true},
                    "path_glob": {"type": "string", "required": false},
                    "max_results": {"type": "number", "required": false, "default": 200, "description": "bd z0kg: absent → SERENA_DEFAULT_MAX_ITEMS env when set"}
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
                    "_compact": {"type": "bool", "required": false, "default": true, "description": "(private)"},
                    "_debug_raw": {"type": "bool", "required": false, "default": false, "description": "bd aap4: true (or SERENA_DEBUG_RAW=1) → attach raw_lsp_response 200B snapshot on silent-empty with non-null LS response"}
                }
            },
            "find-referencing-code-snippets": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "line": {"type": "number", "required": true},
                    "col": {"type": "number", "required": true},
                    "context_lines": {"type": "number", "required": false, "default": 3},
                    "max_results": {"type": "number", "required": false, "default": 50, "description": "bd z0kg: absent → SERENA_DEFAULT_MAX_ITEMS env when set"},
                    "_compact": {"type": "bool", "required": false, "default": true, "description": "(private)"},
                    "_debug_raw": {"type": "bool", "required": false, "default": false, "description": "bd aap4: true (or SERENA_DEBUG_RAW=1) → attach raw_lsp_response 200B snapshot on silent-empty with non-null LS response"}
                }
            },
            "replace-text-in-symbol": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true},
                    "old_text": {"type": "string", "required": true},
                    "new_text": {"type": "string", "required": true},
                    "format_on_write": {"type": "bool", "required": false, "default": false, "description": "bd ou83: run textDocument/formatting on the file after a successful write and apply edits to disk (all write tools accept this)"}
                }
            },
            "insert-text-after-symbol": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true},
                    "text": {"type": "string", "required": true},
                    "auto_indent": {"type": "bool", "required": false, "default": true, "description": "bd bt3h: indent continuation lines to the host symbol's indentation (set false to disable)"},
                    "format_on_write": {"type": "bool", "required": false, "default": false, "description": "bd ou83: format the file after a successful write"}
                }
            },
            "insert-text-before-symbol": {
                "args": {
                    "file": {"type": "string", "required": true},
                    "symbol": {"type": "string", "required": true},
                    "text": {"type": "string", "required": true},
                    "auto_indent": {"type": "bool", "required": false, "default": true, "description": "bd bt3h: indent continuation lines to the host symbol's indentation (set false to disable)"},
                    "format_on_write": {"type": "bool", "required": false, "default": false, "description": "bd ou83: format the file after a successful write"}
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
            },
            "create-text-file": {
                "args": {
                    "file": {"type": "string", "required": true, "description": "target path relative to project root; must NOT exist"},
                    "content": {"type": "string", "required": true, "description": "full file content"}
                }
            },
            "undo": {
                "args": {
                    "steps": {"type": "number", "required": false, "description": "number of transactions to roll back (default 1)"},
                    "list": {"type": "boolean", "required": false, "description": "list undo stack overview instead of rolling back"}
                }
            },
            "redo": {
                "args": {}
            },
            "test": {
                "args": {
                    "target": {"type": "string", "required": true, "description": "test target: crate/test dir or test file path (relative to project root or absolute)"},
                    "name": {"type": "string", "required": false, "description": "test name filter (cargo test positional / npm passthrough)"}
                }
            },
            "diff": {
                "args": {
                    "txn_id": {"type": "number", "required": false, "description": "undo txn id (default: most recent active txn)"},
                    "patch": {"type": "boolean", "required": false, "description": "emit unified diff consumable by patch -p1 / git apply"}
                }
            },
            "find-test": {
                "args": {
                    "symbol": {"type": "string", "required": true, "description": "symbol name to locate tests for"}
                }
            },
            "recipe": {
                "args": {
                    "name": {"type": "string", "required": true, "description": "recipe: fix-bug|add-feature|rename|add-test|refactor-extract|refactor-rename|review-diff|explore"},
                    "pos": {"type": "array", "required": false, "description": "positional args; fix-bug/rename/refactor-extract = [file, sym], add-feature = [name], add-test/refactor-rename = [sym], explore = [file] (file, not a directory), review-diff = [txn-id]"},
                    "new_body": {"type": "string", "required": false, "description": "fix-bug: replacement function body (absent = analyze-only chain)"},
                    "to": {"type": "string", "required": false, "description": "rename / refactor-rename: new name"},
                    "as": {"type": "string", "required": false, "description": "refactor-extract: new fn name"},
                    "target": {"type": "string", "required": false, "description": "add-feature: stub target file (.rs)"},
                    "tests_file": {"type": "string", "required": false, "description": "add-feature: test file path (with tests)"},
                    "tests": {"type": "string", "required": false, "description": "add-feature: test code (with tests_file)"},
                    "run": {"type": "boolean", "required": false, "description": "add-test: run test backend after writing template"}
                }
            }
        }
    })
}

/// 工具名 → 参数表对象（catalog().tools 子集）。catalog.rs 测试用。
pub fn catalog_tool_names() -> Vec<String> {
    let v = catalog();
    let tools = v
        .get("tools")
        .and_then(|t| t.as_object())
        .expect("tools object");
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
            let args = tool_def
                .get("args")
                .and_then(|a| a.as_object())
                .expect("args object");
            for (arg_name, arg_def) in args {
                let required = arg_def
                    .get("required")
                    .and_then(|r| r.as_bool())
                    .unwrap_or(false);
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
