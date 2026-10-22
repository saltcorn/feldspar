// TODO-imap.md M7.5 and M7.6: every mail of the corpus through parse.js.
//
// The public corpus is fetched, not committed (`test/fetch-corpus.sh`). Without it, only the
// generated mails run here, and the corpus checks are skipped with a note.

import { test, before } from "node:test";
import assert from "node:assert/strict";
import { existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { corpusFiles, corpusDir, runCorpus, knownDifferences, pythonView, comparePython } from "./corpus.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const fetched = existsSync(join(corpusDir, "stalwart"));
const skipCorpus = fetched ? false : "the corpus is not fetched: run test/fetch-corpus.sh";

let run;
before(async () => {
  run = await runCorpus({ files: corpusFiles() });
});

test("(a)–(c): no mail throws, fails to parse, lacks a body or placeholder, or keeps a NUL", () => {
  assert.ok(run.results.length >= 40, "the generated mails at least");
  const bad = run.results.filter((r) => r.problems.length || r.importError);
  assert.deepEqual(
    bad.map((r) => `${r.name}: ${[...r.problems, r.importError].filter(Boolean).join("; ")}`),
    [],
  );
});

test("(d): the disagreements with Stalwart's expected output are exactly known-differences.md", { skip: skipCorpus }, () => {
  const known = knownDifferences().stalwart;
  const found = new Set(run.results.flatMap((r) => (r.stalwart || []).map((d) => `${r.name} ${d.field}`)));
  assert.ok(run.results.filter((r) => r.stalwart).length > 150, "Stalwart's mails were compared");
  assert.deepEqual([...found].filter((k) => !known.has(k)).sort(), [], "new disagreements: fix them, or add them with a reason");
  assert.deepEqual([...known].filter((k) => !found.has(k)).sort(), [], "fixed: remove these rows from known-differences.md");
});

test("(e): Python's email package disagrees no more often than known-differences.md allows", { skip: skipCorpus }, (t) => {
  const view = pythonView(run.results.map((r) => r.path));
  if (!view) return t.skip("python3 is not on the PATH");
  const { counts } = comparePython(run.results, view);
  const ceilings = knownDifferences().python;
  assert.equal(counts.python_error, 0);
  for (const field of ["subject", "from", "text", "attachments"]) {
    assert.ok(field in ceilings, `known-differences.md has no (e) ${field} row`);
    assert.ok(counts[field] <= ceilings[field], `(e) ${field}: ${counts[field]} disagreements, more than the ${ceilings[field]} known`);
  }
});

test("M7.6: the whole corpus parses in under a minute", { skip: skipCorpus }, () => {
  assert.ok(run.results.length > 6000, `${run.results.length} mails`);
  assert.ok(run.seconds < 60, `${run.seconds.toFixed(1)} s`);
});

test("M7.6: a 25 MB mail parses within the module worker's 256 MB heap", () => {
  // A child with the worker's heap bound: if the parse needed more, V8 would abort it.
  const script = `
    const { parse, SIZE_CAP } = require(${JSON.stringify(join(here, "../parse.js"))});
    const crypto = require("node:crypto");
    // base64 is 4 characters for 3 bytes, and a line break after every 76 characters.
    const attachment = crypto.randomBytes(Math.floor(((SIZE_CAP - 4096) * 3) / 4 * (76 / 78) / 57) * 57);
    const b64 = attachment.toString("base64").replace(/.{76}/g, "$&\\r\\n");
    const source = Buffer.from(
      "From: a@feldspar.test\\r\\nSubject: big\\r\\nContent-Type: multipart/mixed; boundary=b\\r\\n\\r\\n" +
      "--b\\r\\nContent-Type: text/plain\\r\\n\\r\\nSee the file.\\r\\n" +
      "--b\\r\\nContent-Type: application/octet-stream; name=big.bin\\r\\nContent-Transfer-Encoding: base64\\r\\n\\r\\n" + b64 + "\\r\\n--b--\\r\\n");
    const t0 = Date.now();
    parse(source).then(({ message, attachments }) => {
      const same = attachments.length === 1 && attachments[0].content.equals(attachment);
      console.log(JSON.stringify({ size: source.length, cap: SIZE_CAP, same, text: message.body_text, ms: Date.now() - t0,
        heap: require("node:v8").getHeapStatistics().heap_size_limit }));
    });
  `;
  const child = spawnSync(process.execPath, ["--max-heap-size=256", "-e", script], { encoding: "utf8", maxBuffer: 1 << 20 });
  assert.equal(child.status, 0, child.stderr);
  const out = JSON.parse(child.stdout);
  assert.ok(out.size > out.cap - 8192 && out.size <= out.cap, `the mail is ${out.size} bytes`);
  assert.equal(out.heap, 256 * 1024 * 1024, "the child ran with the bound");
  assert.equal(out.same, true, "the attachment's bytes survived");
  assert.equal(out.text.trim(), "See the file.");
});
