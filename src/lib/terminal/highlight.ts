import type { HighlightRule } from "../stores/app.svelte.ts";

const RST = "\x1b[0m";

/** Hex color → ANSI 24-bit true color escape. */
export function ansiColor(hex: string): string {
    const h = hex.replace("#", "");
    if (h.length !== 6) return "";
    const r = parseInt(h.slice(0, 2), 16);
    const g = parseInt(h.slice(2, 4), 16);
    const b = parseInt(h.slice(4, 6), 16);
    return `\x1b[38;2;${r};${g};${b}m`;
}

export interface CompiledHighlightRule {
    keyword: string;
    color: string;
    enabled: boolean;
    is_regex: boolean;
    is_case_sensitive: boolean;
    source: string;
    regex: RegExp | null;
}

export type HighlightValidationError =
    | { kind: "invalid"; message: string }
    | { kind: "zero_width" };

/**
 * 校验单条高亮规则（仅正则模式需要校验）。
 * 返回 null 表示合法；否则返回错误类型，由 UI 映射到 i18n 文案。
 */
export function validateHighlightRule(rule: HighlightRule): HighlightValidationError | null {
    if (!rule.is_regex || !rule.keyword) return null;
    const flags = rule.is_case_sensitive ? "g" : "gi";
    try {
        const re = new RegExp(rule.keyword, flags);
        if (re.test("")) {
            return { kind: "zero_width" };
        }
    } catch (e: any) {
        return { kind: "invalid", message: e?.message || String(e) };
    }
    return null;
}

/** 预编译高亮规则，生成可在终端输出中复用的 RegExp。非法规则会被标记为 regex=null。 */
export function compileHighlightRules(rules: HighlightRule[]): CompiledHighlightRule[] {
    return rules.map((rule) => {
        if (!rule.enabled || !rule.keyword) {
            return { ...rule, source: "", regex: null };
        }
        const source = rule.is_regex
            ? rule.keyword
            : rule.keyword.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
        const flags = rule.is_case_sensitive ? "g" : "gi";
        try {
            const regex = new RegExp(source, flags);
            return { ...rule, source, regex };
        } catch (e) {
            console.warn("[highlight] invalid regex, skipping:", rule.keyword, e);
            return { ...rule, source, regex: null };
        }
    });
}

interface Match {
    start: number;
    end: number;
    color: string;
    index: number;
}

/**
 * 对一段“纯文本”（不含 ANSI 转义序列）应用高亮规则。
 * 规则按列表顺序处理；同一位置多条规则匹配时保留第一条。
 */
export function highlightPlain(plain: string, compiled: CompiledHighlightRule[]): string {
    const enabled = compiled.filter((c) => c.enabled && c.keyword && c.regex);
    if (!enabled.length) return plain;

    const matches: Match[] = [];

    for (let i = 0; i < enabled.length; i++) {
        const rule = enabled[i];
        const re = rule.regex!;
        re.lastIndex = 0;
        let m: RegExpExecArray | null;
        while ((m = re.exec(plain)) !== null) {
            const start = m.index;
            const end = start + m[0].length;
            if (end === start) {
                // 零宽匹配：前进一格避免死循环（防御性保护）
                re.lastIndex = start + 1;
                continue;
            }
            matches.push({ start, end, color: rule.color, index: i });
        }
    }

    matches.sort((a, b) => {
        if (a.start !== b.start) return a.start - b.start;
        return a.index - b.index;
    });

    const parts: string[] = [];
    let pos = 0;
    let lastEnd = -1;

    for (const m of matches) {
        if (m.start < pos || m.start < lastEnd) continue;
        parts.push(plain.slice(pos, m.start));
        parts.push(ansiColor(m.color));
        parts.push(plain.slice(m.start, m.end));
        parts.push(RST);
        pos = m.end;
        lastEnd = m.end;
    }

    parts.push(plain.slice(pos));
    return parts.join("");
}
