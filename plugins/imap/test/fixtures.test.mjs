// The test material of TODO-imap.md M1: the corpus lock (M1.1), the generated
// mails (M1.2) and the manual samples (M1.3).
//
//   node --test plugins/imap/test/
//
// No dependencies: M7 adds package.json and `npm test`, which will run this too.

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, existsSync, mkdtempSync, rmSync } from "node:fs";
import { join, dirname } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { generate, lzfuCrc } from "./fixtures/generate.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const generated = join(here, "fixtures/generated");
const expected = JSON.parse(readFileSync(join(generated, "expected.json"), "utf8"));

test("the committed mails are exactly what generate.mjs writes", () => {
  const out = mkdtempSync(join(tmpdir(), "imap-fixtures-"));
  try {
    generate(out);
    const fresh = readdirSync(out).sort();
    assert.deepEqual(readdirSync(generated).sort(), fresh, "run `node test/fixtures/generate.mjs` and commit the result");
    for (const name of fresh) {
      assert.ok(readFileSync(join(out, name)).equals(readFileSync(join(generated, name))), `${name} differs from what generate.mjs writes`);
    }
  } finally {
    rmSync(out, { recursive: true, force: true });
  }
});

test("every lesson and parsing rule M1.2 is for has a mail", () => {
  const covered = new Set(expected.flatMap((m) => m.covers));
  const lessons = ["§6.4", "§6.5", "§6.7", "§6.8", "§6.9", "§6.10", "§6.11", "§6.12", "§6.13"];
  for (const lesson of lessons) assert.ok(covered.has(lesson), `no mail covers ${lesson}`);
  const rules = ["every header is optional", "charsets", "undeclared 8-bit headers", "body choice", "no body part (encrypted", "text/calendar", "attachments only", "message/rfc822", "inline images", "TNEF", "NUL", "size limits", "binary-safe"];
  const text = [...covered].join("\n");
  for (const rule of rules) assert.ok(text.includes(rule), `no mail covers the §5.4 rule "${rule}"`);
  // The examples M1.2 names.
  for (const file of ["base64-windows-1252.eml", "unknown-charset-utf8.eml", "single-part-html.eml", "no-from-no-date.eml", "rfc822-inline-and-attached.eml", "cid-images.eml", "nul-byte.eml", "tnef-rtf-only.eml"]) {
    assert.ok(expected.some((m) => m.file === file), file);
  }
});

// Python's `email` package is an independent MIME parser: the tree it finds is
// the tree the generator meant to write.
test("Python's email package parses each mail into the intended MIME tree", (t) => {
  const python = spawnSync("python3", ["-I", "-c", "import sys; sys.exit(0)"]);
  if (python.status !== 0) return t.skip("python3 is not on the PATH");
  const script = `
import email, email.policy, json, sys, pathlib
out = {}
for path in sys.argv[1:]:
    msg = email.message_from_bytes(pathlib.Path(path).read_bytes(), policy=email.policy.default)
    out[pathlib.Path(path).name] = [p.get_content_type() for p in msg.walk()]
print(json.dumps(out))
`;
  const files = expected.map((m) => join(generated, m.file));
  const run = spawnSync("python3", ["-I", "-c", script, ...files], { encoding: "utf8", maxBuffer: 1 << 24 });
  assert.equal(run.status, 0, run.stderr);
  const trees = JSON.parse(run.stdout);
  for (const mail of expected) assert.deepEqual(trees[mail.file], mail.structure, mail.file);
});

// A reader written from MS-OXTNEF and MS-OXRTFCP, independent of the writer:
// checksums, the LZFu CRC, the decompressed RTF and the attachment names.
function decompressRtf(buf) {
  const compSize = buf.readUInt32LE(0);
  const rawSize = buf.readUInt32LE(4);
  assert.equal(buf.toString("latin1", 8, 12), "LZFu");
  const data = buf.subarray(16, compSize + 4);
  assert.equal(lzfuCrc(data), buf.readUInt32LE(12), "LZFu CRC");
  const prebuf =
    "{\\rtf1\\ansi\\mac\\deff0\\deftab720{\\fonttbl;}{\\f0\\fnil \\froman \\fswiss \\fmodern \\fscript \\fdecor MS Sans SerifSymbolArialTimes New RomanCourier{\\colortbl\\red0\\green0\\blue0\r\n\\par \\pard\\plain\\f0\\fs20\\b\\i\\u\\tab\\tx";
  const ring = Buffer.alloc(4096);
  ring.write(prebuf, 0, "latin1");
  let w = prebuf.length;
  const out = [];
  let i = 0;
  for (;;) {
    const ctl = data[i++];
    for (let bit = 0; bit < 8; bit++) {
      if (ctl & (1 << bit)) {
        const token = (data[i] << 8) | data[i + 1];
        i += 2;
        let offset = token >> 4;
        const length = (token & 0xf) + 2;
        if (offset === w) {
          assert.equal(out.length, rawSize);
          return Buffer.from(out).toString("latin1");
        }
        for (let k = 0; k < length; k++) {
          const byte = ring[offset];
          offset = (offset + 1) & 0xfff;
          ring[w] = byte;
          w = (w + 1) & 0xfff;
          out.push(byte);
        }
      } else {
        const byte = data[i++];
        ring[w] = byte;
        w = (w + 1) & 0xfff;
        out.push(byte);
      }
      assert.ok(i <= data.length, "ran past the compressed data without an end marker");
    }
  }
}

