import { describe, it, expect } from "vitest";
import type { HighlightRule } from "../stores/app.svelte.ts";
import {
    ansiColor,
    compileHighlightRules,
    highlightPlain,
    validateHighlightRule,
} from "./highlight.ts";

function rule(
    keyword: string,
    opts: Partial<HighlightRule> = {}
): HighlightRule {
    return {
        keyword,
        color: "#FF6B6B",
        enabled: true,
        is_regex: false,
        is_case_sensitive: false,
        ...opts,
    };
}

const RST = "\x1b[0m";

describe("ansiColor", () => {
    it("returns 24-bit ANSI sequence for hex color", () => {
        expect(ansiColor("#FF6B6B")).toBe("\x1b[38;2;255;107;107m");
    });

    it("returns empty string for invalid hex", () => {
        expect(ansiColor("red")).toBe("");
    });
});

describe("validateHighlightRule", () => {
    it("accepts valid regex", () => {
        expect(validateHighlightRule(rule("\\d+", { is_regex: true }))).toBeNull();
    });

    it("rejects invalid regex", () => {
        const err = validateHighlightRule(rule("(\\d+", { is_regex: true }));
        expect(err?.kind).toBe("invalid");
    });

    it("rejects zero-width regex", () => {
        const err = validateHighlightRule(rule("^$", { is_regex: true }));
        expect(err?.kind).toBe("zero_width");
    });

    it("ignores non-regex rules", () => {
        expect(validateHighlightRule(rule("ERROR"))).toBeNull();
    });
});

describe("highlightPlain", () => {
    it("highlights date regex from issue #102", () => {
        const input = "log: 2026-06-09 09:05:02 done";
        const r = rule(
            "\\d{4}[-/]\\d{2}[-/]\\d{2}\\s\\d{2}:\\d{2}:\\d{2}",
            { is_regex: true, color: "#6EDAA0" }
        );
        const out = highlightPlain(input, compileHighlightRules([r]));
        const color = ansiColor("#6EDAA0");
        expect(out).toBe(
            `log: ${color}2026-06-09 09:05:02${RST} done`
        );
        expect(out).toContain("2026-06-09 09:05:02");
    });

    it("matches literal keyword case-insensitively by default", () => {
        const out = highlightPlain("error ERROR", compileHighlightRules([
            rule("ERROR"),
        ]));
        const color = ansiColor("#FF6B6B");
        expect(out).toBe(`${color}error${RST} ${color}ERROR${RST}`);
    });

    it("respects literal case sensitivity when enabled", () => {
        const out = highlightPlain("error ERROR", compileHighlightRules([
            rule("ERROR", { is_case_sensitive: true }),
        ]));
        const color = ansiColor("#FF6B6B");
        expect(out).toBe(`error ${color}ERROR${RST}`);
    });

    it("matches regex case-insensitively by default", () => {
        const out = highlightPlain("ABC abc", compileHighlightRules([
            rule("[a-z]+", { is_regex: true }),
        ]));
        const color = ansiColor("#FF6B6B");
        expect(out).toBe(`${color}ABC${RST} ${color}abc${RST}`);
    });

    it("respects regex case sensitivity when enabled", () => {
        const out = highlightPlain("ABC abc", compileHighlightRules([
            rule("[a-z]+", { is_regex: true, is_case_sensitive: true }),
        ]));
        const color = ansiColor("#FF6B6B");
        expect(out).toBe(`ABC ${color}abc${RST}`);
    });

    it("treats regex alternation as a single rule", () => {
        const out = highlightPlain("foo bar", compileHighlightRules([
            rule("foo|bar", { is_regex: true }),
        ]));
        const color = ansiColor("#FF6B6B");
        expect(out).toBe(`${color}foo${RST} ${color}bar${RST}`);
    });

    it("skips disabled and invalid rules without throwing", () => {
        const out = highlightPlain("hello", compileHighlightRules([
            rule("(\\d+", { is_regex: true }),
            rule("hello"),
        ]));
        const color = ansiColor("#FF6B6B");
        expect(out).toBe(`${color}hello${RST}`);
    });

    it("keeps the first rule when multiple rules overlap at same position", () => {
        const out = highlightPlain("ERRORs", compileHighlightRules([
            rule("ERROR", { color: "#FF0000" }),
            rule("[A-Z]+", { is_regex: true, color: "#00FF00" }),
        ]));
        const firstColor = ansiColor("#FF0000");
        expect(out).toBe(`${firstColor}ERROR${RST}s`);
    });

    it("returns plain text when no rules are enabled", () => {
        const input = "nothing here";
        expect(highlightPlain(input, compileHighlightRules([
            rule("ERROR", { enabled: false }),
        ]))).toBe(input);
    });
});
