// The corpus run of TODO-imap.md M7.5: every mail of the public corpus (M1.1) and the generated
// mails (M1.2) through parse.js, with (a)–(c) checked for each and (d) compared against
// Stalwart's expected output. `corpus.test.mjs` asserts on the result; run this file directly
// for the report:
//
//   node test/corpus.mjs [--python] [--verbose]
//
// `--python` adds (e), a differential report against Python's `email` package.

import { readFileSync, readdirSync, existsSync, statSync } from "node:fs";
import { join, dirname, relative } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const require = createRequire(import.meta.url);
const { parse, PLACEHOLDERS } = require("../parse.js");

const here = dirname(fileURLToPath(import.meta.url));
export const corpusDir = join(here, "corpus");
const generatedDir = join(here, "fixtures/generated");

/** Every file under `dir` (not following `.git`), recursively. */
function walk(dir) {
  if (!existsSync(dir)) return [];
  const out = [];
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === ".git") continue;
    const path = join(dir, entry.name);
    if (entry.isDirectory()) out.push(...walk(path));
    else if (entry.isFile()) out.push(path);
  }
  return out.sort();
}

/** Which files of each source are mails. The rest are expected outputs, attachments and licences. */
const SOURCES = [
  ["stalwart", "resources/eml", (f) => f.endsWith(".eml")],
  ["zbateson", "tests/_data/emails", (f, rel) => f.endsWith(".txt") && !rel.includes("/")],
  ["mikel-mail", "spec/fixtures/emails", (f) => f.endsWith(".eml")],
  ["mime4j", "core/src/test/resources", (f) => f.endsWith(".msg")],
  ["cpython", "Lib/test/test_email/data", (f) => /^msg_.*\.txt$/.test(f)],
  ["php-mime-mail-parser", "tests/mails", (f, rel) => !rel.includes("/")],
  ["flanker", "tests/fixtures/messages", (f, rel) => !rel.startsWith("attachments/")],
  ["spamscope", "tests/mails", () => true],
  ["mailparser", "test/fixtures", (f) => f.endsWith(".eml")],
  ["spamassassin", "", () => true],
];

/** `[{ source, path, name }]`: the corpus mails that have been fetched, and the generated ones. */
export function corpusFiles() {
  const files = [];
  for (const [source, sub, isMail] of SOURCES) {
    const root = join(corpusDir, source, sub);
    for (const path of walk(root)) {
      const rel = relative(root, path);
      const base = rel.split("/").pop();
      if (source === "spamassassin" && path.endsWith(".tar.bz2")) continue;
      if (isMail(base, rel)) files.push({ source, path, name: `${source}/${rel}` });
    }
  }
  for (const path of walk(generatedDir)) {
    if (path.endsWith(".eml")) files.push({ source: "generated", path, name: `generated/${relative(generatedDir, path)}` });
  }
  return files;
}

export const norm = (s) => String(s ?? "").replace(/\s+/g, " ").trim();

/** (a)–(c) for one parsed mail: the problems, as strings. */
export function sanity({ message, attachments }) {
  const problems = [];
  if (!norm(message.body_text) && !norm(message.body_html)) problems.push("(b) no body and no placeholder");
  const strings = [];
  const collect = (v) => {
    if (typeof v === "string") strings.push(v);
    else if (Array.isArray(v)) v.forEach(collect);
    else if (v && typeof v === "object" && !Buffer.isBuffer(v)) Object.values(v).forEach(collect);
  };
  collect(message);
  attachments.forEach((a) => collect({ ...a, content: null }));
  if (strings.some((s) => s.includes("\0"))) problems.push("(c) a NUL byte in a value");
  return problems;
}

// ------------------------------------------------------------------ (d) Stalwart

/** The first value of a root header in Stalwart's expected output. */
function stalwartHeader(part, name) {
  const h = (part.headers || []).find((x) => x.name.toLowerCase() === name);
  return h ? h.value : undefined;
}

/** The first mailbox of a Stalwart address value, wherever groups put it. */
function firstMailbox(value) {
  if (!value || typeof value !== "object") return null;
  if (value.mailbox) return value.mailbox;
  for (const v of Object.values(value)) {
    const found = Array.isArray(v) ? v.map(firstMailbox).find(Boolean) : firstMailbox(v);
    if (found) return found;
  }
  return null;
}