function readTnef(buf) {
  assert.equal(buf.readUInt32LE(0), 0x223e9f78, "TNEF signature");
  let pos = 6;
  let rtf = null;
  const attachments = [];
  while (pos < buf.length) {
    const level = buf[pos];
    const id = buf.readUInt32LE(pos + 1);
    const length = buf.readUInt32LE(pos + 5);
    const data = buf.subarray(pos + 9, pos + 9 + length);
    const checksum = buf.readUInt16LE(pos + 9 + length);
    assert.equal(checksum, data.reduce((s, b) => (s + b) & 0xffff, 0), `checksum of attribute ${id.toString(16)}`);
    pos += 11 + length;
    if (id === 0x00069003 || id === 0x00069005) {
      let p = 4;
      for (let n = data.readUInt32LE(0); n > 0; n--) {
        const type = data.readUInt16LE(p);
        const propId = data.readUInt16LE(p + 2);
        const size = data.readUInt32LE(p + 8);
        const value = data.subarray(p + 12, p + 12 + size);
        p += 12 + size + ((4 - (size % 4)) % 4);
        if (level === 1 && propId === 0x1009) rtf = decompressRtf(value);
        if (level === 2 && propId === 0x3707 && type === 0x001e) attachments.push(value.toString("latin1").replace(/\0$/, ""));
      }
    }
  }
  return { rtf, attachments };
}

test("the TNEF mails hold the RTF and the files they say they do", () => {
  const tnefMails = expected.filter((m) => m.tnef);
  assert.equal(tnefMails.length, 2);
  for (const mail of tnefMails) {
    const source = readFileSync(join(generated, mail.file), "latin1");
    const b64 = source.split('filename="winmail.dat"')[1].split(/\r\n\r\n/)[1].split("--")[0];
    const decoded = readTnef(Buffer.from(b64.replace(/\s+/g, ""), "base64"));
    assert.equal(decoded.rtf, mail.tnef.rtf, mail.file);
    assert.deepEqual(decoded.attachments, mail.tnef.attachments, mail.file);
  }
});

test("corpus.lock pins every source fetch-corpus.sh fetches", () => {
  const script = readFileSync(join(here, "fetch-corpus.sh"), "utf8");
  const gitSources = [...script.matchAll(/^\s+"([\w-]+)\|https:/gm)].map((m) => m[1]);
  const tarballs = script.match(/SPAMASSASSIN_FILES="([^"]+)"/)[1].split(/\s+/);
  assert.equal(gitSources.length, 10);
  const lock = new Map(
    readFileSync(join(here, "corpus.lock"), "utf8")
      .split("\n")
      .filter((line) => line && !line.startsWith("#"))
      .map((line) => line.split(" ")),
  );
  for (const name of gitSources) assert.match(lock.get(name) ?? "", /^[0-9a-f]{40}$/, `${name} has no commit in corpus.lock`);
  for (const file of tarballs) assert.match(lock.get(`spamassassin/${file}`) ?? "", /^[0-9a-f]{64}$/, `${file} has no sha256 in corpus.lock`);
  // Fetched or not, the corpus is never committed.
  const ignore = readFileSync(join(here, "../.gitignore"), "utf8");
  assert.match(ignore, /^test\/corpus\/$/m);
});

test("every manual sample is a raw .eml listed in MANUAL-SAMPLES.md", () => {
  const list = readFileSync(join(here, "MANUAL-SAMPLES.md"), "utf8");
  const dir = join(here, "fixtures/manual");
  const files = existsSync(dir) ? readdirSync(dir).filter((f) => f !== "README.md") : [];
  for (const file of files) {
    assert.match(file, /\.eml$/, `${file}: raw sources only`);
    assert.ok(list.includes(file), `${file} is not in MANUAL-SAMPLES.md`);
  }
});
