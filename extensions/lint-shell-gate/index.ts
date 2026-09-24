/**
 * lint-shell-gate — omp extension
 *
 * Bridges oh-my-pi's bash tool to the serena-rust `lint-shell` static
 * linter. After every successful `bash` tool_result we spawn
 *
 *   <cli> lint-shell --cmd <the command string> --json
 *
 * and, when the linter reports at least one `error`-level finding, append
 * a `[lint-shell] N error(s) found ...` reminder to the tool_result
 * content array. Because reminders are part of the tool_result they land
 * in the conversation history the model sees on its next turn, instead
 * of a transient TUI status line the model can't read.
 *
 * Everything is fail-open: a missing binary, a 5s timeout, a JSON parse
 * glitch — none of them block the tool result or surface in the UI.
 * `warning`/`info` findings stay silent so good commands don't get noisy.
 *
 * All tunables live in `~/.omp/hooks/configs.yml` under `lint_shell_gate:`.
 * Missing keys fall back to the defaults below.
 *
 * ── Upgrade notes ──
 * If omp renames the `tool_call` / `tool_result` events or the event
 * payload fields, only the lines marked `// UPGRADE:` need a second look.
 */

import { spawn } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { homedir } from "node:os";
import type { ExtensionAPI } from "@oh-my-pi/pi-coding-agent";

// ── Defaults ─────────────────────────────────────────────────────────────

const DEFAULTS = {
    /** Only tool_result events whose toolName is in this list are gated. */
    gated_tools: ["bash"],
    /** Default CLI binary name; resolve from PATH or an absolute path. */
    cli: "serena-cli",
    /** Hard timeout for a single `lint-shell` spawn. */
    cli_timeout_ms: 5_000,
    /** Severity threshold — only inject when at least one finding has this
     *  severity or higher. `error` keeps the agent quiet on warnings/infos. */
    severity_threshold: "error",
} as const;

const CONFIG_PATH = join(homedir(), ".omp", "hooks", "configs.yml");

// ── Internal state ──────────────────────────────────────────────────────

interface LintShellConfig {
    gated_tools: string[];
    cli: string;
    cli_timeout_ms: number;
    severity_threshold: "error" | "warning" | "info";
}

// Last-write-wins cache: tool_call populates event.input.command, then
// tool_result reads it. Single slot — concurrent bash calls would race
// anyway, and OMP runs tool calls sequentially per turn. Keying by callId
// would require carrying it across events, which the wire format does
// not guarantee (see cbm-bridge/index.ts for the same single-slot pattern).
let lastCommand: string | null = null;

// ── YAML loader (Bun-native, mirrors cbm-bridge) ────────────────────────

function loadConfig(): LintShellConfig {
    const out: LintShellConfig = {
        gated_tools: [...DEFAULTS.gated_tools],
        cli: DEFAULTS.cli,
        cli_timeout_ms: DEFAULTS.cli_timeout_ms,
        severity_threshold: DEFAULTS.severity_threshold,
    };
    let raw: string;
    try {
        raw = readFileSync(CONFIG_PATH, "utf8");
    } catch {
        return out;
    }

    let parsed: unknown;
    try {
        parsed = Bun.YAML.parse(raw);
    } catch {
        return out;
    }
    if (typeof parsed !== "object" || parsed === null) return out;

    const section = (parsed as Record<string, unknown>)["lint_shell_gate"];
    if (typeof section !== "object" || section === null) return out;
    const s = section as Record<string, unknown>;

    if (Array.isArray(s["gated_tools"]) && s["gated_tools"].every((v) => typeof v === "string")) {
        out.gated_tools = s["gated_tools"] as string[];
    }
    if (typeof s["cli"] === "string" && s["cli"].length > 0) {
        out.cli = s["cli"];
    }
    if (typeof s["cli_timeout_ms"] === "number" && s["cli_timeout_ms"] > 0) {
        out.cli_timeout_ms = s["cli_timeout_ms"];
    }
    if (s["severity_threshold"] === "error" || s["severity_threshold"] === "warning" || s["severity_threshold"] === "info") {
        out.severity_threshold = s["severity_threshold"];
    }
    return out;
}

// ── Spawn wrapper (fail-open) ────────────────────────────────────────────

interface SpawnResult {
    ok: boolean;
    stdout: string;
    stderr: string;
}

function runLintShell(cli: string, command: string, timeoutMs: number): Promise<SpawnResult> {
    const { promise, resolve } = Promise.withResolvers<SpawnResult>();

    const child = spawn(cli, ["lint-shell", "--cmd", command, "--json"], {
        windowsHide: true,
        stdio: ["ignore", "pipe", "pipe"],
    });

    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => {
        stdout += chunk.toString();
    });
    child.stderr.on("data", (chunk: Buffer) => {
        stderr += chunk.toString();
    });

    const killTimer = setTimeout(() => {
        child.kill();
        resolve({ ok: false, stdout: "", stderr: `timeout after ${timeoutMs}ms` });
    }, timeoutMs);

    child.on("error", (e) => {
        clearTimeout(killTimer);
        // ENOENT (binary missing), EACCES (no execute), EPIPE, etc. — all
        // surfaced as `spawn error` and never block the tool result.
        resolve({ ok: false, stdout: "", stderr: `spawn error: ${e.message}` });
    });

    child.on("exit", (code) => {
        clearTimeout(killTimer);
        // lint-shell is warn-only by default — exit 0 even with errors.
        // `code === null` means we killed it (timeout).
        resolve({ ok: code === 0, stdout, stderr });
    });

    return promise;
}

