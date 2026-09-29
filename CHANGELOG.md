# Changelog

All notable changes to `serena-rust` are recorded here, ordered by phase. Anchored to upstream `oraios/serena@43ae0211`. Commit hashes reflect `feature/solidlsp-phase0-1` at the time of this writing; run `git log --oneline | head -80` for the canonical list.

## Unreleased

| commit | summary | key metric |
|---|---|---|
| (pending) | **+4 languages: docker/sql/pgsql/mysql** (bd 56a, Δ self-designed — upstream has none): docker → `docker-langserver` npm (full trio: overview/hover/def live); sql → `sqls` download w/ GitHub digest sha anchor (syntax+format live); pgsql → `pgls` 0.25.7 (libpg_query real syntax diagnostics); mysql → `sqls` dialect entry; `--lang` explicit routing live for all four; SQL-family symbol layer = no-DB capability boundary (-32601), unlocked by user DB config; **pre-existing P1 fixed**: `ensure_launch` 5 package-manager exits dropped exe from argv → all T0 languages LS_SPAWN_FAILED (bash/json masked by hand-written T2); Dockerfile filename-variant detection in file_detect | workspace green, clippy `-D warnings`; PM-verified docker overview=7 directives + hover EXPOSE official docs; pgls 2 real syntax errors pending:false; install ×4 ok:true |
| (pending) | **45-language campaign CLOSED — full-catalog expansion, CI-verified** (bd 7bb-续/W3-W5): matrix 69 doors = **53 CI-PASS real doors + 16 evidence SKIPs** (taxonomy LICENSE/PLATFORM/HOST/BUDGET, every SKIP carries run-log evidence, ledger frozen); 53 of 73 upstream languages covered; per-platform `bin_path`/`url`/`archive` schema; smoke framework: baseline assertion + capability-aware fallback + BUDGET watchdog + verdict-line audit (door count == verdict count, anti-fake-green) + LS stderr/record.jsonl persistence + artifact upload; sharded budget-weighted LPT matrix; **product bugs found by CI**: lsp-core workspace/configuration null→array (pyright/FSAC class), CLI probe default --project, pyright languageId must be official `python`, terraform-ls `serve` subcommand, install.rs 600s download cap, ensure_launch argv exe; doors landed across W1-W5: toml/terraform/cue/nix/ansible/rego/nextflow/svelte/deno/sass/php/lua/scala/haskell/ocaml/erlang/fortran/pascal/perl/r/crystal/zig/gleam/qml/lean4/julia (groovy = no-LS-exists, angular precedent; gdscript/msl migrated out — mIRC script not Metal, 73-catalog correction); **pyright CI-linux hang SOLVED**: root cause = pyright ≤1.1.403 headless + initialize workspaceFolders wedges the analysis engine (bisected with bare-probe matrix); fix = version floor 1.1.414 (uvx + npm pins, coverage-locked); falsified along the way: workspace/configuration array, languageId, didOpen-first ordering, pythonPath injection (kept as upstream-alignment); **final CI run 36594970043 @ e96319e: 6/6 shards green, 53 PASS / 16 SKIP / FAIL=0; M0 CI green** | M0 CI green @ master; local workspace 67 suites + clippy `-D warnings` clean; anchors: coverage count 73, README 53-of-73 ×2 |
| (pending) | **stop-all residual-daemon sweep** (bd 3ab): lock-absent branch now port-probes :7860 → netstat PID lookup → image allowlist (serena-cli/cli.exe) → taskkill → recheck; every failure branch warns on stderr; doctor daemon check gains "lock absent but port listening" WARN | failing-before: real residual PID kept listener after `daemon: not running`; passing-after: probe→warn→terminate→port released; 6 new unit tests |
| (pending) | **+6 languages: kotlin/dart/yaml/markdown/html/css** (bd 7bb, 56a follow-up): kotlin → upstream-managed JetBrains LSP 263.4702.0 (346 MiB, sha anchor = upstream hashes json, bundled JBR no system JDK; same-file hover/def/documentSymbol live, cross-file needs gradle/maven project import — LS boundary); dart → full SDK download (206 MiB, pinned upstream 3.7.1; full semantics on bare folder); yaml → yaml-language-server npm (syntax diagnostics live without schema); markdown → marksman download (link def/refs/hover when project root detectable — marksman ignores client rootUri, git root/`.marksman.toml` unlocks); html/css → vscode-langservers-extracted npm shared package, mdn-driven hover/completion; **pre-existing bug fixed**: install.rs download total-timeout 600s killed 200-350 MiB downloads (dart first-install) → 1800s; `expand_exec` +`{bin_dir}` placeholder | 67 suites green, clippy `-D warnings`; PM-verified all 6 live (yaml tab diag pending:false, markdown cross-file refs, html/css mdn hover, kotlin/dart signature hover); README ×2 → 20 of 73 |
| (pending) | **`uninstall <lang>` subcommand** (bd 2gc): removes `{cache_root}/{id}/` (all versions/variants) behind `ensure_within_cache_root` safety gate (both sides canonicalized + prefix check — refuses path-traversal ids, symlink/junction escapes, and anything resolving outside cache root; unit-tested); path_only/uvx report "not serena-managed"; not-installed rc=3; `--json` = {ok, lang, removed_dirs, bytes_freed}; lint_shell TOOL_NAMES reconciliation + README 56-command count | PM-verified: uninstall html → dirs removed → 2nd call not-installed → hover LS_NOT_INSTALLED; full workspace green + clippy clean |
| (pending) | **half-install probe fix** (bd re5): ① install side — `clear_half_installed`: "dir present + exe missing" = interrupted-install residue, wiped before reinstall (wired after sha gate in all 5 installer exits: Download/Npm/Dotnet/Gem/Source; unauthorized-refusal path never touches user cache — ordering-locked unit test); ② supervisor side — `launch_exe` registry: session-cache hits re-verify the LS exe is still on disk, evicting live sessions whose exe vanished (uninstall/half-pack) so they re-probe cold instead of serving stale semantics; **bonus**: fixed latent same-lock re-entry deadlock in the hit branch (edition-2024 if-let temporary guard; HANGPROBE-confirmed) | PM four-step repro: hot daemon → uninstall → empty shell → hover = LS_NOT_INSTALLED, daemon alive, reinstall recovers; 67 suites green, clippy clean |
| (pending) | **45-language campaign Phase 0 + W1/W2** (user-directed full-catalog expansion, local machine zero-install — real-machine acceptance moved to CI): ① infra — servers.toml per-platform `bin_path_per_platform` (miss falls back to single value; 3 consumption points wired); CI smoke matrix: `smoke_one.sh` (baseline assertion = initialize + didOpen + documentSymbol non-empty, capability-aware fallback to hover/diagnostics when provider absent — ansible NOT_PLANNED #601; BUDGET watchdog; per-door stop-all hygiene) + `smoke_langs.toml` 57 doors (48 real + SKIP with evidence: LICENSE/PLATFORM/HOST/BUDGET taxonomy, budget 7) + `smoke_shard.py` budget-weighted LPT sharding + `ls-smoke.yml` (build artifact split, 6 shards, self-installed pinned LS only — SERENA_SKIP_LS_E2E policy untouched); 73-catalog reconciliation closed (64 covered + 1 angular SKIP [tri-server orchestration, never-verified] + 8 remaining); ② W1+W2 wiring (all stock entries — zero new toml rows): toml/terraform/**terraform exec bug fix** (`serve` subcommand, bare start = handshake timeout)/cue (`cue lsp` — cuelsp nonexistent, releases 404)/nix (HOST skip, nixd releases assets=[], flip when upstream ships)/ansible/rego/nextflow (JAR download confirmed, npm pick was wrong)/svelte (hybrid companion TS)/deno (**T2**: bare `deno lsp` = dead server without init options; `deno lsp` subcommand entry)/sass; PM real-world: workspace green ×2, clippy clean | CI truth pending first run; 57-door list parses; shards balanced |
| (pending) | TS probe prefers `src/` subtree (mirror upstream a4dff9e0): tsconfig-adjacent representative file selection for tsserver warm-up now picks the first `.ts`/`.tsx` under `src/` before same-dir files, avoiding excluded root-level tool configs (`vitest.config.ts` etc.); mirror comment dual-anchored `@43ae021`+`@a4dff9e0`; + `PROBE_ROOT_TEST_LOCK` serializing 5 existing probe tests (parallel static-slot race) | `cargo test -p ls-adapters typescript` 9/9 (3 new cases); clippy `-D warnings` clean; upstream sync round 2026-09-28 (`local/upstream-sync-2026-09-28.md`, 28 commits reviewed, anchor → 7a296833) |
| (pending) | bd closeout (7m8 + g6k): `wait-ready --stage semantic` probe now hovers symbol-name offsets (selectionRange first → in-line UTF-16 name lookup → range.start fallback, up to 3 symbols) fixing permanent false-pending on csharp-ls (file-symbol at 0,0, no selectionRange) and astro (template symbols legally null); overview probe errors no longer silently swallowed; astro `.astro`-originated references now merge main + companion TS results with (uri,line,col) dedupe (companion miss/failure degrades to primary) | real-machine: astro `ready in 0s`, csharp cold `ready in 5s` / warm `0s` exit=0 (was permanent timeout); astro refs=3 incl. companion-side `fmt.ts:1:17` |
| (pending) | upstream sync round 2026-09-28 wave 1+2 — **TS $/progress drain** (mirror cf54869a): lsp-core `IndexProgressTracker` (watch-based, no lost-wakeup window) + `window/workDoneProgress/create` handler (Weak, shutdown-cleaned) + trait `wait_for_cross_file_index` default-noop, TS override (first-query start-grace 5s / drain timeout 30s, later queries drain in-flight tokens), wired in `fetch_references` before request; **C# `--solution` seam** (behavior-equivalent of 8833e5e8 for csharp-ls): `find_unique_sln` (depth 4, ignore-dirs, vendored excluded) → `--solution` when exactly 1 visible .sln, else autodiscovery unchanged; spike proved csharp-ls folds new .cs via didOpen `tryAddDocument` (714c260e N/A, Roslyn-specific); **Astro language** (mirror 15502cee/7a296833): dual-server `astro-ls` + companion typescript-language-server with @astrojs/ts-plugin, 4-package npm install, per-file languageId override table in lsp-core docsync, ts/js→companion refs routing in supervisor, full registry (LanguageId/file_detect/servers.toml/doctor), README 11→12 | TS: progress_index_e2e 2 passed + real-machine e2e exit=0; C#: product e2e vendored symbols gone with --solution (B1/B2/B3 bare-LSP probes), 4 new tests; Astro: real-machine overview=5 symbols / hover cross-file type / refs=3 incl. .astro; workspace green, clippy `-D warnings` clean |
| (pending v0.2.0) | 3-platform release support: `ProcessTreeGuard` unifies process-tree governance (Windows Job Object unchanged; Unix process_group+killpg, linux PR_SET_PDEATHSIG parent-death backstop, macos explicit-kill tradeoff documented) + win32job gated to cfg(windows) deps + release.yml matrix → windows/linux(x86_64-gnu)/macos(aarch64) + unix spawn tests; deps.rs unix compile fixes | dual-target zigbuild link-level green (x86_64-linux-gnu, aarch64-apple-darwin); Windows 66 suites / 677 tests zero regression; live CI verification pending |

| commit | summary | key metric |
|---|---|---|
| (v0.1.1) | IDE undo/redo (txn-based snapshot stack): every write-tool call snapshots pre-write state via `recorded_write` (统一收口 atomic_write) into `{cache}/undo/{sha16(project_root)}/txn-{N}` (manifest + ≤100KB inline / side-file snapshots, created=true = new file); `undo`/`undo --list`/`redo` CLI + daemon tools; rename-symbol multi-file changes aggregate into ONE txn; undo conflict gate reuses WRITE_CONFLICT wire code (per-file sha256 == after_sha256, all-or-nothing); redo re-applies stored after-content; new write clears redo chain; prune (append + --list, no timers): >20 txns / >200MB / >30d evicted oldest-first; deterministic project_hash = sha256(canonicalize(root))[..16] (NOT DefaultHasher — random per-process seed) | e2e 18/18 PASS (`local/undo_e2e.py`): md5 round-trips, 2-file rename single-undo, created-file delete/restore, conflict names file, cross-restart stack survival, 31d-old txn pruned; 18 unit tests |
| (v0.1.1) | undo/redo LS-state sync (P2-b) + `undo::commit` under write_gate (P3): restore writes now push LS updates — modified/rebuilt files in the docsync buffers table get full `didChange` (ensure_open), deleted created files get `didClose` (new `Session::force_close`), never-opened files skipped (LS reads disk on demand); sync failure only warns, never fails undo; touched files carried out of `undo_one`/`redo_one` via uid-keyed `TOUCHED` registry (PENDING-style side channel — wire output & existing tests unchanged); `undo::commit` moved inside `write_gate` at the execute_tool collection point (serialize vs concurrent undo/redo prune; commit_at itself non-reentrant) | real-machine rust fixture: undo → immediate hover returns restored-content type (f64 stale → i32 restored), recreate-after-undo hover = new content (char, no i32 residual); mock_ls wire test proves didClose emitted; undo unit suite 19/19, workspace green, clippy -D warnings clean |
| (v0.1.1) | jdtls auto-install wired into `launch_info` (P-1 fix, 「机制存在≠接线生效」 third instance): PATH-miss + no pre-install dir → `spawn_blocking(ensure_jdtls_installed)` → DownloadInstaller (cache short-circuit = idempotent) → `java -jar equinox` launch; spec fixed to real snapshot layout (P-2: tar top level IS the payload — `strip_components` 1→0, short-circuit probe pinned launcher jar → `bin/jdtls`); sha gate jdtls-exception (rolling `latest` symlink, no companion hash file on eclipse.org — trust anchor = HTTPS + allowed_hosts, allow_unsigned_sha in-code with rationale); docs JRE 21+ → 25+ (P-3: current snapshot equinox requires JavaSE 25, JDK21 dies at OSGi resolve); doctor jdtls check now probes install cache and reports the true auto-download behavior (was: nonexistent `serena-cli install jdtls`); cache root single source of truth sunk to `ls_runtime::install::default_cache_root` (ls-registry delegates; adapters can't depend on ls-registry — cycle) | pre-install moved away + PATH w/o jdtls: cold `overview App.java` auto-downloads 51MB to `%LOCALAPPDATA%\serena\ls\jdtls\latest\` and returns 4 symbols in the 90s window; 2nd call serves from cache (no network); URL-unreachable → LS_NOT_INSTALLED (not panic/hang, 30s connect / 600s total timeout) |
| (v0.1.1) | pyright diagnostics pipeline fix: `diag_uri_key` normalizes push-uri cache keys with percent-decode + lowercase (pyright pushes `file:///c%3A/...`, path_to_uri generates `file:///C:/...` — lowercase-only normalization never matched, python diagnostics were constant `items:[]` `pending:true`; RA/clangd push forms are decode no-ops, behavior unchanged) | broken.py: `items:[] pending:true` (even 150s after push) → 2 exact errors `pending:false`; go/c regression clean; regression test `diag_uri_key_normalizes_percent_encoded_and_cased_uris` |
| (v0.1.1) | +4 languages (user-directed batch): bash (bash-language-server@5.6.0 npm), json (vscode-json-languageserver@1.3.4 npm), powershell (PowerShellEditorServices@4.4.0 download; bundled_modules_path = install_dir — upstream py bug not mirrored, PSScriptAnalyzer diagnostics verified live), vue (@vue/language-server@3.1.5 hybrid — companion TS LS with @vue/typescript-plugin, semantic routing live; diagnostics via tsserver bridge pending, filed) + LanguageId Bash/Json/PowerShell/Vue registry + file_detect extensions + doctor npm probe root-fix (which_no_unc bare-name shim shadowed npm.cmd) + doctor LS entries | 11/11 real-server smoke (per-adapter reports `local/report-*-adapter.md`); workspace green, clippy clean |
| (v0.1.1) | 9.5 campaign: 7/7 languages real-server smoke (c# via csharp-ls 0.15.0, java via jdtls snapshot — `local/report-ls-smoke-7of7.md`) + jdtls auto-download wired (ensure_jdtls_installed was dead_code with zero call sites — launch_info path4 now spawns it; strip_components 1→0 per real snapshot layout; JRE 25+ documented; doctor hint truthful) + timing-flaky campaign (write_gate oneshot signal sync replaces sleep guess, 2 wall-clock thresholds raised with headroom notes; 6 full runs 626/0 — `local/report-flaky-campaign.md`) + P0-B reverified at HEAD (p0b_fix_verify ×2 PASS, triple cold overview 634/403/302ms no channel-closed) | README claims 7/7 with zero caveats; workspace green ×multiple runs, clippy clean |
| (v0.1.1) | bilingual docs (user-directed): new `README.zh-CN.md` full translation with language-switch links (EN remains source of truth) + language coverage 2/7 → 5/7: c/cpp (clangd, single-file granularity works without compile_commands), python (pyright), go (gopls v0.23.0) real-machine smokes all pass overview/def+hover/diagnostics (`local/report-ls-smoke-5of7.md`); acceptance issue 7 (nonexistent file → INTERNAL) confirmed already fixed in ef22eb (BAD_ARGS rc=2); write-after-index staleness confirmed fixed (explicit stale warning + self-heal) | 3/3 smokes PASS; new lead filed: python diagnostics pipeline returns empty `pending:true` for pyright — follow-up |
| (v0.1.1) | docs accuracy batch (user-perspective audit): README position-baseline corrected 0-based → 1-based (bd serena-rust-7xv contract), command count 24 → 52 + long-tail list added, stale "Not implemented" claim removed (long-tail tools landed & verified 2026-09-23); global `--json` help fixed (default is compact JSON, flag disables compact — was claiming "default human-readable"); `diagnostics` empty help text filled | README no longer contradicts CLI help; zero behavior change |
| (v0.1.1) | dogfood UX batch (bd serena-rust-bxd): `wait-ready --stage symbol\|semantic` + `SERENA_WAIT_READY_TIMEOUT_SECS` + staged stderr progress; CLI surfaces `[warn]`/`[hint]` on cold-start empty results; `find-referencing-code-snippets --symbol NAME` one-step resolution (documentSymbol cache → workspace/symbol fallback, multi-hit warning, cold-window warming hint + 1 retry); LS warmup window (10s or first semantic success) stamps find-symbol with `index warming: results may be partial`, false-empty results skipped from Phase 3.1 cache | large-repo symbol-stage ready 0.0s (was 120s timeout); one-step refs == two-step identical; cold-window BAD_ARGS no longer misleading |
| (v0.1.1) | ContentModified(-32801) client-layer retry whitelist wired in production (bd serena-rust-s3u): `Session::start` registers positional methods via shared `init_params::RETRY_ON_CONTENT_MODIFIED` (single source of truth with `stale_request_support` declaration) | concurrent hover error rate 1.0% (12/1200, stable across 2 rounds) → 0 |
| (v0.1.1) | percent-encoded path_to_uri (bd serena-rust-cbd): non-ASCII/space/`#`/`?`/`%` UTF-8 percent-encoded in `docsync::path_to_uri_str` (`percent-encoding` crate promoted to direct dep) | emoji filename `😀.rs` overview: INTERNAL → symbols returned with valid `%F0%9F%98%80.rs` URI |
| (v0.1.1) | daemon orphan race fix (bd y2y): lock ownership-checked removal (`remove_owned` pid+boot_ms), stale-grace probing (3×300ms) before takeover, bind-before-lock arbitration, CLI 403 token self-heal (`refresh_token_if_stale`) | stop-all × lazy-spawn cross: 403 = 0 (was 400/400 calls), orphan daemons = 0, lock↔listener consistency held across daemon generations |
| (v0.1.1) | `wait-ready` subcommand (bd serena-rust-55m): blocks until type analysis truly usable (overview first-symbol → hover non-empty we0-verdict), 500ms→2s backoff, stderr progress; timeout exit 4 | AI/e2e scripts drop ~20 lines of hand-rolled polling each |
| (v0.1.1) | DAEMON_DRAINING client-side self-heal (bd serena-rust-g0m): forward classifies 503 + wire code, ≤5s window × 300ms retries full chain incl. lazy-spawn re-probe; beyond window original rc=3 error preserved | stop-all → immediate tool call succeeds instead of hard rc=3 |
| (v0.1.1) | configurable idle policy (bd serena-rust-j8b): `SERENA_IDLE_TIMEOUT_SECS` (global self-kill, default 900) + `SERENA_LS_IDLE_EVICTION_SECS` (LS eviction, default 600), `0` = never; invalid values warn+default; default behavior unchanged | AI batch jobs (90min) stop paying 30-90s cold restarts per idle eviction |
| (v0.1.1) | response token estimate (bd serena-rust-7rh): tools_post success envelope gains `"~tokens": N` (serialized bytes / 4, no tokenizer dep); `SERENA_NO_TOKEN_ESTIMATE=1` opts out; error responses / /batch unchanged | AI agents get a cost feedback signal on every tool response |

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
