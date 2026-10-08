// The Markdown of text panels and a report's text blocks (analytics TODO
// A4.4): the part of it a report needs — headings, paragraphs, lists, quotes,
// code, rules, and strong, emphasis, code and links within a line — parsed
// into a small tree that `Markdown.tsx` draws as React elements.
//
// A tree, not HTML: nothing a person types reaches `innerHTML`, so there is
// nothing to sanitise, and a link goes only where an `http(s):`, `mailto:` or
// relative address goes — anything else (`javascript:`) stays as its text.
//
// `_` makes emphasis only at a word's edge, so a column name like
// `price_per_m2` reads as written.

/** A run within a line. */
export type Inline =
  | { kind: "text"; text: string }
  | { kind: "code"; text: string }
  | { kind: "strong"; children: Inline[] }
  | { kind: "em"; children: Inline[] }
  | { kind: "link"; href: string; children: Inline[] };

/** A block of a document. */
export type MdBlock =
  | { kind: "heading"; level: 1 | 2 | 3 | 4 | 5 | 6; children: Inline[] }
  | { kind: "paragraph"; children: Inline[] }
  | { kind: "list"; ordered: boolean; start: number; items: Inline[][] }
  | { kind: "quote"; children: Inline[] }
  | { kind: "code"; text: string }
  | { kind: "rule" };

