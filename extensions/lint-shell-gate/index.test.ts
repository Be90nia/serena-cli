/**
 * Tests for lint-shell-gate — pure-function suite for parseFindings and
 * renderReminder. Spawn/timeout/cache behavior is verified by manual
 * session tests in local/report-sm0.md (we don't have an OMP event-bus
 * fixture harness in this sandbox).
 *
 * Run: `bun test extensions/lint-shell-gate/index.test.ts`
 */

import { describe, expect, test } from "bun:test";

// ── Mirror of the production helpers (kept in sync manually; the
// production module's `default export` is an OMP extension factory
// requiring ExtensionAPI, which we cannot instantiate here).

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

function renderReminder(_command: string, gated: Finding[]): string {
    return (
        `\n\n[lint-shell] ${gated.length} error(s) found in shell command:\n` +
        gated.map((f) => `  - ${f.code}: ${f.message} (line ${f.line})`).join("\n") +
        `\nrun 'serena-cli lint-shell --cmd "<command>"' to see all`
    );
}

// ── Tests ──────────────────────────────────────────────────────────────

describe("parseFindings", () => {
    test("parses a single error finding", () => {
        const stdout = JSON.stringify({
            findings: [
                {
                    code: "UNKNOWN_TOOL",
                    severity: "error",
                    message: "unknown tool 'foo'",
                    line: 1,
                },
            ],
            summary: { errors: 1, warnings: 0, infos: 0 },
        });
        const f = parseFindings(stdout);
        expect(f).toHaveLength(1);
        expect(f[0].code).toBe("UNKNOWN_TOOL");
        expect(f[0].severity).toBe("error");
    });

    test("returns empty on non-JSON (fail-open at parse layer)", () => {
        expect(parseFindings("garbage")).toEqual([]);
        expect(parseFindings("")).toEqual([]);
        expect(parseFindings("not json at all")).toEqual([]);
    });

    test("returns empty when findings is missing", () => {
        expect(parseFindings('{"summary":{}}')).toEqual([]);
    });

    test("filters out findings with bad shape", () => {
        const stdout = JSON.stringify({
            findings: [
                { code: "X", severity: "bogus", message: "m", line: 1 },
                { code: 42, severity: "error", message: "m", line: 1 },
                { code: "X", severity: "error", message: "ok", line: 1 },
                null,
            ],
        });
        const f = parseFindings(stdout);
        expect(f).toHaveLength(1);
        expect(f[0].code).toBe("X");
    });

    test("preserves multi-finding error arrays", () => {
        const stdout = JSON.stringify({
            findings: [
                { code: "PY_UNPACK", severity: "error", message: "x, y = subprocess.run(...)", line: 3 },
                { code: "BASH_UNAVAILABLE", severity: "info", message: "bash is a WSL stub", line: 1 },
            ],
        });
        const f = parseFindings(stdout);
        expect(f).toHaveLength(2);
        expect(f[0].code).toBe("PY_UNPACK");
        expect(f[1].severity).toBe("info");
    });
});

describe("severity threshold filter (mirrors handler-side filter)", () => {
    test("error threshold keeps only error findings", () => {
        const findings: Finding[] = [
            { code: "E1", severity: "error", message: "m", line: 1 },
            { code: "W1", severity: "warning", message: "m", line: 2 },
            { code: "I1", severity: "info", message: "m", line: 3 },
        ];
        const cutoff = SEVERITY_RANK["error"];
        const gated = findings.filter((f) => SEVERITY_RANK[f.severity] >= cutoff);
        expect(gated).toHaveLength(1);
        expect(gated[0].code).toBe("E1");
    });

    test("warning threshold keeps error + warning, drops info", () => {
        const findings: Finding[] = [
            { code: "E1", severity: "error", message: "m", line: 1 },
            { code: "W1", severity: "warning", message: "m", line: 2 },
            { code: "I1", severity: "info", message: "m", line: 3 },
        ];
        const cutoff = SEVERITY_RANK["warning"];
        const gated = findings.filter((f) => SEVERITY_RANK[f.severity] >= cutoff);
        expect(gated).toHaveLength(2);
    });

    test("info threshold keeps everything", () => {
        const findings: Finding[] = [
            { code: "E1", severity: "error", message: "m", line: 1 },
            { code: "W1", severity: "warning", message: "m", line: 2 },
            { code: "I1", severity: "info", message: "m", line: 3 },
        ];
        const cutoff = SEVERITY_RANK["info"];
        const gated = findings.filter((f) => SEVERITY_RANK[f.severity] >= cutoff);
        expect(gated).toHaveLength(3);
    });
});

describe("renderReminder", () => {
    test("renders the expected shape", () => {
        const findings: Finding[] = [
            { code: "UNKNOWN_TOOL", severity: "error", message: "unknown tool 'foo'", line: 1 },
            { code: "PY_UNPACK", severity: "error", message: "subprocess.run unpacking", line: 3 },
        ];
        const out = renderReminder("ignored", findings);
        expect(out).toContain("[lint-shell] 2 error(s) found in shell command:");
        expect(out).toContain("- UNKNOWN_TOOL: unknown tool 'foo' (line 1)");
        expect(out).toContain("- PY_UNPACK: subprocess.run unpacking (line 3)");
        expect(out).toContain("run 'serena-cli lint-shell --cmd");
    });

    test("empty gated list still renders a zero-count header (defensive — handler skips this anyway)", () => {
        const out = renderReminder("x", []);
        expect(out).toContain("[lint-shell] 0 error(s) found");
    });
});