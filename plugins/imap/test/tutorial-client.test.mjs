// docs/tutorial-mail.md Step 10's `src/mail/mailHtml.ts`, taken out of the tutorial and run, so
// the code a reader copies is the code that was tested (TODO-imap.md M10.1). Node strips the
// types; the DOM is jsdom's, from ui/builder's dependencies (`npm ci` in ui/builder). Skipped when
// that is not installed.

import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFileSync, writeFileSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const tutorial = join(here, "..", "..", "..", "docs", "tutorial-mail.md");

const JSDOM = (() => {
  try {
    return createRequire(join(here, "..", "..", "..", "ui", "builder", "package.json"))("jsdom").JSDOM;
  } catch {
    return null;
  }
})();
const skip = JSDOM ? false : "jsdom is not installed (npm ci in ui/builder)";

const ts = (() => {
  try {
    return createRequire(join(here, "..", "..", "..", "ui", "builder", "package.json"))("typescript");
  } catch {
    return null;
  }
})();

/** The fenced block that starts with `// <path>`, from the tutorial. */
export function snippet(path) {
  const text = readFileSync(tutorial, "utf8");
  const blocks = [...text.matchAll(/```(?:ts|tsx)\n([\s\S]*?)```/g)].map((m) => m[1]);
  const found = blocks.find((b) => b.startsWith(`// ${path}\n`));
  if (!found) throw new Error(`docs/tutorial-mail.md has no block for ${path}`);
  return found;
}

async function loadMailHtml() {
  globalThis.DOMParser = new JSDOM("").window.DOMParser;
  const dir = mkdtempSync(join(tmpdir(), "mail-tutorial-"));
  const file = join(dir, "mailHtml.ts");
  writeFileSync(file, snippet("src/mail/mailHtml.ts"));
  return import(pathToFileURL(file).href);
}

/** The srcdoc as a document again, to look at. */
const reparse = (srcdoc) => new JSDOM(srcdoc).window.document;

test("the tutorial's component blocks are all there", () => {
  for (const path of ["src/mail/Folder.tsx", "src/mail/mailHtml.ts", "src/mail/Message.tsx", "src/mail/Reply.tsx"]) {
    assert.ok(snippet(path).length > 100, path);
  }
  // Message.tsx imports what mailHtml.ts exports.
  assert.match(snippet("src/mail/Message.tsx"), /import \{ mailDocument, type InlineImages \} from "\.\/mailHtml";/);
});

test("the tutorial's TypeScript and TSX blocks are valid syntax", { skip: ts ? false : "typescript is not installed (npm ci in ui/builder)" }, () => {
  for (const path of ["src/mail/Folder.tsx", "src/mail/mailHtml.ts", "src/mail/Message.tsx", "src/mail/Reply.tsx"]) {
    const out = ts.transpileModule(snippet(path), {
      fileName: path.split("/").pop(),
      reportDiagnostics: true,
      compilerOptions: { jsx: ts.JsxEmit.ReactJSX, target: ts.ScriptTarget.ES2022, module: ts.ModuleKind.ESNext },
    });
    const errors = (out.diagnostics || []).map((d) => ts.flattenDiagnosticMessageText(d.messageText, "\n"));
    assert.deepEqual(errors, [], path);
  }
});