// ── Finding parser ──────────────────────────────────────────────────────

interface Finding {
    code: string;
    severity: "error" | "warning" | "info";
    message: string;
    line: number;
}

interface LintShellJson {
    findings?: Finding[];
    summary?: { errors?: number; warnings?: number; infos?: number };
}

const SEVERITY_RANK: Record<Finding["severity"], number> = {
    error: 3,
    warning: 2,
    info: 1,
};

function parseFindings(stdout: string): Finding[] {
    let v: LintShellJson;
    try {
        v = JSON.parse(stdout);
    } catch {
        return [];
    }
    if (!Array.isArray(v.findings)) return [];
    return v.findings.filter(
        (f): f is Finding =>
            !!f &&
            typeof f.code === "string" &&
            (f.severity === "error" || f.severity === "warning" || f.severity === "info") &&
            typeof f.message === "string" &&
            typeof f.line === "number",
    );
}

// ── Event payload for the tool_result hook ──────────────────────────────
// Only the fields this extension reads. omp types live in
// @oh-my-pi/pi-coding-agent; we keep the interface local so this file
// stays self-contained if the upstream type evolves.
//
// UPGRADE: these structural types reflect the wire payload of the
// `tool_call` / `tool_result` events. If omp renames or repacks the
// fields, edit these.

interface ToolResultEvent {
    toolName: string;
    input?: Record<string, unknown>;
    isError?: boolean;
    content: Array<{ type: string; text?: string }>;
}

interface ToolCallEvent {
    toolName: string;
    input?: Record<string, unknown>;
}

// Cast helpers. The runtime event payload matches these structural types;
// we keep them local so a future upstream type change does not require
// editing the handler bodies.
const asToolResult = (event: unknown): ToolResultEvent => event as ToolResultEvent;
const asToolCall = (event: unknown): ToolCallEvent => event as ToolCallEvent;

// ── Extension entry point ───────────────────────────────────────────────

export default function lintShellGate(pi: ExtensionAPI) {
    // Diagnostic: confirm the factory actually ran. Writes to stderr so
    // it shows up in the omp session log even if notify never fires.
    process.stderr.write(`[lint-shell-gate] loaded (pid=${process.pid})\n`);
    pi.setLabel("lint-shell gate");

    const cfg = loadConfig();

    pi.on("tool_call", async (event, _ctx) => {
        if (!cfg.gated_tools.includes(event.toolName)) return;
        const input = (event as unknown as { input?: Record<string, unknown> }).input ?? {};
        const candidate = (input.command ?? input.cmd ?? null) as unknown;
        // UPGRADE: `command` is the canonical field on bash events; `cmd`
        // is a forward-compat fallback for forks that rename it.
        if (typeof candidate !== "string" || candidate.length === 0) return;
        lastCommand = candidate;
    });

    pi.on("tool_result", async (event, _ctx) => {
        if (!cfg.gated_tools.includes(event.toolName)) return;

        // Don't attach a reminder to error responses — when bash itself fails
        // (non-zero exit), the failure is already in `content`. The lint
        // would be noise on top of an error.
        const er = asToolResult(event);
        if (er.isError) return;

        const cmd = lastCommand;
        // No cached command — happens when tool_result fires without a
        // preceding tool_call in this session (e.g. replay). Stay silent.
        if (cmd === null) return;
        // Cache: only flag input commands; one-shot reminders, don't burn
        // CLI cycles re-checking the same string.
        lastCommand = null;

        // CLI binary existence check is best-effort: PATH-relative names
        // pass silently here (we let `spawn` produce ENOENT if missing —
        // the same `ok: false` branch handles it).
        if (cfg.cli.includes("/") || cfg.cli.includes("\\")) {
            if (!existsSync(cfg.cli)) return;
        }

        const r = await runLintShell(cfg.cli, cmd, cfg.cli_timeout_ms);
        if (!r.ok) {
            // Fail-open: any spawn/timeout/parse error stays silent. Log
            // the raw output to stderr for debugging; users find it in
            // ~/.omp/logs/omp.*.log.
            process.stderr.write(
                `[lint-shell-gate] cli not ok (cli=${cfg.cli} timeout=${cfg.cli_timeout_ms}): ` +
                    `${r.stderr || "exit non-zero"} | stdout=${r.stdout.slice(0, 200)}\n`,
            );
            return;
        }

        const findings = parseFindings(r.stdout);
        const cutoff = SEVERITY_RANK[cfg.severity_threshold];
        const gated = findings.filter((f) => SEVERITY_RANK[f.severity] >= cutoff);
        if (gated.length === 0) return;

        const reminderText =
            `\n\n[lint-shell] ${gated.length} error(s) found in shell command:\n` +
            gated.map((f) => `  - ${f.code}: ${f.message} (line ${f.line})`).join("\n") +
            `\nrun 'serena-cli lint-shell --cmd "<command>"' to see all`;
        return {
            content: [...er.content, { type: "text", text: reminderText }],
        };
    });
}