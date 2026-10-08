English | [简体中文](README.zh-CN.md)

# serena-rust

> A position-free, symbol-level code assistant for AI agents — Rust reimplementation of [oraios/serena](https://github.com/oraios/serena), exposed as a CLI + skill (no MCP server required).

`serena-rust` gives an LLM agent the ability to navigate, search, and edit a codebase by **symbol name** (functions, types, fields) instead of by line number. It speaks the Language Server Protocol natively, so the same tool works across Rust, TypeScript, Python, Go, C/C++, C#, and Java without per-language adapters.

This is a focused reimplementation of [Serena](https://github.com/oraios/serena) anchored at upstream commit `7a296833`, kept current with the subset of features a CLI-based agent actually consumes (no MCP, no Python, no LSP server multiplexing beyond the standard one-session-per-root model).

## Why CLI + skill, not MCP

The upstream Serena project ships as an MCP server: the agent connects via the Model Context Protocol and the tools arrive as MCP method calls. We chose a different delivery:

- **CLI-first** — every capability is a subcommand of `serena-cli`. The agent drives it through `bash`, `serena-cli shell` (JSONL long-lived session), or pipes JSON.
- **Skill-first** — a single skill file (`skills/serena-cli/SKILL.md`) gives the agent the golden-path workflow and the token-discipline rules. No protocol negotiation, no handshake, no `mcp.json`.
- **Single daemon** — one process per workspace, lazily spawned on first command. Idle timeout = 15 min. Stops cleanly on `stop-all` (no zombies).

Trade-off: you lose the "MCP auto-discovery" story. You gain `bash`-debuggability, JSONL streaming, deterministic exit codes, and a 100% reproducible wire format (the 9-error-code contract in `ARCHITECTURE.md`).

## What it covers

### CLI commands (59)

Read / navigate (7): `overview` · `symbol-tree` · `read-file` · `list-dir` · `find-file` · `search` · `hover`

Symbols (8): `def` · `refs` · `find-symbol` · `symbol-body` · `find-implementations` · `find-referencing-symbols` · `find-referencing-code-snippets` · `containing-symbol`

Diagnostics (3): `diagnostics` (with `--wait-gen N`) · pull diagnostics fallback · `signature-help`

Editing (11): `rename-symbol` · `safe-delete-symbol` · `replace-body` · `replace-text-in-symbol` · `insert-text-before-symbol` · `insert-text-after-symbol` · `delete-text-in-symbol` · `insert-at-line` · `replace-lines` · `delete-lines` · `create-text-file`

Undo / redo (2): `undo` (`--steps N`, `--list`) · `redo` — transactional snapshot stack: every successful write records the prior state; a multi-file edit (e.g. cross-file rename) is one transaction and rolls back as a whole. Files created by a transaction are deleted on undo. Conflict gate: if a file changed on disk after the transaction, undo refuses instead of overwriting. Stack lives in the user cache dir (survives restarts and upgrades), capped at 20 txns / 200 MB / 30 days.

Completion (1): `completion` (with `--limit` and per-file-suffix trigger inference)

Admin (8): `status` · `stop-all` · `install <lang>` · `uninstall <lang>` · `ls-use <lang-or-id> <path>` · `ls-list` · `ls-remove <id>` · `shell` (JSONL stdin/stdout session)

Long-tail (19): `defining-symbol` · `edit-context` · `repo-map` · `warm` · `wait-ready` · `doctor` · `lint-shell` · `workspace-diagnostic` · `format` · `format-range` · `inlay-hint` · `document-highlight` · `folding-range` · `semantic-tokens` · `code-lens` · `document-link` · `call-hierarchy` · `type-hierarchy` · `moniker`

**Position baseline convention**: commands taking `line`/`col` (position-addressed: `def`, `refs`, `hover`, `find-implementations`, `rename-symbol`, `find-referencing-*`, `containing-symbol`, `defining-symbol`, `signature-help`, `code-action`, `document-highlight`, `completion`, `format-range`, `inlay-hint`, `call-hierarchy prepare`, `type-hierarchy prepare`, `moniker`) are **1-based** on the CLI surface — converted to LSP's 0-based `Position` internally (`normalize_positions`, bd serena-rust-7xv); passing `0` is a usage error. Line-range and line-editing commands (`read-file`, `insert-at-line`, `replace-lines`, `delete-lines`, `delete-text-in-symbol`) are **1-based inclusive**. Every command also states this in its `--help`.

vs. upstream oraios/serena: 19/19 high-ROI wrappers covered (every tool an agent realistically uses), plus the long tail (`documentHighlight`, `codeLens`, `documentLink`, `foldingRange`, `call/type hierarchy`, `moniker`, `semanticTokens`, `inlayHint`) — all landed and verified 2026-09-23; `document-link`/`moniker` return empty on LS without the capability (e.g. rust-analyzer stable).

### Language servers (53 of 73 in upstream catalog)

| Lang | Server | Status | Notes |
|---|---|---|---|
| Rust | tokio | ready | rustup-aware lookup chain (`rustup which` → cargo bin → PATH), workspace mode |
| C / C++ | clangd | ready | needs `compile_commands.json`; passes `--compile-commands-dir` |
| TypeScript / JS | typescript-language-server | ready | tsconfig walk, ATA off, npm shim fallback |
| Python | pyright | ready (adapter shell) | venv interpreter detection stubbed in `crates/ls-adapters/src/python.rs` |
| Go | gopls | ready (adapter shell) | `go.work` / multi-module dir detection stubbed |
| C# | csharp-ls | ready (adapter shell) | decision log: `local/csharp-ls-decision.md` (upstream migrated to roslyn LS) |
| Java | jdtls | ready (auto-download) | ~100MB download, JVM arg templates from `crates/ls-runtime/src/install.rs` |
| Bash | bash-language-server | ready (npm) | tree-sitter syntax diagnostics; hover needs Unix `man` pages (empty on Windows); ShellCheck integration not bundled |
| JSON | vscode-json-languageserver | ready (npm) | schema-driven hover/diagnostics |
| PowerShell | PowerShellEditorServices | ready (download) | requires `pwsh` 7+; bundled PSScriptAnalyzer diagnostics |
| Vue | @vue/language-server | ready (npm, hybrid) | companion typescript-language-server with `@vue/typescript-plugin`; semantic hover/routing live, diagnostics via tsserver bridge pending |
| Astro | @astrojs/language-server | ready (npm, hybrid) | companion typescript-language-server with `@astrojs/ts-plugin` (upstream `7a296833`); ts/js refs route to companion, `.astro` semantic hover live |
| Docker | docker-langserver | ready (npm) | `Dockerfile*` (incl. `Dockerfile.dev` variants) + `*.dockerfile`; didOpen languageId sent as `dockerfile` |
| SQL | sqls | ready (download) | `*.sql`; session/syntax layer live (formatting works), semantic hover/def need a `sqls` config (DB connection); no documentSymbol/references capability |
| PostgreSQL | postgres-language-server (pgls) | ready (download) | `--lang pgsql` (`.sql` defaults to the sql entry); syntax diagnostics live without a DB (libpg_query local parser); documentSymbol/schema hover need a DB connection — LS capability boundary |
| MySQL | sqls | ready (download) | `--lang mysql`; same sqls binary as the sql entry (separate cache dir); hover/diagnostics need a DB connection (LS warns "no database connection") |
| Kotlin | Kotlin LSP (JetBrains managed intellij-server) | ready (download) | `*.kt`/`*.kts`; same-file hover/def + documentSymbol live (managed LSP pinned to upstream `263.4702.0`, sha-verified, bundled JBR — no system JDK); cross-file def/refs need the LS project import (a `build.gradle.kts`/`pom.xml` marker), which auto-downloads the Gradle distribution — LS capability boundary |
| Dart | Dart SDK analysis server (`dart language-server`) | ready (download) | `*.dart`; full SDK download (206 MiB zip, sha-verified; pinned to upstream `3.7.1`); hover/def/refs/documentSymbol live on a bare folder (pubspec.yaml picked up automatically) |
| HTML | vscode-html-language-server | ready (npm) | `*.html`/`*.htm`; in-file element/id symbols + mdn-driven hover/completion (`vscode-langservers-extracted`); cross-file refs/def not meaningful for HTML (upstream) |
| CSS | vscode-css-language-server | ready (npm) | `*.css`; mdn-driven hover/completion for properties/selectors; same npm package as the html entry (separate cache dir) |
| YAML | yaml-language-server | ready (npm) | `*.yaml`/`*.yml`; schema-driven hover/completion/diagnostics (schemastore); syntax diagnostics live without a schema |
| Markdown | marksman | ready (download) | `*.md`/`*.markdown`; heading documentSymbols/workspace symbols; link def/refs/hover live when the project root is detectable (git repo or `.marksman.toml`) — marksman-side project detection, marker-less scratch dirs degrade to per-file assist |
| TOML | taplo | ready (download) | `*.toml`; single-file gzip binary (pinned `0.10.0`, sha-verified against upstream's embedded checksums); table/key documentSymbols; schema-driven hover/diagnostics when a schema association exists (taplo feature) |
| Terraform | terraform-ls | ready (download) | `*.tf`/`*.tfvars`; block/resource documentSymbols (pinned `0.36.5`, HashiCorp releases, sha-verified; launched as `terraform-ls serve`); upstream requires a `terraform` CLI on PATH for module features — documentSymbol is parser-only and works without |
| Cue | `cue lsp` (built into the cue CLI) | ready (download) | `*.cue`; the cue CLI embeds its LSP behind a hidden `lsp` subcommand (v0.16.1 verified); field/package documentSymbols |
| Nix | nixd | source build | `*.nix`; installed via `git clone` + `nix build` — requires the Nix toolchain (upstream ships no prebuilt release assets, same constraint as upstream's adapter); attribute documentSymbols once built |
| Ansible | ansible-language-server | wired (npm); CI smoke pending | `--lang ansible` (`.yaml`/`.yml` stay with the YAML gate); hover/completion/diagnostics live; **no documentSymbol** — upstream declined (vscode-ansible#601 NOT_PLANNED), the smoke gate degrades to diagnostics via `fallback_assert` |
| Rego | regal | wired (download); CI smoke pending | `*.rego`; single-file binary (pinned `0.42.0`, sha-verified against GitHub release `assets[].digest`); `regal language-server` launch; documentSymbol/hover/def/diagnostics per Regal docs |
| Nextflow | Nextflow language server | wired (download); CI smoke pending | `*.nf`; fat JAR (pinned `26.04.3`, sha-verified; requires JDK ≥17 on PATH); outline/def/refs/hover/diagnostics; no npm package exists (registry 404) — upstream's JAR distribution is the only install form |
| Svelte | svelte-language-server (svelteserver) | wired (npm); CI smoke pending | `*.svelte`; hybrid: main svelteserver + companion typescript-language-server with `typescript-svelte-plugin` (upstream `7a296833`); ts/js semantics route to the companion; `.svelte` files pre-opened on the companion so the plugin sees the full TS graph |
| Deno | `deno lsp` (Deno CLI built-in) | wired (download); CI smoke pending | `--lang deno` explicit routing only — TS-family extensions stay with the typescript gate (upstream marks deno experimental for exactly this overlap); zip from GitHub releases (pinned `2.9.7`, sha-verified against `assets[].digest`); entry is the `deno lsp` subcommand, not a `--stdio` flag; init options `{enable, lint}` injected (bare deno lsp starts disabled) |
| Sass | some-sass-language-server | wired (npm); CI smoke pending | `--lang sass` (servers.toml entry id `scss` = cache-dir key); `*.sass`/`*.scss` (`.css` stays with the css gate); didOpen languageId `scss` with per-file `.sass` override; somesass init options + `workspace/configuration` slice mirrored from upstream `7a296833` |
| PHP | intelephense (smoke gate) / phpactor | wired (npm); CI smoke pending | `*.php`; language route `php` stays with the phpactor download entry (pre-existing collision avoidance), the smoke gate installs and routes by entry id `--lang intelephense` (phpantom precedent); didOpen languageId mapped `intelephense` → `php` (official); intelephense needs no PHP runtime (npm only), phpactor PHAR needs PHP 8.1+ |
| Lua | lua-language-server (LuaLS) | wired (download); CI smoke pending | `*.lua`; GitHub release tarballs pinned `3.15.0` (sha-verified); first CI run already green (run 36528495148) — this batch adds the Rust-side routing closure (LanguageId/EXT_TABLE) |
| Scala | metals | wired (path_only); CI smoke pending | `*.scala`; upstream launches metals from PATH or via coursier bootstrap (pinned `metals_2.13:1.6.4`, upstream `DEFAULT_METALS_VERSION`); servers.toml entry is path_only (GitHub releases carry no prebuilt assets, v1.6.9 checked 2026-09-29); CI gate bootstraps via coursier to `/usr/local/bin` (JDK 11+, runner has 17); no build files in the fixture — our T0 client never answers import prompts, so metals runs its standalone presentation compiler; `fallback_assert` hover probe registered |
| Swift | sourcekit-lsp | wired (path_only); CI smoke = PLATFORM SKIP | `*.swift`; ships with the Xcode/Swift toolchain (path_only entry, no installable asset) — ubuntu runners have no Swift toolchain, so the gate logs PLATFORM SKIP (8th SKIP ledger entry, PM-approved quota 7→8, hard cap 8); converts to a real gate if runners ship a Swift toolchain or sourcekit-ls publishes standalone binaries; Rust-side wiring complete (LanguageId/EXT_TABLE/doctor) |
| Fortran | fortls | wired (uvx); CI smoke passing | `*.f90`/`*.f95`/`*.f03`/`*.f08`/`*.f`/`*.for`/`*.fpp`; pip `fortls` 3.2.2 via uvx — the matrix gate already passed in an earlier run; this batch closes the Rust-side routing (LanguageId/EXT_TABLE, per-door template) |
| Pascal | pasls | wired (download); CI smoke = BUDGET SKIP | `*.pas`/`*.pp`; prebuilt v0.2.0 (win/macOS assets in the entry); full features need the FPC toolchain (PP/FPCDIR) — apt fpc ≈400MB exceeds the per-door budget (pre-existing PM adjudication); unix wiring + FPC prep owned by this batch's follow-up |
| Haskell | haskell-language-server-wrapper | wired (path_only); CI smoke pending | `*.hs`/`*.lhs`; entry exec fix `--lsp` (bare wrapper prints usage and exits — terraform `serve` class); HLS needs a matching GHC — bindist 2.9.0.1 chosen because 2.15 dropped GHC 9.4 (ubuntu-24.04 apt ceiling); bare files use the default cradle against apt ghc |
| Groovy | (no managed server — upstream demands a user-supplied JAR) | not in matrix (angular precedent — no installable LS exists; wiring kept for instant adoption) | `*.groovy`/`*.gvy`; upstream `groovy_language_server.py` hard-requires `ls_jar_path` — npm has no LS package, GroovyLanguageServer GitHub releases are `[]`, apt has no LS: four install routes exhausted; LanguageId/extensions wired, no servers.toml entry (angular/java precedent) |
| OCaml | ocamllsp | wired (path_only); CI smoke pending | `*.ml`/`*.mli`; opam `ocaml-lsp-server` (opam switch on the system compiler avoids building the toolchain); upstream resolves ocamllsp via `opam exec` then launches it bare = path_only semantics; OCaml 5.1.0 is incompatible (documented upstream) |
| Erlang | erlang_ls | wired (path_only); CI smoke pending | `*.erl`/`*.hrl`; entry exec fix `--transport stdio` (default transport is TCP — stdio clients would hang); prebuilt escript per OTP release (ubuntu-24.04 apt = OTP 25.3 → the `-25` tarball); needs the erlang runtime on PATH |
| Perl | Perl::LanguageServer (via perl) | wired (path_only); CI smoke pending | `*.pl`/`*.pm`/`*.t`; launch argv mirrored verbatim (`perl -MPerl::LanguageServer -e Perl::LanguageServer::run`); installed via cpanm; upstream answers `workspace/configuration` — our T0 relies on lsp-core's default null-success reply, so file filters fall back to LS defaults |
| R | languageserver (via R) | wired (path_only); CI smoke pending | `*.r`/`*.rmd`/`*.rnw`; launch argv mirrored verbatim (`R --vanilla --quiet --slave -e ... languageserver::run()`); CRAN install compiles from source (runners ship gcc) |
| Crystal | crystalline | wired (path_only); CI smoke pending | `*.cr`; musl static single binary (no crystal toolchain needed); documentSymbol "works reliably" per upstream; entry stays path_only (legacy anchor canary) — the CI gate curls the pinned v0.20.0 release URL (sha-verified) |
| Zig | zls | wired (download); CI smoke pending | `*.zig`/`*.zon`; entry upgraded path_only → download (six-platform pins, sha-verified against `assets[].digest`, 0.16.0); zls pairs strictly with the same-minor zig — the gate installs the ziglang.org 0.16.0 toolchain tarball (sha verified against the official index) and symlinks it onto PATH |
| Gleam | `gleam lsp` (Gleam CLI built-in) | wired (path_only); CI smoke pending | `*.gleam`; the LS ships inside the self-contained gleam compiler binary — the gate installs the pinned v1.18.1 musl release (sha-verified against `assets[].digest`); entry exec is the `gleam lsp` subcommand (deno precedent); upstream waits for the first `$/progress` dependency phase — our T0 has no such gate (tool-level timeout covers it), bare fixtures without `gle.toml` degrade to per-file analysis with a hover fallback registered |
| QML | qmlls (Qt 6 official) | wired (path_only); CI smoke pending | `*.qml`; ships with Qt 6 — apt `qt6-declarative-dev-tools` provides `/usr/bin/qmlls6` (Debian install list verified), the gate symlinks it to `qmlls` to match the entry's single binary name (upstream discovers `qmlls6` then `qmlls`); ubuntu-24.04 pins Qt 6.4.2, the initial qmlls LSP release (narrow capability set) — documentSymbol fallback to hover registered; newer Qt needs the interactive installer (not CI-scriptable) |
| Lean 4 | `lean --server` (Lean toolchain built-in) | wired (path_only); CI smoke pending | `*.lean`; language name `lean` routes to entry id `lean4` (zls/zig dual-name precedent); the gate installs the pinned v4.34.1 full toolchain tarball (580 MB tar.zst, sha-verified against `assets[].digest`) without elan; standalone fixtures cover basic symbols (def/theorem documentSymbol) — upstream's lake env LEAN_PATH injection for cross-file semantics is not mirrored |
| Julia | LanguageServer.jl (via julia) | wired (path_only); CI smoke pending | `*.jl`; launch argv mirrored verbatim (`julia --startup-file=no --history-file=no -e 'using LanguageServer; runserver()'`) — the trailing repo_root argument is dropped because our T0 spawns with cwd = project root and runserver's env fallback chain includes pwd; requires the julia runtime + `Pkg.add("LanguageServer")` (the gate pre-installs; budget 1200 s upper-bound pending CI measurement); `workspace/configuration` falls back to lsp-core's null reply (perl precedent), lint settings stay at LS defaults |
| Wolfram | WolframKernel LSPServer paclet | LICENSE SKIP candidate; PM adjudication pending | `*.wl`/`*.nb`; the LS ships only inside Mathematica 13.0+ / Wolfram Engine 12.1+ (licensed install, no scriptable CI route — upstream `wolfram_language_server.py` discovery relies entirely on a local Wolfram install); entry `[servers.wolfram]` kept as the PATH-probe surface for users who own an install (haskell_ls runtime-supplied semantics); converts to a real gate if a license-free scriptable route appears |
| GDScript (Godot) | (no standalone server — upstream connects to a running editor over TCP) | not in matrix (angular precedent — LS = TCP client to a running editor, no stdio T0) | `*.gd`; upstream `godot_language_server.py` is a TCP client to an already-running Godot editor on :6008 and never launches a process — our transport is stdio-only; LanguageId/extensions wired, no servers.toml entry (angular/groovy precedent); converts to a real gate if lsp-core grows a TCP transport + editor orchestration |
| mSL (mIRC) | (upstream LS is a pygls script inside the serena repo) | not in matrix (angular precedent — LS = TCP client to a running editor, no stdio T0) | `*.mrc`; upstream launches `[python, msl_lsp_server.py]` — a script shipped inside serena, not independently distributed and not bundled with our Rust binary; the W5 contract's "metal" was a misreading of this door (the 73-door ledger has no metal entry — mSL = mIRC Scripting Language); LanguageId/extensions wired, no servers.toml entry; converts to a real gate if msl_lsp is published standalone (pip package / separate repo) |

All 20 end-to-end smoke verified on real language servers (rust, typescript, c/cpp, python, go — 2026-09-25; c#, java — 2026-09-25; bash, json, powershell, vue — 2026-09-25; astro — 2026-09-28; docker, sql — 2026-09-28; postgresql, mysql — 2026-09-28; yaml, markdown — 2026-09-28; kotlin, dart, html, css — 2026-09-28; `local/report-ls-smoke-5of7.md` / `local/report-ls-smoke-7of7.md` / per-adapter reports `local/report-*-adapter.md`). Per-language CI smoke lives in `scripts/smoke_one.sh` (matrix manifest `scripts/smoke_langs.toml`, weekly workflow `.github/workflows/ls-smoke.yml`).

## Install

```bash
# From source (single binary, no runtime deps beyond `rustup` itself)
git clone https://github.com/Be90nia/serena-cli.git
cd serena-cli
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

### Using an LS you already have (custom path)

No need to re-download what your machine already has. `ls-use` points a `servers.toml` entry at your own binary by writing `%APPDATA%/serena/external-servers.toml` (`~/.config/serena/` on Unix):

```bash
serena-cli ls-use python D:/tools/jedi-ls.exe    # known language/id: inherits languages/extensions/exec, swaps in your binary
serena-cli ls-use mydsl D:/tools/mydsl-ls.exe --lang mydsl --ext .mydsl   # brand-new language
serena-cli ls-use --list                         # registered entries + per-language effective source
serena-cli ls-use --remove mydsl                 # unregister (rest of the file, comments included, stays byte-identical)
serena-cli ls-list                               # full inventory: builtin × installed / external-override / not-installed + reclaimable bytes
serena-cli ls-remove marksman                    # uninstall serena-managed cache only (never touches PATH/ecosystem installs)
```

Registration takes effect after a daemon restart (`serena-cli stop-all` or the idle timeout). Entries are plain TOML — hand-editing is fine; `ls-use` only rewrites its own `[servers.<id>]` block.

### Vendor-specific LS configuration (MATLAB)

Some servers need your machine's own paths to work at all. MATLAB's language server, for example, must be told where MATLAB is installed (↖ upstream `matlab_language_server.py` replies `installPath` / `matlabConnectionTiming` to `workspace/configuration` and injects `MATLAB_INSTALL_PATH` into the launch env). `servers.toml` ships the template **commented out** under `[servers.matlab]` — point both paths at your install and uncomment:

```toml
[[servers.matlab.config_reply]]
section = "MATLAB"
value = { installPath = "C:/Program Files/MATLAB/R2024b", matlabConnectionTiming = "onStart" }

[servers.matlab.env]
MATLAB_INSTALL_PATH = "C:/Program Files/MATLAB/R2024b"
```

This uses the same override channels as `external-servers.toml` / `ls-use` (`config_reply` answers the server's `workspace/configuration`; `env` adds launch environment variables). Restart the daemon afterwards (`serena-cli stop-all`).

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

- `crates/lsp-core` — JSON-RPC framing, request/response client with id normalization, `ContentModified` retry, managed LSP process spawn + stdio/TCP transport (3-pump topology, mirror of upstream `ls_process.py`)
- `crates/ls-registry` — `LanguageServerId` ↔ file extension table
- `crates/ls-adapters` — per-LS launch args + readiness probes (rust-analyzer / clangd / pyright / gopls / typescript / csharp-ls / jdtls)
- `crates/supervisor` — `Supervisor` trait + `DaemonSupervisor` impl: per-key load gate, session cache, tool dispatch, write gate, symbol cache
- `crates/daemon` — HTTP front (axum), 9-error-code wire contract, singleton lock, idle reaper, graceful shutdown
- `crates/cli` — clap subcommands, `--json` mode, `shell` JSONL session, `install`, management commands
- `crates/ls-runtime` — `deps.rs` (download/SHA), `install.rs` (auto-install flow), `servers.toml` adapter

The single source of architectural truth is [`ARCHITECTURE.md`](ARCHITECTURE.md). The 9 error codes (`BAD_ARGS`, `LS_NOT_INSTALLED`, `LS_SPAWN_FAILED`, `LS_NOT_READY`, `LS_TERMINATED`, `LS_TIMEOUT`, `RPC_ERROR`, `WRITE_CONFLICT`, `INTERNAL`) are a wire contract — do not rename, do not split, do not merge.

## Limitations

| Limitation | Impact | When to revisit |
|---|---|---|
| SHA-256 download matrix for non-clangd LS is partial | `install` works for verified entries; unverified entries fall back to PATH probe | When CI needs hermetic installs — populate `local/ls-download-matrix.md` from upstream SolidLSP source |
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

- [oraios/serena](https://github.com/oraios/serena) — the original Python MCP server this reimplementation is modeled on. Anchored at upstream commit `7a296833`.
- [helix-editor/helix](https://github.com/helix-editor/helix) — the `find_lsp_workspace` algorithm in `crates/supervisor/src/root.rs` is a direct translation.
- The Cargo dependency tree (jsonrpc-core, tokio, axum, lsp-types, ...) — see `Cargo.lock`.

## License

[MIT](LICENSE)

## Detailed data

End-to-end PR description, commit-by-commit metrics, and Round 2 backlog: [`local/PR-description.md`](local/PR-description.md).