// Printing a report (analytics TODO A4.5): **Export PDF** is the browser's
// print dialog, whose "Save as PDF" is the PDF. There is no PDF on the server.
//
// The print stylesheet (`analytics.css`, "Printing a report") does the work:
// while `<html>` has `an-printing`, everything but the element marked
// `an-print-root` and what holds it is hidden, the holders stop scrolling and
// clipping, and the root is printed on its own named `@page` — one rule for
// each paper size and orientation (`pageName`), so nothing has to be written
// into a `<style>` at print time. This module only marks the report, opens
// the dialog, and takes the marks off afterwards.

import { pageName, type Page } from "./pages";

/** The classes and the title a report is printed with. */
export type PrintMarks = { root: string[]; html: string[]; title: string };

/** What `printReport` puts on the page while the dialog is open: the root's
 * classes (which name its `@page`), `<html>`'s, and the document's title,
 * which the browser offers as the PDF's file name. */
export function printMarks(page: Page, name: string | undefined, fallback: string): PrintMarks {
  const title = name?.trim() || fallback;
  return { root: ["an-print-root", `an-page-${pageName(page)}`], html: ["an-printing"], title };
}

/** How long to wait for the report's panels to draw before printing anyway. */
const READY_TIMEOUT_MS = 15_000;

/** Whether a report's panels are all drawn: none still shows its spinner. */
export function panelsDrawn(root: ParentNode): boolean {
  return root.querySelector(".an-panel-loading") === null;
}

/**
 * Print the report drawn in `root`, once its panels have drawn (or after a
 * while, so a panel that never answers does not stop the export). Resolves
 * when the dialog has closed.
 */
export async function printReport(root: HTMLElement, page: Page, name: string | undefined, fallback: string): Promise<void> {
  const started = Date.now();
  while (!panelsDrawn(root) && Date.now() - started < READY_TIMEOUT_MS) {
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  const marks = printMarks(page, name, fallback);
  const html = document.documentElement;
  const title = document.title;
  root.classList.add(...marks.root);
  html.classList.add(...marks.html);
  document.title = marks.title;
  await new Promise<void>((resolve) => {
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      window.removeEventListener("afterprint", finish);
      window.removeEventListener("focus", finish);
      root.classList.remove(...marks.root);
      html.classList.remove(...marks.html);
      document.title = title;
      resolve();
    };
    // Not a timer after `print()`: some browsers return from it at once and
    // lay the page out for print afterwards.
    window.addEventListener("afterprint", finish);
    window.print();
    // A browser that never sends `afterprint`: the window has its focus back
    // when the dialog is gone.
    setTimeout(() => window.addEventListener("focus", finish), 1000);
  });
}