test("mailDocument: scripts, frames and handlers go, remote images wait, cid: images point at their files", { skip }, async () => {
  const { mailDocument } = await loadMailHtml();
  const html = `<html><head><meta http-equiv="refresh" content="0;url=https://evil.example/">
      <base href="https://evil.example/"><link rel="stylesheet" href="https://evil.example/a.css">
      <script>parent.alert(1)</script></head>
    <body onload="alert(2)">
      <p onclick="alert(3)">Hello <b>Ann</b></p>
      <img src="cid:logo@shop.example" alt="logo">
      <img src="CID:%3Cchart%40shop.example%3E">
      <img src="cid:missing@shop.example">
      <img src="https://tracker.example/pixel.gif?u=ann" width="1">
      <img src="//cdn.example/banner.png" srcset="https://cdn.example/banner@2x.png 2x">
      <table background="http://cdn.example/bg.png"><tr><td>x</td></tr></table>
      <img src="data:image/gif;base64,R0lGODlhAQABAAAAACw=">
      <a href="https://shop.example/order/17">your order</a>
      <a href="javascript:alert(4)">click</a>
      <iframe src="https://evil.example/"></iframe><object data="x.swf"></object>
      <form action="https://evil.example/"><input name="pw"></form>
    </body></html>`;
  const images = { "logo@shop.example": "/api/mail_attachments/7/file", "<chart@shop.example>": "/api/mail_attachments/8/file" };
  const { srcdoc, blocked } = mailDocument(html, images);
  assert.ok(srcdoc.startsWith("<!doctype html><html>"), srcdoc.slice(0, 40));
  const doc = reparse(srcdoc);

  // The policy is the first thing in the document, and it allows no remote images.
  const first = doc.head.firstElementChild;
  assert.equal(first.getAttribute("http-equiv"), "Content-Security-Policy");
  const csp = first.getAttribute("content");
  assert.match(csp, /^default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; font-src data:$/);
  assert.equal(doc.querySelectorAll("meta").length, 1, "only our meta: the refresh is gone");

  for (const gone of ["script", "base", "link", "iframe", "object"]) assert.equal(doc.querySelector(gone), null, gone);
  assert.equal(srcdoc.includes("alert("), false, "no handler or script survives");
  assert.equal(doc.querySelector("p").innerHTML, "Hello <b>Ann</b>");

  const imgs = [...doc.querySelectorAll("img")];
  assert.equal(imgs[0].getAttribute("src"), "/api/mail_attachments/7/file");
  assert.equal(imgs[1].getAttribute("src"), "/api/mail_attachments/8/file", "CID: is case-blind and may be URL-encoded with brackets");
  assert.equal(imgs[2].hasAttribute("src"), false, "a cid: with no inline image loads nothing");
  assert.equal(imgs[3].hasAttribute("src"), false);
  assert.equal(imgs[3].getAttribute("data-blocked-src"), "https://tracker.example/pixel.gif?u=ann");
  assert.equal(imgs[4].hasAttribute("src") || imgs[4].hasAttribute("srcset"), false, "protocol-relative and srcset too");
  assert.equal(doc.querySelector("table").hasAttribute("background"), false);
  assert.match(imgs[5].getAttribute("src"), /^data:image\/gif/, "data: images are the mail's own");
  assert.equal(blocked, 3);

  const [order, js] = doc.querySelectorAll("a");
  assert.equal(order.getAttribute("href"), "https://shop.example/order/17");
  assert.equal(order.getAttribute("target"), "_blank");
  assert.equal(order.getAttribute("rel"), "noopener noreferrer");
  assert.equal(js.hasAttribute("href"), false);
});

test("mailDocument: Show images lets remote images load, and nothing else changes", { skip }, async () => {
  const { mailDocument } = await loadMailHtml();
  const html = `<p>Hi</p><img src="https://cdn.example/banner.png"><img src="cid:logo"><script>x()</script>`;
  const { srcdoc, blocked } = mailDocument(html, { logo: "/api/mail_attachments/1/file" }, true);
  const doc = reparse(srcdoc);
  assert.equal(blocked, 0);
  assert.match(doc.head.firstElementChild.getAttribute("content"), /img-src 'self' data: https: http:;/);
  const [banner, logo] = doc.querySelectorAll("img");
  assert.equal(banner.getAttribute("src"), "https://cdn.example/banner.png");
  assert.equal(logo.getAttribute("src"), "/api/mail_attachments/1/file");
  assert.equal(doc.querySelector("script"), null, "scripts never come back");
});

test("mailDocument: a fragment, an empty body and broken markup all make a document", { skip }, async () => {
  const { mailDocument } = await loadMailHtml();
  for (const html of ["", "just text", "<div><p>unclosed <b>bold", "<html><body></body></html>", "</body></html><p>after the end"]) {
    const { srcdoc, blocked } = mailDocument(html, {});
    const doc = reparse(srcdoc);
    assert.equal(doc.head.firstElementChild.getAttribute("http-equiv"), "Content-Security-Policy", html);
    assert.equal(blocked, 0);
  }
});