/** What Stalwart's `.json` next to an `.eml` says the four compared fields are. */
export function stalwartExpected(emlPath) {
  const jsonPath = emlPath.replace(/\.eml$/, ".json");
  if (!existsSync(jsonPath)) return null;
  const j = JSON.parse(readFileSync(jsonPath, "utf8"));
  const msg = j.messages[0];
  const root = j.parts[msg.root];
  const subject = stalwartHeader(root, "subject");
  const from = firstMailbox(stalwartHeader(root, "from"));
  const textParts = msg.text_body.map((id) => j.parts[id]);
  return {
    subject: subject && typeof subject === "object" && "text" in subject ? subject.text : subject === undefined ? "" : null,
    from_addr: from ? from.address || null : null,
    // Only plain text is compared: Stalwart's HTML-to-text is not html-to-text's.
    text: textParts.length && textParts.every((p) => p.kind === "text") ? textParts.map((p) => p.text).join("\n") : null,
    attachments: msg.attachments.length,
  };
}

/** The (d) disagreements of one parsed Stalwart mail: `[{ field, ours, theirs }]`. */
export function compareStalwart(parsed, expected) {
  const out = [];
  const { message, attachments } = parsed;
  if (expected.subject !== null && norm(message.subject) !== norm(expected.subject)) {
    out.push({ field: "subject", ours: message.subject, theirs: expected.subject });
  }
  if ((message.from_addr || "").toLowerCase() !== (expected.from_addr || "").toLowerCase()) {
    out.push({ field: "from", ours: message.from_addr, theirs: expected.from_addr });
  }
  // (c) removes NUL, and (b) puts a placeholder where Stalwart has no text: neither is a disagreement.
  const theirText = expected.text === null ? null : expected.text.replace(/\0/g, "");
  const placeholder = Object.values(PLACEHOLDERS).includes(message.body_text);
  if (theirText !== null && norm(message.body_text) !== norm(theirText) && !(placeholder && !norm(theirText))) {
    out.push({ field: "text", ours: message.body_text.slice(0, 200), theirs: expected.text.slice(0, 200) });
  }
  if (attachments.length !== expected.attachments) {
    out.push({ field: "attachments", ours: attachments.length, theirs: expected.attachments });
  }
  return out;
}

// ------------------------------------------------------------------ (e) Python

const PYTHON = `
import email, email.policy, json, sys
from email.utils import getaddresses
out = {}
for path in sys.argv[1:]:
    try:
        with open(path, "rb") as fh:
            msg = email.message_from_binary_file(fh, policy=email.policy.default)
        frm = getaddresses([str(msg.get("from", ""))])
        body = msg.get_body(preferencelist=("plain",))
        text = None
        if body is not None:
            try:
                text = body.get_content()
            except Exception:
                text = None
        out[path] = {
            "subject": str(msg.get("subject", "") or ""),
            "from_addr": frm[0][1] if frm and frm[0][1] else None,
            "text": text,
            "attachments": len(list(msg.iter_attachments())),
        }
    except Exception as e:
        out[path] = {"error": str(e)}
print(json.dumps(out))
`;

/** Python's view of each mail, by path: `{ subject, from_addr, text, attachments }`, or `null` without Python. */
export function pythonView(paths) {
  const probe = spawnSync("python3", ["-I", "-c", "pass"]);
  if (probe.status !== 0) return null;
  const out = {};
  for (let i = 0; i < paths.length; i += 500) {
    const run = spawnSync("python3", ["-I", "-c", PYTHON, ...paths.slice(i, i + 500)], { encoding: "utf8", maxBuffer: 1 << 28 });
    if (run.status !== 0) throw new Error(run.stderr);
    Object.assign(out, JSON.parse(run.stdout));
  }
  return out;
}

// ------------------------------------------------------------------ the run

/**
 * Parse every mail. `{ results: [{ name, source, problems, stalwart, ms }], seconds }`, where
 * `problems` are (a)–(c) and `stalwart` the (d) disagreements (Stalwart mails only).
 */