const HEADING = /^(#{1,6})\s+(.*?)\s*#*\s*$/;
const BULLET = /^\s{0,3}[-*+]\s+(.*)$/;
const NUMBERED = /^\s{0,3}(\d{1,9})[.)]\s+(.*)$/;
const QUOTE = /^\s{0,3}>\s?(.*)$/;
const FENCE = /^\s{0,3}(```|~~~)/;
const RULE = /^\s{0,3}([-*_])(\s*\1){2,}\s*$/;

/** Whether a line starts a block of its own, ending a paragraph. */
function startsBlock(line: string): boolean {
  return HEADING.test(line) || BULLET.test(line) || NUMBERED.test(line) || QUOTE.test(line) || FENCE.test(line) || RULE.test(line);
}

/** A document's blocks. */
export function parseMarkdown(source: string): MdBlock[] {
  const lines = source.replace(/\r\n?/g, "\n").split("\n");
  const blocks: MdBlock[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (line.trim() === "") {
      i++;
      continue;
    }
    const fence = FENCE.exec(line);
    if (fence) {
      const body: string[] = [];
      i++;
      while (i < lines.length && !lines[i].trimStart().startsWith(fence[1])) body.push(lines[i++]);
      i++; // the closing fence, or past the end
      blocks.push({ kind: "code", text: body.join("\n") });
      continue;
    }
    if (RULE.test(line)) {
      blocks.push({ kind: "rule" });
      i++;
      continue;
    }
    const heading = HEADING.exec(line);
    if (heading) {
      const level = heading[1].length as 1 | 2 | 3 | 4 | 5 | 6;
      blocks.push({ kind: "heading", level, children: parseInline(heading[2]) });
      i++;
      continue;
    }
    if (QUOTE.test(line)) {
      const body: string[] = [];
      while (i < lines.length && QUOTE.test(lines[i])) body.push(QUOTE.exec(lines[i++])![1]);
      blocks.push({ kind: "quote", children: parseInline(body.join(" ").trim()) });
      continue;
    }
    const numbered = NUMBERED.exec(line);
    if (BULLET.test(line) || numbered) {
      const ordered = Boolean(numbered);
      const item = ordered ? NUMBERED : BULLET;
      const items: Inline[][] = [];
      while (i < lines.length && item.test(lines[i])) {
        const m = item.exec(lines[i++])!;
        let text = ordered ? m[2] : m[1];
        // A line indented under an item continues it.
        while (i < lines.length && /^\s{2,}\S/.test(lines[i]) && !startsBlock(lines[i])) text += " " + lines[i++].trim();
        items.push(parseInline(text));
      }
      blocks.push({ kind: "list", ordered, start: numbered ? Number(numbered[1]) : 1, items });
      continue;
    }
    const body: string[] = [];
    while (i < lines.length && lines[i].trim() !== "" && (body.length === 0 || !startsBlock(lines[i]))) {
      body.push(lines[i++].trim());
    }
    blocks.push({ kind: "paragraph", children: parseInline(body.join(" ")) });
  }
  return blocks;
}

/** Whether a link may go to `href`: the web, mail, or this site. */
export function safeHref(href: string): boolean {
  const h = href.trim();
  if (/^(https?:|mailto:)/i.test(h)) return true;
  // No scheme at all: a relative address or a fragment.
  return !/^[a-z][a-z0-9+.-]*:/i.test(h) && !h.startsWith("//");
}

const ESCAPABLE = /[\\`*_[\]()#+\-.!>~|{}]/;
const WORD = /[\p{L}\p{N}]/u;

/** The runs of one line (or a paragraph's lines joined). */
export function parseInline(s: string): Inline[] {
  const out: Inline[] = [];
  let text = "";
  const flush = () => {
    if (text !== "") out.push({ kind: "text", text });
    text = "";
  };
  let i = 0;
  while (i < s.length) {
    const c = s[i];
    if (c === "\\" && i + 1 < s.length && ESCAPABLE.test(s[i + 1])) {
      text += s[i + 1];
      i += 2;
      continue;
    }
    if (c === "`") {
      const end = s.indexOf("`", i + 1);
      if (end > i + 1) {
        flush();
        out.push({ kind: "code", text: s.slice(i + 1, end) });
        i = end + 1;
        continue;
      }
    }
    if ((c === "*" || c === "_") && opens(s, i, c)) {
      const double = s[i + 1] === c;
      const width = double ? 2 : 1;
      const end = closing(s, i + width, c, width);
      if (end !== -1) {
        flush();
        const children = parseInline(s.slice(i + width, end));
        out.push(double ? { kind: "strong", children } : { kind: "em", children });
        i = end + width;
        continue;
      }
    }
    if (c === "[") {
      const link = readLink(s, i);
      if (link) {
        flush();
        if (safeHref(link.href)) out.push({ kind: "link", href: link.href, children: parseInline(link.text) });
        else out.push(...parseInline(link.text));
        i = link.end;
        continue;
      }
    }
    text += c;
    i++;
  }
  flush();
  return out;
}

/** Whether the `*` or `_` at `i` can open emphasis: something follows it that
 * is not a space, and (for `_`) it does not sit inside a word. */
function opens(s: string, i: number, c: string): boolean {
  const width = s[i + 1] === c ? 2 : 1;
  const next = s[i + width];
  if (next === undefined || /\s/.test(next)) return false;
  return c === "*" || i === 0 || !WORD.test(s[i - 1]);
}

/** Where the emphasis opened before `from` closes: the next run of `width`
 * markers after something that is not a space (for `_`, not followed by a
 * word character), or -1. */
function closing(s: string, from: number, c: string, width: number): number {
  const marker = c.repeat(width);
  for (let j = s.indexOf(marker, from + 1); j !== -1; j = s.indexOf(marker, j + 1)) {
    if (/\s/.test(s[j - 1])) continue;
    // `**` inside `*…*` is not its end, nor is `*` the half of a `**`.
    if (width === 1 && s[j + 1] === c) {
      j++;
      continue;
    }
    if (c === "_" && j + width < s.length && WORD.test(s[j + width])) continue;
    return j;
  }
  return -1;
}

/** `[text](href)` at `i`, or null. */
function readLink(s: string, i: number): { text: string; href: string; end: number } | null {
  let depth = 0;
  let close = -1;
  for (let j = i; j < s.length; j++) {
    if (s[j] === "\\") {
      j++;
      continue;
    }
    if (s[j] === "[") depth++;
    if (s[j] === "]" && --depth === 0) {
      close = j;
      break;
    }
  }
  if (close === -1 || s[close + 1] !== "(") return null;
  const end = s.indexOf(")", close + 2);
  if (end === -1) return null;
  const href = s.slice(close + 2, end).trim();
  if (href === "" || /\s/.test(href)) return null;
  return { text: s.slice(i + 1, close), href, end: end + 1 };
}

/** The plain text of some runs: a heading's words, for a title or a label. */
export function plainText(runs: Inline[]): string {
  return runs.map((r) => (r.kind === "text" || r.kind === "code" ? r.text : plainText(r.children))).join("");
}
