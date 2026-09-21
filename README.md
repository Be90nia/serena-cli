# serena-rust

> A position-free, symbol-level code assistant for AI agents — Rust reimplementation of [oraios/serena](https://github.com/oraios/serena), exposed as a CLI + skill (no MCP server required).

`serena-rust` gives an LLM agent the ability to navigate, search, and edit a codebase by **symbol name** (functions, types, fields) instead of by line number. It speaks the Language Server Protocol natively, so the same tool works across Rust, TypeScript, Python, Go, C/C++, C#, and Java without per-language adapters.

This is a focused reimplementation of [Serena](https://github.com/oraios/serena) anchored at upstream commit `43ae0211`, kept current with the subset of features a CLI-based agent actually consumes (no MCP, no Python, no LSP server multiplexing beyond the standard one-session-per-root model).

## Why CLI + skill, not MCP

The upstream Serena project ships as an MCP server: the agent connects via the Model Context Protocol and the tools arrive as MCP method calls. We chose a different delivery:

- **CLI-first** — every capability is a subcommand of `serena-cli`. The agent drives it through `bash`, `serena-cli shell` (JSONL long-lived session), or pipes JSON.
- **Skill-first** — a single skill file (`skills/serena-cli/SKILL.md`) gives the agent the golden-path workflow and the token-discipline rules. No protocol negotiation, no handshake, no `mcp.json`.
- **Single daemon** — one process per workspace, lazily spawned on first command. Idle timeout = 15 min. Stops cleanly on `stop-all` (no zombies).

Trade-off: you lose the "MCP auto-discovery" story. You gain `bash`-debuggability, JSONL streaming, deterministic exit codes, and a 100% reproducible wire format (the 9-error-code contract in `ARCHITECTURE.md`).

## What it covers

### CLI commands (24)

Read / navigate (7): `overview` · `symbol-tree` · `read-file` · `list-dir` · `find-file` · `search` · `hover`

Symbols (8): `def` · `refs` · `find-symbol` · `symbol-body` · `find-implementations` · `find-referencing-symbols` · `find-referencing-code-snippets` · `containing-symbol`

Diagnostics (3): `diagnostics` (with `--wait-gen N`) · pull diagnostics fallback · `signature-help`

Editing (10): `rename-symbol` · `safe-delete-symbol` · `replace-body` · `replace-text-in-symbol` · `insert-text-before-symbol` · `insert-text-after-symbol` · `delete-text-in-symbol` · `insert-at-line` · `replace-lines` · `delete-lines`

Completion (1): `completion` (with `--limit` and per-file-suffix trigger inference)

Admin (4): `status` · `stop-all` · `install <lang>` · `shell` (JSONL stdin/stdout session)

**Position baseline convention**: commands taking `line`/`col` (position-addressed: `def`, `refs`, `hover`, `find-implementations`, `rename-symbol`, `find-referencing-*`, `containing-symbol`, `defining-symbol`, `signature-help`, `code-action`, `document-highlight`, `completion`, `format-range`, `inlay-hint`, `call-hierarchy prepare`, `type-hierarchy prepare`, `moniker`) are **0-based**, passed straight through as LSP `Position`. Line-range and line-editing commands (`read-file`, `insert-at-line`, `replace-lines`, `delete-lines`, `delete-text-in-symbol`) are **1-based inclusive**. Every command also states this in its `--help`.

vs. upstream oraios/serena: 19/19 high-ROI wrappers covered (every tool an agent realistically uses). Not implemented: `documentHighlight`, `codeLens`, `documentLink`, `foldingRange`, `call/type hierarchy`, `moniker`, `semanticTokens`, `inlayHint` — none of these have an agent-side consumer today; see [Phase 6 limitations](#limitations) for the criterion.

### Language servers (7 of 73 in upstream catalog)

| Lang | Server | Status | Notes |
|---|---|---|---|
| Rust | tokio | ready | rustup-aware lookup chain (`rustup which` → cargo bin → PATH), workspace mode |
| C / C++ | clangd | ready | needs `compile_commands.json`; passes `--compile-commands-dir` |
| TypeScript / JS | typescript-language-server | ready | tsconfig walk, ATA off, npm shim fallback |
| Python | pyright | ready (adapter shell) | venv interpreter detection stubbed in `crates/ls-adapters/src/python.rs` |
| Go | gopls | ready (adapter shell) | `go.work` / multi-module dir detection stubbed |
| C# | csharp-ls | ready (adapter shell) | decision log: `local/csharp-ls-decision.md` (upstream migrated to roslyn LS) |
| Java | jdtls | ready (auto-download) | ~100MB download, JVM arg templates from `crates/ls-runtime/src/install.rs` |

Code is present for all 7; **end-to-end smoke verified on 2/7** (rust + typescript) — the others need the corresponding binary installed locally. CI 7-language smoke script lives in `scripts/ci_smoke.sh`.

## Install

```bash
# From source (single binary, no runtime deps beyond `rustup` itself)
git clone https://github.com/<you>/serena-rust.git
cd serena-rust
cargo install --path crates/cli --locked

# Verify
serena-cli --help
```

### First-run language server setup

`serena-cli install <lang>` walks `servers.toml` — for each supported language it tries:

1. **PATH probe** — `which <bin>`, accepts `rustup which <bin>`, etc.
2. **Download** — GitHub release asset / npm tarball / uvx wheel, depending on the language's `InstallSpec` (A-class binaries, B-class npm, C-class uvx, D-class dotnet tool, E-class gem, F-class source build, G-class path-only).
3. **SHA-256 verify** — hashes are anchored to the upstream SolidLSP matrix in `local/ls-download-matrix.md`. Mismatched bytes fail closed with `LS_NOT_INSTALLED`.

Hand-installed T2 servers (rustup toolchain, system Python, etc.) are also accepted — `install` is convenience, not a gate.

## Golden path (8 commands, ~90% of agent traffic)

```bash
# 1. Where am I? (skips reading the whole file)
serena-cli overview <file>

# 2. What is this symbol?
serena-cli symbol-body <file> <symbol>
serena-cli hover <file> <line> <col>

# 3. Where is it used? Where is it defined?
serena-cli refs <file> <line> <col>
serena-cli def <file> <line> <col>
serena-cli find-referencing-symbols <file> <line> <col>

# 4. Edit by symbol name, not by line number
serena-cli replace-body <file> <symbol> --with '<new body>'
serena-cli replace-text-in-symbol <file> <symbol> '<old>' '<new>'

# 5. Verify
serena-cli diagnostics <file>
serena-cli safe-delete-symbol <file> <symbol>   # refuses if referenced
```

For sustained work, use `serena-cli shell` (stdin/stdout JSONL) — keeps the daemon warm, sub-ms cached lookups, no per-command spawn overhead.

See [`skills/serena-cli/SKILL.md`](skills/serena-cli/SKILL.md) for the full token-discipline guide.

## Performance baseline

Measured on `feature/solidlsp-phase0-1`, 2026-09-16, Windows 11 / i9-10900F, no competing rust-analyzer:

| Workload | Latency |
|---|---|
| `find-symbol` warm daemon, cached | < 1 ms |
| `overview` warm daemon, first hit | 65 – 108 ms (CLI spawn) / 0.88 – 0.93 ms (shell mode) |
| `find-referencing-symbols` warm | ~70 ms |
| `symbol-tree <dir>` 2-file aggregate | 0.34 s |
| `rename-symbol` cold start (rust fixture workspace) | 162 ms (was 30s+ pre-fix) |
| Cold daemon first overview (rust fixture, workspace mode) | ~5 s (was 89 s with no workspace / 300 s+ with VS Code competing) |

`cargo test --workspace`: 49 test targets green, 30+ new unit tests added. `cargo clippy --workspace --all-targets -- -D warnings`: 0 errors.

## Architecture in 30 seconds

7-crate Cargo workspace:

- `crates/lsp-core` — JSON-RPC framing, request/response client with id normalization, `ContentModified` retry
- `crates/runtime` — managed LSP process spawn, 3-pump topology (mirror of upstream `ls_process.py`)
- `crates/transport` — stdio / TCP transport
- `crates/registry` — `LanguageServerId` ↔ file extension table
- `crates/ls-adapters` — per-LS launch args + readiness probes (rust-analyzer / clangd / pyright / gopls / typescript / csharp-ls / jdtls)
- `crates/supervisor` — `Supervisor` trait + `DaemonSupervisor` impl: per-key load gate, session cache, tool dispatch, write gate, symbol cache
- `crates/daemon` — HTTP front (axum), 9-error-code wire contract, singleton lock, idle reaper, graceful shutdown
- `crates/cli` — clap subcommands, `--json` mode, `shell` JSONL session, `install`, management commands
- `crates/ls-runtime` — `deps.rs` (download/SHA), `install.rs` (auto-install flow), `servers.toml` adapter

The single source of architectural truth is [`ARCHITECTURE.md`](ARCHITECTURE.md). The 9 error codes (`internal`, `not_found`, `invalid_request`, `unauthorized`, `rpc_error`, `timeout`, `ls_spawn_failed`, `ls_not_installed`, `unsupported`) are a wire contract — do not rename, do not split, do not merge.

## Limitations

| Limitation | Impact | When to revisit |
|---|---|---|
| SHA-256 download matrix for non-clangd LS is partial | `install` works for verified entries; unverified entries fall back to PATH probe | When CI needs hermetic installs — populate `local/ls-download-matrix.md` from upstream SolidLSP source |
| 5 of 7 adapter shells not smoke-tested locally | Works in unit tests, needs binary locally for `cargo run` smoke | Add `scripts/ci_smoke.sh` runner to CI matrix |
| No monorepo multi-root support | Each `serena-cli` invocation scopes to one workspace root | Add `additionalWorkspaceFolders` (Phase 4 stretch) |
| No `$/progress` notification buffering | Long-running tools block until complete | When tools like refactor cross 30s boundaries |
| Per-LS global timeout only | A slow single file blocks the whole session | Add per-call `timeout_ms` arg |
| `typescript-language-server` 7.x incompatible | LSP returns `-32603` (no `tsserver.js`) | Pin `typescript@<6` in fixture / user projects |
| typescript-language-server on Windows: npm shim must be `.cmd` | Bare-name shim is a `sh` script, not spawnable | Already enforced — see Task 20 commit `c0e3cea` |

Full Phase 6 limitation table with rationale: [`local/solidlsp-development-plan.md`](local/solidlsp-development-plan.md) § Phase 6.

## Token discipline (why this exists)

Reading a file to find a line number, then reading again to confirm the edit, then reading once more to verify the change — that's 3 file reads to make one edit. With symbol-addressed tools:

- `symbol-body <file> <symbol>` returns the function body with no line-number dance
- `replace-body` carries `--expected-hash` for optimistic concurrency control without round-tripping
- `search --max-results 50` keeps grep-style work bounded
- `shell` (JSONL) reuses the warm daemon across many small commands

Rough rule of thumb: a 20-step task using `read-file` consumes ~10× more tokens than the same task using `overview` → `symbol-body` → `replace-body`. The skill file is the canonical reference; the README is the elevator pitch.

## Testing

```bash
# All tests
cargo test --workspace

# Real install path (requires network + npm)
SERENA_TEST_DOWNLOAD=1 cargo test -p ls-registry --test npm_install_e2e

# Lint (CI gate)
cargo clippy --workspace --all-targets -- -D warnings
```

## Acknowledgements

- [oraios/serena](https://github.com/oraios/serena) — the original Python MCP server this reimplementation is modeled on. Anchored at upstream commit `43ae0211`.
- [helix-editor/helix](https://github.com/helix-editor/helix) — the `find_lsp_workspace` algorithm in `crates/supervisor/src/root.rs` is a direct translation.
- The Cargo dependency tree (jsonrpc-core, tokio, axum, lsp-types, ...) — see `Cargo.lock`.

## License

[MIT](LICENSE)

## Detailed data

End-to-end PR description, commit-by-commit metrics, and Round 2 backlog: [`local/PR-description.md`](local/PR-description.md).