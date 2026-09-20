# Changelog

All notable changes to `serena-rust` are recorded here, ordered by phase. Anchored to upstream `oraios/serena@43ae0211`. Commit hashes reflect `feature/solidlsp-phase0-1` at the time of this writing; run `git log --oneline | head -80` for the canonical list.

## Phase 0 · Stability foundations (P0)

| commit | summary | key metric |
|---|---|---|
| `b6525e0` | Phase 0 P0 stability — sha256 real verify + cold-start fix + graceful shutdown + note_activity | cold-start first req 120s → 15s; daemon no zombies after `stop-all`; empty catch-all cleared |
| `821cd9c` | Phase 0.2 cold-start probe rewritten to real project files | 6 adapter probes validated; virtual URI fallback removed |
| `fbe21d5` | Phase 3.2 per-LS readiness / index wait (fix rename 30s timeout + replace-body position drift) | rename 162ms success; replace-body lands at correct symbol |
| `4b7ebf4` | Phase 3.1 document symbol cache (overview / find-symbol / symbol-body) | repeat overview 0.88-0.93ms (shell) / 19-21ms (CLI spawn) |
| `de6cfa3` | Phase 3.3 ignore spec for find-file / list-dir / symbol-tree | filters 21 dirs (venv / node_modules / target / dist / …) |

## Phase 1 · Wiring closeouts (P1)

| commit | summary | key metric |
|---|---|---|
| `dd39f43` | Phase 1.1 completion wiring closed (consumes `local/completion-design.md`) | 5 unit tests + real CLI smoke (`printf` in C++) |
| `c0d841a` | Task 21 dual-path wiring + `install` command + Task 20 coverage gate sealed | `install <lang>` end-to-end on PATH probe + download + SHA verify |

## Phase 2 · Upstream wrapper gap (P1-P2, by ROI)

| commit | summary | key metric |
|---|---|---|
| `2b80433` | Phase 2.1 `containing-symbol` — position → deepest containing symbol | 5 unit tests + real CLI smoke |
| `66926f8` | Phase 2.2 `signature-help` — function call signature hint | 3 unit tests + smoke (`add(int a, int b)`) |
| `90976ac` | Phase 2.3 `defining-symbol` — def + symbol refinement | 4 e2e + smoke (`add in math.h`) |
| `24aa867` | Phase 2.4 diagnostics generation API — `diagnostics --wait-gen N` | 5 unit tests; default behavior 100% backward compatible |
| `76aa522` | Phase 2.5 pull diagnostics probe + transparent fallback | 10 unit tests; LS without pull support still works |
| `ad65d09` | Phase 7.2 cross-file `symbol-tree <dir>` | 2 unit tests; 2-file aggregate 0.34s |

## Phase 3 · Performance & correctness infra

| commit | summary | key metric |
|---|---|---|
| `fbe21d5` | per-LS readiness wait (see Phase 0 — listed under both phases) | rename 162ms success |
| `4b7ebf4` | document symbol cache (see Phase 0 — listed under both phases) | repeat overview <1ms |
| `de6cfa3` | ignore spec (see Phase 0 — listed under both phases) | 21 ignore patterns active |

## Phase 4 · Adapter depth (P2)

| commit | summary | key metric |
|---|---|---|
| `f364715` | Phase 4.1 rust-analyzer lookup upgrade — rustup which → cargo bin → PATH + capability probe | +2 unit tests; rustup-first preferred |
| `62cc336` | Phase 4.3 TypeScript adapter depth — tsconfig walk + ATA off + npm shim fallback | 5 unit tests; e2e cold ~5s, hot 65-108ms |
| `3a8ae48` | rust_demo fixture gains `Cargo.toml` (real workspace mode) | cold start 89s → 5.07s (17×); hot 274ms |

## Phase 5 · Install mechanism infra

| commit | summary | key metric |
|---|---|---|
| `0906842` | Task 18 download / install infra — auto-install §3/§5 three-pump | GitHub release 302 Location migration handled (objects → release-assets domain); Windows rename lock semantics enforced (single-test caught it) |
| `28fac4e` | Task 19 `servers.toml` schema + `ConfigAdapter` | `ServerSpec → InstallSpec` mapping + override priority |
| `f5ca3b9` | Task 20 G-class `path_only` batch (14/18) | path-only install flow validated |
| `c0e3cea` | npm / uvx installer mechanisms (Task 20 B/C/D/E class paving) | npm 11.12.1 real-install `bash-language-server` 7.9s PASS |
| `8042e3a` | Task 20 A-class 24 download-shape entries recorded into `servers.toml` | 24 A-class entries available |

## M3 · Editing closure (cross-phase)

