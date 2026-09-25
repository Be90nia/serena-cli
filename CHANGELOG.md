# Changelog

All notable changes to `serena-rust` are recorded here, ordered by phase. Anchored to upstream `oraios/serena@43ae0211`. Commit hashes reflect `feature/solidlsp-phase0-1` at the time of this writing; run `git log --oneline | head -80` for the canonical list.

## Unreleased

| commit | summary | key metric |
|---|---|---|
| (uncommitted) | jdtls auto-install wired into `launch_info` (P-1 fix, 「机制存在≠接线生效」 third instance): PATH-miss + no pre-install dir → `spawn_blocking(ensure_jdtls_installed)` → DownloadInstaller (cache short-circuit = idempotent) → `java -jar equinox` launch; spec fixed to real snapshot layout (P-2: tar top level IS the payload — `strip_components` 1→0, short-circuit probe pinned launcher jar → `bin/jdtls`); sha gate jdtls-exception (rolling `latest` symlink, no companion hash file on eclipse.org — trust anchor = HTTPS + allowed_hosts, allow_unsigned_sha in-code with rationale); docs JRE 21+ → 25+ (P-3: current snapshot equinox requires JavaSE 25, JDK21 dies at OSGi resolve); doctor jdtls check now probes install cache and reports the true auto-download behavior (was: nonexistent `serena-cli install jdtls`); cache root single source of truth sunk to `ls_runtime::install::default_cache_root` (ls-registry delegates; adapters can't depend on ls-registry — cycle) | pre-install moved away + PATH w/o jdtls: cold `overview App.java` auto-downloads 51MB to `%LOCALAPPDATA%\serena\ls\jdtls\latest\` and returns 4 symbols in the 90s window; 2nd call serves from cache (no network); URL-unreachable → LS_NOT_INSTALLED (not panic/hang, 30s connect / 600s total timeout) |
| (uncommitted) | pyright diagnostics pipeline fix: `diag_uri_key` normalizes push-uri cache keys with percent-decode + lowercase (pyright pushes `file:///c%3A/...`, path_to_uri generates `file:///C:/...` — lowercase-only normalization never matched, python diagnostics were constant `items:[]` `pending:true`; RA/clangd push forms are decode no-ops, behavior unchanged) | broken.py: `items:[] pending:true` (even 150s after push) → 2 exact errors `pending:false`; go/c regression clean; regression test `diag_uri_key_normalizes_percent_encoded_and_cased_uris` |
| (uncommitted) | +4 languages (user-directed batch): bash (bash-language-server@5.6.0 npm), json (vscode-json-languageserver@1.3.4 npm), powershell (PowerShellEditorServices@4.4.0 download; bundled_modules_path = install_dir — upstream py bug not mirrored, PSScriptAnalyzer diagnostics verified live), vue (@vue/language-server@3.1.5 hybrid — companion TS LS with @vue/typescript-plugin, semantic routing live; diagnostics via tsserver bridge pending, filed) + LanguageId Bash/Json/PowerShell/Vue registry + file_detect extensions + doctor npm probe root-fix (which_no_unc bare-name shim shadowed npm.cmd) + doctor LS entries | 11/11 real-server smoke (per-adapter reports `local/report-*-adapter.md`); workspace green, clippy clean |
| (uncommitted) | 9.5 campaign: 7/7 languages real-server smoke (c# via csharp-ls 0.15.0, java via jdtls snapshot — `local/report-ls-smoke-7of7.md`) + jdtls auto-download wired (ensure_jdtls_installed was dead_code with zero call sites — launch_info path4 now spawns it; strip_components 1→0 per real snapshot layout; JRE 25+ documented; doctor hint truthful) + timing-flaky campaign (write_gate oneshot signal sync replaces sleep guess, 2 wall-clock thresholds raised with headroom notes; 6 full runs 626/0 — `local/report-flaky-campaign.md`) + P0-B reverified at HEAD (p0b_fix_verify ×2 PASS, triple cold overview 634/403/302ms no channel-closed) | README claims 7/7 with zero caveats; workspace green ×multiple runs, clippy clean |
| (uncommitted) | bilingual docs (user-directed): new `README.zh-CN.md` full translation with language-switch links (EN remains source of truth) + language coverage 2/7 → 5/7: c/cpp (clangd, single-file granularity works without compile_commands), python (pyright), go (gopls v0.23.0) real-machine smokes all pass overview/def+hover/diagnostics (`local/report-ls-smoke-5of7.md`); acceptance issue 7 (nonexistent file → INTERNAL) confirmed already fixed in ef22eb (BAD_ARGS rc=2); write-after-index staleness confirmed fixed (explicit stale warning + self-heal) | 3/3 smokes PASS; new lead filed: python diagnostics pipeline returns empty `pending:true` for pyright — follow-up |
| (uncommitted) | docs accuracy batch (user-perspective audit): README position-baseline corrected 0-based → 1-based (bd serena-rust-7xv contract), command count 24 → 52 + long-tail list added, stale "Not implemented" claim removed (long-tail tools landed & verified 2026-09-23); global `--json` help fixed (default is compact JSON, flag disables compact — was claiming "default human-readable"); `diagnostics` empty help text filled | README no longer contradicts CLI help; zero behavior change |
| (uncommitted) | dogfood UX batch (bd serena-rust-bxd): `wait-ready --stage symbol\|semantic` + `SERENA_WAIT_READY_TIMEOUT_SECS` + staged stderr progress; CLI surfaces `[warn]`/`[hint]` on cold-start empty results; `find-referencing-code-snippets --symbol NAME` one-step resolution (documentSymbol cache → workspace/symbol fallback, multi-hit warning, cold-window warming hint + 1 retry); LS warmup window (10s or first semantic success) stamps find-symbol with `index warming: results may be partial`, false-empty results skipped from Phase 3.1 cache | large-repo symbol-stage ready 0.0s (was 120s timeout); one-step refs == two-step identical; cold-window BAD_ARGS no longer misleading |
| (uncommitted) | ContentModified(-32801) client-layer retry whitelist wired in production (bd serena-rust-s3u): `Session::start` registers positional methods via shared `init_params::RETRY_ON_CONTENT_MODIFIED` (single source of truth with `stale_request_support` declaration) | concurrent hover error rate 1.0% (12/1200, stable across 2 rounds) → 0 |
| (uncommitted) | percent-encoded path_to_uri (bd serena-rust-cbd): non-ASCII/space/`#`/`?`/`%` UTF-8 percent-encoded in `docsync::path_to_uri_str` (`percent-encoding` crate promoted to direct dep) | emoji filename `😀.rs` overview: INTERNAL → symbols returned with valid `%F0%9F%98%80.rs` URI |
| (uncommitted) | daemon orphan race fix (bd y2y): lock ownership-checked removal (`remove_owned` pid+boot_ms), stale-grace probing (3×300ms) before takeover, bind-before-lock arbitration, CLI 403 token self-heal (`refresh_token_if_stale`) | stop-all × lazy-spawn cross: 403 = 0 (was 400/400 calls), orphan daemons = 0, lock↔listener consistency held across daemon generations |
| (uncommitted) | `wait-ready` subcommand (bd serena-rust-55m): blocks until type analysis truly usable (overview first-symbol → hover non-empty we0-verdict), 500ms→2s backoff, stderr progress; timeout exit 4 | AI/e2e scripts drop ~20 lines of hand-rolled polling each |
| (uncommitted) | DAEMON_DRAINING client-side self-heal (bd serena-rust-g0m): forward classifies 503 + wire code, ≤5s window × 300ms retries full chain incl. lazy-spawn re-probe; beyond window original rc=3 error preserved | stop-all → immediate tool call succeeds instead of hard rc=3 |
| (uncommitted) | configurable idle policy (bd serena-rust-j8b): `SERENA_IDLE_TIMEOUT_SECS` (global self-kill, default 900) + `SERENA_LS_IDLE_EVICTION_SECS` (LS eviction, default 600), `0` = never; invalid values warn+default; default behavior unchanged | AI batch jobs (90min) stop paying 30-90s cold restarts per idle eviction |
| (uncommitted) | response token estimate (bd serena-rust-7rh): tools_post success envelope gains `"~tokens": N` (serialized bytes / 4, no tokenizer dep); `SERENA_NO_TOKEN_ESTIMATE=1` opts out; error responses / /batch unchanged | AI agents get a cost feedback signal on every tool response |

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
