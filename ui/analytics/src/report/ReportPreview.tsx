// A report drawn read-only (the model editor's fit report modal): the same
// page, blocks and panels as the Report workspace, without its toolbar, its
// drop zones or its editing — what "Save as report workspace" would store, as
// it will look once stored.
//
// The page element is handed back (`pageRef`) so its owner can print it with
// `printReport`, as the workspace's **Export PDF** does.

import { createElement, type Ref } from "react";

import { Markdown } from "../panels/MarkdownView";
import { PanelView, type PanelLook } from "../panels/PanelView";
import { MARGIN_MM, paper } from "./pages";
import type { Block, ReportState } from "./state";

/** How a report draws its panels: still, in vector graphics, on white paper. */
const LOOK: PanelLook = { still: true, renderer: "svg", theme: "light" };

export function ReportPreview({ report, pageRef }: { report: ReportState; pageRef?: Ref<HTMLDivElement> }) {
  const sheet = paper(report.page);
  return (
    <div
      ref={pageRef}
      className="an-report-page an-report-preview"
      data-bs-theme="light"
      style={{ maxWidth: `${sheet.width}mm`, padding: `${MARGIN_MM}mm` }}
    >
      {report.blocks.map((block) => (
        <section key={block.id} className={`an-report-block an-block-${block.kind.replace("_", "-")}`}>
          <BlockBody block={block} />
        </section>
      ))}
    </div>
  );
}

function BlockBody({ block }: { block: Block }) {
  switch (block.kind) {
    case "panel":
      return (
        <>
          {block.panel.title && <h3 className="h5 an-panel-title">{block.panel.title}</h3>}
          <PanelView panel={block.panel} look={LOOK} />
        </>
      );
    case "heading":
      return createElement(`h${block.level}`, { className: `an-block-heading-text an-heading-${block.level}` }, block.text);
    case "text":
      return (
        <div className="an-block-text-body">
          <Markdown source={block.markdown} className="an-markdown" />
        </div>
      );
    case "page_break":
      return <hr className="an-preview-page-break" />;
  }
}
