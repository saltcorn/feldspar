// tnef.js (TODO-imap.md M7.4): the verdammelt/tnef samples of the corpus (M1.1), and the TNEF
// mails of the generated set (M1.2).

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const tnef = require("../tnef.js");
const { split, decodeTransfer } = require("../parse.js");

const here = dirname(fileURLToPath(import.meta.url));
const samples = join(here, "corpus/tnef/tests/files/datafiles");
const generated = join(here, "fixtures/generated");

/** The winmail.dat inside a generated mail, decoded. */
async function winmailOf(file) {
  const nodes = await split(readFileSync(join(generated, file)));
  const { node, raw } = nodes.find(({ node }) => node.contentType === "application/ms-tnef");
  return decodeTransfer(raw, node.encoding);
}

// Each `.list` names the files `tnef --save-body=<name>-body` writes and the upstream test checks:
// the attachments, and `<name>-body.rtf` or `-body.html` for the body.
test("every verdammelt/tnef sample yields the attachments and body its .list names", { skip: !existsSync(samples) && "the corpus is not fetched" }, () => {
  const files = readdirSync(samples).filter((f) => f.endsWith(".tnef"));
  assert.equal(files.length, 15);
  for (const file of files) {
    const stem = file.replace(/\.tnef$/, "");
    const listed = readFileSync(join(samples, `${stem}.list`), "utf8").split("\n").filter(Boolean);
    const result = tnef.decode(readFileSync(join(samples, file)));
    assert.ok(result, `${file} is TNEF`);
    const names = result.attachments.map((a) => a.name);
    for (const name of listed) {
      if (name === `${stem}-body.rtf`) assert.ok(result.body_rtf.startsWith("{\\rtf"), `${file}: no RTF body`);
      else if (name === `${stem}-body.html`) assert.match(result.body_html, /<html/i, `${file}: no HTML body`);
      else assert.ok(names.includes(name), `${file}: ${name} is not among ${JSON.stringify(names)}`);
    }
  }
});

test("an embedded-object attachment (PR_ATTACH_DATA_OBJ) is unpacked with its long name", { skip: !existsSync(samples) && "the corpus is not fetched" }, () => {
  const result = tnef.decode(readFileSync(join(samples, "MAPI_ATTACH_DATA_OBJ.tnef")));
  const pdf = result.attachments.find((a) => a.name === "VIA_Nytt_1402.pdf");
  assert.equal(pdf.data.subarray(0, 5).toString("latin1"), "%PDF-");
  // An attachment whose data attribute is empty is still listed, as tnef(1) lists it.
  const empty = tnef.decode(readFileSync(join(samples, "data-before-name.tnef")));
  assert.deepEqual(empty.attachments.map((a) => [a.name, a.data.length]), [["AUTOEXEC.BAT", 0], ["CONFIG.SYS", 0], ["boot.ini", 289]]);
});

test("the generated TNEF mails: the compressed RTF, its text, and the attached file", async () => {
  const expected = JSON.parse(readFileSync(join(generated, "expected.json"), "utf8"));
  for (const mail of expected.filter((m) => m.tnef)) {
    const result = tnef.decode(await winmailOf(mail.file));
    assert.equal(result.body_rtf, mail.tnef.rtf, mail.file);
    assert.deepEqual(result.attachments.map((a) => a.name), mail.tnef.attachments, mail.file);
  }
  const rtfOnly = tnef.decode(await winmailOf("tnef-rtf-only.eml"));
  assert.equal(rtfOnly.body_text, "Hello from Outlook.\nGrüße aus Köln.");
  // HTML-in-RTF (\fromhtml1): the HTML it encapsulates, without the RTF-only text.
  const html = tnef.decode(await winmailOf("tnef-with-attachment.eml"));
  assert.equal(html.body_html.replace(/\s+/g, ""), "<html><body>HelloinHTML-in-RTF.<br></body></html>".replace(/\s+/g, ""));
});

test("rtfToText: escapes, \\uN with its stand-in, code pages, and skipped groups", () => {
  const rtf = "{\\rtf1\\ansi\\ansicpg1251{\\fonttbl{\\f0 Arial;}}{\\*\\generator x;}\\uc1 Caf\\u233? \\'cf\\'f0\\'e8\\par{\\info{\\title t}}\\{ok\\}\\tab end}";
  assert.equal(tnef.rtfToText(rtf), "Café При\n{ok}\tend");
  assert.equal(tnef.rtfToText(rtf, { html: true }), "", "not HTML-in-RTF");
});

test("damaged or foreign input never throws", () => {
  assert.equal(tnef.decode(Buffer.from("not tnef at all")), null);
  assert.equal(tnef.decode(null), null);
  const sample = existsSync(samples) ? readFileSync(join(samples, "two-files.tnef")) : null;
  if (sample) {
    for (const cut of [10, 100, 500, sample.length - 3]) {
      const result = tnef.decode(sample.subarray(0, cut));
      assert.ok(result, `cut at ${cut}`);
    }
    const garbled = Buffer.from(sample);
    for (let i = 20; i < garbled.length; i += 37) garbled[i] ^= 0xff;
    assert.ok(tnef.decode(garbled));
  }
  assert.equal(tnef.decompressRtf(Buffer.from("short")).length, 0);
});
