# upstream SolidLanguageServer @43ae0211 API surface

> 锚: `oraios/serena@43ae0211d7f3bba4101cd0552707fa21d37f4c84` (2026-08-30)
> 来源: `src/solidlsp/ls.py` (3256 行), `src/solidlsp/lsp_protocol_handler/lsp_requests.py` (561 行)
> 仅列**公开方法**（无前导下划线、非 dunder）+ 内部高层抽象；钩子/缓存/factory 见源码。

## A. SolidLanguageServer 公开方法（55 个，去重后）

### A.1 符号查找与导航（13）
| # | 方法 | LSP method | 入参 |
|---|------|------------|------|
| 1 | `request_hover` | `textDocument/hover` | file, line, col |
| 2 | `request_signature_help` | `textDocument/signatureHelp` | file, line, col |
| 3 | `request_definition` | `textDocument/definition` | file, line, col |
| 4 | `request_implementation` | `textDocument/implementation` | file, line, col |
| 5 | `request_references` | `textDocument/references` | file, line, col |
| 6 | `request_document_symbols` | `textDocument/documentSymbol` | file |
| 7 | `request_full_symbol_tree` | (documentSymbol × all files) | dir? |
| 8 | `request_dir_overview` | (documentSymbol × dir) | dir |
| 9 | `request_document_overview` | documentSymbol + filter top-level | file |
| 10 | `request_overview` | (documentSymbol × path) | path |
| 11 | `request_workspace_symbol` | `workspace/symbol` | query |
| 12 | `request_symbol_at_location` | (documentSymbol + find_by_range) | file, line, col |
| 13 | `request_defining_symbol` / `request_implementing_symbols` | definition + symbol-at | file, line, col |
| 14 | `request_referencing_symbols` | references + symbol-name normalize | file, line, col |
| 15 | `request_containing_symbol` / `request_container_of_symbol` | (documentSymbol + walk ancestors) | file, line, col |
| 16 | `create_symbol_body` | (派生，无 LSP 调用) | UnifiedSymbolInformation |

### A.2 补全与编辑（6）
| # | 方法 | LSP method | 备注 |
|---|------|------------|------|
| 17 | `request_completions` | `textDocument/completion` | + resolve_completion_item |
| 18 | `request_rename_symbol_edit` | `prepareRename` + `textDocument/rename` | 提交前 prepare 校验 |
| 19 | `apply_text_edits_to_file` | (无 LSP 调用) | 落盘 `TextEdit[]` |
| 20 | `insert_text_at_position` | (无 LSP 调用) | 编辑辅助 |
| 21 | `delete_text_between_positions` | (无 LSP 调用) | 编辑辅助 |
| 22 | `open_file` | `textDocument/didOpen` | 文档打开 |

### A.3 诊断（4）
| # | 方法 | LSP method | 备注 |
|---|------|------------|------|
| 23 | `request_text_document_diagnostics` | `textDocument/diagnostic` (pull 3.17) | 带 sub-support 检查 |
| 24 | `request_published_text_document_diagnostics` | (push + wait generation) | 大文件 fallback |
| 25 | `get_cached_published_text_document_diagnostics` | (cache only) | |
| 26 | `get_published_diagnostics_generation` | (generation counter) | |

### A.4 缓存与生命周期（10）
`save_cache` / `start` / `stop` / `is_running` / `set_request_timeout` /
`get_ignore_spec` / `get_source_fn_matcher` / `is_ignored_path` /
`start_server_context` / `close` / `ensure_open_in_ls`

### A.5 LSP 标准 method 全集（lsp_requests.py 暴露的 45 个 request + 25 notification）

**textDocument/ (28)**: `implementation` `typeDefinition` `documentColor` `colorPresentation`
`foldingRange` `declaration` `selectionRange` `prepareCallHierarchy` `incomingCalls`
`outgoingCalls` `linkedEditingRange` `moniker` `prepareTypeHierarchy`
`typeHierarchySupertypes` `typeHierarchySubtypes` `inlineValue` `inlayHint`
`resolveInlayHint` `diagnostic` `willSaveWaitUntil` `completion`
`resolveCompletionItem` `hover` `signatureHelp` `definition` `references`
`documentHighlight` `documentSymbol` `codeAction` `resolveCodeAction`
`codeLens` `resolveCodeLens` `documentLink` `resolveDocumentLink`
`formatting` `rangeFormatting` `onTypeFormatting` `rename` `prepareRename`

**workspace/ (10)**: `willCreateFiles` `willRenameFiles` `willDeleteFiles`
`symbol` `resolveWorkspaceSymbol` `diagnostic` `executeCommand`
`didChangeConfiguration` `didChangeWorkspaceFolders`

## B. 上游 ls-adapters 落地表（73 个 LS）

73 = 上游 `language_servers/` 73 个适配器（`common.py` 非适配器；MSL 两文件算 1 个；elixir 在子目录）。详见 `local/upstream-ls-catalog.md`（权威）。

| 语言 | 上游 | 本项目 |
|---|---|---|
| C/C++ | clangd / ccls | ✅ clangd |
| Python | pyright / basedpyright / jedi / pyrefly / ty | ✅ pyright |
| Go | gopls | ✅ gopls |
| TypeScript | typescript-language-server / deno / vtsls / vue / svelte / angular | ✅ typescript |
| C# | csharp(roslyn) / omnisharp | ✅ csharp_ls |
| Java | eclipse-jdtls | ✅ jdtls |
| Rust | rust-analyzer | ✅ rust_analyzer |
| PHP | intelephense / phpactor / phpantom | ❌ |
| Ruby | ruby-lsp / solargraph | ❌ |
| Kotlin / Scala / Swift / Lua / Elixir / Bash / Dart / Terraform / YAML / JSON / Markdown / PowerShell / Haskell / OCaml / ... | 各 1-2 个 | ❌ |

**缺口 66 个**（73 − 7）。安装机制分类与逐个 URL 模板见 `local/upstream-ls-catalog.md`。

---

## B. 上游 ls-adapters 落地表（71 个 LS）

7 + 64 = 71（**注**：本项目仅 7 个，缺口 64）。

| 语言 | 上游 | 本项目 |
|---|---|---|
| C/C++ | clangd / ccls | ✅ clangd |
| Python | pyright / basedpyright / jedi / pyrefly / ty | ✅ pyright |
| Go | gopls | ✅ gopls |
| TypeScript | typescript-language-server / deno | ✅ typescript |
| C# | csharp-ls / omnisharp | ✅ csharp_ls |
| Java | eclipse-jdtls | ✅ jdtls |
| Rust | rust-analyzer | ✅ rust_analyzer |
| PHP | intelephense / phpactor / ph pantom | ❌ |
| Ruby | ruby-lsp / solargraph | ❌ |
| Kotlin | kotlin-language-server | ❌ |
| Scala | metals | ❌ |
| Swift | sourcekit-lsp | ❌ |
| Lua | lua-ls / luau-lsp | ❌ |
| Elixir | elixir-tools (elixir-ls 后继) | ❌ |
| Bash | bash-language-server | ❌ |
| 其他 50+ | terraform / vue / svelte / angular / dart / godot / haskell / ocaml / ... | ❌ |