| commit | summary | key metric |
|---|---|---|
| `b86b777` | M3 editing closure — `safe-delete` + line-level trio + test fixture updates | all 3 readers + 1 writer concurrency test green |
| `d9de43c` | rename cross-file (`textDocument/rename`) | walks project root, edits atomically |
| `836a869` | find-implementations (`textDocument/implementation`) | per-LS support probe before dispatch |
| `7e45c19` | find-symbol (`workspace/symbol`) cross-file | cache-aware |
| `d03220e` | search-for-pattern codebase grep + fix execute_tool missing branches | `replace-body` / `symbol-body` paths now reach |
| `05a4868` | read-file / list-dir / find-file (pure fs) | ignore spec active |
| `580f28c` | find-referencing-symbols / code_snippets | context-lines configurable |
| `38f4922` | insert / replace / delete in symbol (Task 25) | hash-verified write gate |

## Wire contract / public API / lint hygiene

| commit | summary | key metric |
|---|---|---|
| `f759cbd` | `ToolError::Launch` split into Launch / Serialize / Protocol; wire mapping Serialize→Internal, Protocol→RpcError, Launch→LS_SPAWN_FAILED; CLI forward takes `wire_error_code_to_exit` (audit #11 dead-code revival) | 9-error-code wire contract preserved; e2e + evidence shipped |
| `10f38c8` | Daemon watcher exits process after reaper (fix `stop-all` zombie lock + permanent `/tools` 503); `Outcome::Won` carries token, no `unwrap_or_default` silently disabling auth | stop-all → restart → /tools immediately 200 |
| `62732c2` | rustfmt sweep + `.codebase-memory` gitignore | style consistent; MCP-side artifacts ignored |
| `4e43ad6` | Drop MCP stdio subcommand (user-chosen CLI + skill only route) | source tree smaller; DESIGN.md §3.2 verdict aligned |

## Adapter test fixtures & e2e scaffolding

| commit | summary | key metric |
|---|---|---|
| `8fb9f38` | 7-language adapter shells (rust-analyzer / clangd / pyright / gopls / typescript-language-server / csharp-ls / jdtls) + clangd C support | all 7 adapter files present |
| `6903d05` | 6 adapter unit tests + shared helper + 6 fixtures | adapter shell logic covered |
| `d853fe5` | M1 e2e bug-fix rollup | hot-daemon paths green |
| `3dc4442` | lsp-core record / replay e2e (`mock_ls` + JSONL verify) | replays assert framing + id normalization |
| `5bd7151` | supervisor Task 25 hardening — 4 cases (boundary / unknown / concurrent serialization / write-back consistency) | 4 new unit tests |
| `aedd8a7` | `tool_diagnostics` ensure_open + key.root sync; remove debug eprintln | root canonicalization done in supervisor |

## Documentation

| commit | summary | |
|---|---|---|
| `55a9563` | serena-cli skill — 8-command golden path + token discipline | |
| `ab6a656` | SolidLSP gap matrix + upstream API / adapter catalog + Phase 0/1 patrol + cold-start diagnosis | |
| `acd508e` | gap matrix + development plan brought in sync with actual completion | |
| `422fa68` | Phase 2 wrapper gap patrol — all 5 tools PASS | |
| `82cdeff` | Phase 5-6 of gap matrix / plan synced with reality | |
| `9556d3f` | gap matrix forward path — Task 18 download infra landed | |
| `275e4d5` | Upstream feature coverage doc regenerated (old 16% stale; new split "to-do / not-to-do" yields 20/22 ≈ 91%) | |

## Verification baseline (at this revision)

- `cargo test --workspace` — 49 test targets green
- `cargo clippy --workspace --all-targets -- -D warnings` — 0 errors
- 9-error-code wire contract preserved
- 0 new third-party dependencies (ARCHITECTURE §8 upheld)
- Public API not broken (trait default-empty / default-impl pattern)
- Real CLI smoke on every changed path
- Hot daemon 65-108 ms; cold start ~5 s; cached repeat <1 ms

## Known limitations (forward to Phase 6 of `local/solidlsp-development-plan.md`)

- SHA-256 matrix partially populated (clangd complete; rust-analyzer / others pending real install demand)
- 5/7 adapter shells not smoke-tested locally (rust + typescript only)
- Same-host VS Code rust-analyzer indexing causes CPU contention (fixture cold start can reach 300s+)
- `typescript-language-server` 7.x incompatible (pin `typescript@<6`)

Round 2 backlog (P0 data fill → P1 venv / clangd compile_commands / gopls go.work → P2 codeAction / formatting → P3 7-lang CI smoke) — see `local/PR-description.md` § Round 2 candidates.