export async function runCorpus({ files = corpusFiles(), onResult } = {}) {
  const started = process.hrtime.bigint();
  const results = [];
  for (const file of files) {
    const source = readFileSync(file.path);
    const t0 = process.hrtime.bigint();
    let parsed;
    const problems = [];
    try {
      parsed = await parse(source, { internalDate: statSync(file.path).mtime, flags: [], size: source.length });
    } catch (err) {
      problems.push(`(a) threw: ${err && err.stack}`);
    }
    const ms = Number(process.hrtime.bigint() - t0) / 1e6;
    let stalwart = null;
    if (parsed) {
      problems.push(...sanity(parsed));
      if (file.source === "stalwart") {
        const expected = stalwartExpected(file.path);
        if (expected) stalwart = compareStalwart(parsed, expected);
      }
    }
    const result = { name: file.name, source: file.source, path: file.path, problems, stalwart, ms, parsed };
    if (onResult) onResult(result);
    // The bytes are dropped, and the four compared fields kept, for (e).
    const summary = parsed && {
      message: { subject: parsed.message.subject, from_addr: parsed.message.from_addr, body_text: parsed.message.body_text },
      attachments: parsed.attachments.map(() => null),
    };
    results.push({ ...result, parsed: summary, importError: parsed ? parsed.message.import_error : null });
  }
  return { results, seconds: Number(process.hrtime.bigint() - started) / 1e9 };
}

/** (e): per field, how many mails Python's view disagrees on, and the disagreements. */
export function comparePython(results, view) {
  const counts = { subject: 0, from: 0, text: 0, attachments: 0, python_error: 0 };
  const diffs = [];
  for (const r of results) {
    const p = view[r.path];
    if (!r.parsed) continue;
    if (!p || p.error) {
      counts.python_error++;
      continue;
    }
    for (const d of compareStalwart(r.parsed, p)) {
      counts[d.field]++;
      diffs.push({ name: r.name, ...d });
    }
  }
  return { counts, diffs };
}

/** `known-differences.md`: the (d) table as a `Set` of `"<mail> <field>"`, and the (e) ceilings. */
export function knownDifferences() {
  const text = readFileSync(join(here, "known-differences.md"), "utf8");
  const stalwart = new Set();
  const python = {};
  for (const line of text.split("\n")) {
    const d = /^\|\s*`([^`]+)`\s*\|\s*(\w+)\s*\|/.exec(line);
    if (d) stalwart.add(`${d[1]} ${d[2]}`);
    const e = /^\|\s*\(e\) (\w+)\s*\|\s*(\d+)\s*\|/.exec(line);
    if (e) python[e[1]] = Number(e[2]);
  }
  return { stalwart, python };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const verbose = process.argv.includes("--verbose");
  const files = corpusFiles();
  const { results, seconds } = await runCorpus({ files });
  const bySource = {};
  for (const r of results) {
    const s = (bySource[r.source] ||= { mails: 0, problems: 0, importErrors: 0, stalwart: 0 });
    s.mails++;
    if (r.problems.length) s.problems++;
    if (r.importError) s.importErrors++;
    if (r.stalwart && r.stalwart.length) s.stalwart++;
  }
  console.table(bySource);
  console.log(`${results.length} mails in ${seconds.toFixed(1)} s; slowest:`, results.sort((a, b) => b.ms - a.ms).slice(0, 3).map((r) => `${r.name} ${r.ms.toFixed(0)} ms`));
  for (const r of results) {
    for (const p of r.problems) console.log(`PROBLEM ${r.name}: ${p}`);
    if (r.importError) console.log(`IMPORT ERROR ${r.name}: ${r.importError}`);
  }
  const known = knownDifferences().stalwart;
  for (const r of results.filter((r) => r.stalwart)) {
    for (const d of r.stalwart) {
      const key = `${r.name} ${d.field}`;
      if (verbose || !known.has(key)) console.log(`${known.has(key) ? "known" : "NEW"} ${key}: ours ${JSON.stringify(d.ours)} / stalwart ${JSON.stringify(d.theirs)}`);
    }
  }
  const differing = new Set(results.flatMap((r) => (r.stalwart || []).map((d) => `${r.name} ${d.field}`)));
  for (const k of known) if (!differing.has(k)) console.log(`FIXED (remove from known-differences.md) ${k}`);
  if (process.argv.includes("--python")) {
    const view = pythonView(files.map((f) => f.path));
    if (!view) console.log("(e) skipped: no python3");
    else {
      const { counts, diffs } = comparePython(results, view);
      if (verbose) for (const d of diffs) console.log(`(e) ${d.name} ${d.field}: ours ${JSON.stringify(d.ours)} / python ${JSON.stringify(d.theirs)}`);
      console.log("(e) disagreements with Python's email package, by field:", counts);
    }
  }
}
