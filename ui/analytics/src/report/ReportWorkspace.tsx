// The Report workspace (analytics TODO A4.3): where panels are dropped.
//
// A4.3 brings it as the sink of drag and drop: a panel dragged from the Data
// explorer or the model editor — on the other side of a split view — is
// dropped here as a copy of its own, at the end or before the block it is
// dropped on, and drawn live from its dataset. A block can be removed. A4.4
// makes it a document: headings, text, page breaks, reordering and the page.

import { useCallback, useMemo, useState, type DragEvent } from "react";
import Button from "react-bootstrap/Button";

import { T, useT } from "../i18n";
import { carriesPanel, readPanelDrag } from "../panels/panel";
import { PanelView } from "../panels/PanelView";
import type { WorkspaceProps, WorkspaceState } from "../workspaces/WorkspaceFrame";
import { addPanel, readReport, removeBlock, type ReportState } from "./state";

export function ReportWorkspace({ state: raw, setState }: WorkspaceProps) {
  const { t } = useT();
  const report = useMemo(() => readReport(raw), [raw]);
  const update = useCallback(
    (change: (r: ReportState) => ReportState) =>
      setState((current) => ({ ...current, ...change(readReport(current)) }) as WorkspaceState),
    [setState],
  );
  // Where a drop would go: before a block, or "end".
  const [target, setTarget] = useState<string | null>(null);

  const accept = (e: DragEvent, at: string) => {
    if (!carriesPanel(e.dataTransfer)) return;
    e.preventDefault();
    e.stopPropagation();
    e.dataTransfer.dropEffect = "copy";
    setTarget(at);
  };
  const drop = (e: DragEvent, before?: string) => {
    if (!carriesPanel(e.dataTransfer)) return;
    e.preventDefault();
    e.stopPropagation();
    setTarget(null);
    const panel = readPanelDrag(e.dataTransfer);
    if (panel) update((r) => addPanel(r, panel, before));
  };

  return (
    <div
      className="an-report"
      onDragOver={(e) => accept(e, "end")}
      onDragLeave={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setTarget(null);
      }}
      onDrop={(e) => drop(e)}
    >
      <div className="an-report-page">
        {report.blocks.length === 0 && (
          <p className="text-secondary an-report-empty">
            <T text="Drag a plot from the Data explorer, or an output from the model editor, into this report. Split the view to have both on the screen." />
          </p>
        )}
        {report.blocks.map((block) => (
          <section
            key={block.id}
            className={target === block.id ? "an-report-block an-drop-before" : "an-report-block"}
            onDragOver={(e) => accept(e, block.id)}
            onDrop={(e) => drop(e, block.id)}
            data-panel-kind={block.panel.kind}
          >
            <div className="an-report-block-head">
              {block.panel.title && <h3 className="h5 mb-0">{block.panel.title}</h3>}
              <Button
                size="sm"
                variant="link"
                className="ms-auto p-0 text-secondary an-report-remove"
                aria-label={t("Remove {name} from the report", { name: block.panel.title ?? t("this panel") })}
                onClick={() => update((r) => removeBlock(r, block.id))}
              >
                ×
              </Button>
            </div>
            <PanelView panel={block.panel} />
          </section>
        ))}
        <div className={target === "end" ? "an-report-end an-drop-before" : "an-report-end"} aria-hidden />
      </div>
    </div>
  );
}
