// One panel, drawn live (analytics TODO A4.2–A4.3): what it is stored as —
// a spec, a fit and an output's name, some text — sent to `renderPanel`, and
// the answer drawn with the explorer's and the model editor's own views.
//
// Live, so a report's plot includes the row added a minute ago, and a panel
// on a dataset edited on the other side of a split view is drawn again. A
// panel whose dataset or fit has been deleted shows the server's sentence
// saying so, as does a plot that no longer draws; neither is a failure of the
// screen that holds it.

import { Suspense, lazy, useEffect, useState, type DragEvent, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type { RenderPanelResponse } from "../client";
import { AnalysisView } from "../explorer/TestResults";
import { isAnalysis } from "../explorer/tests";
import { T, useT } from "../i18n";
import { OutputTableView } from "../models/Outputs";
import type { OutputTable } from "../models/outputs";
import { useChanges } from "../panes";
import { PlotView } from "../plot/PlotView";
import { isRefused, type PlotData, type PlotSpec, type TableData } from "../plot/spec";
import { SummaryTable } from "../plot/SummaryTable";
import { useDocumentTheme, type Theme } from "../theme";
import { Markdown } from "./Markdown";
import type { MapData, MapSpec } from "../map/spec";
import { setPanelDrag, type Panel } from "./panel";

/** MapLibre, fetched when a map panel is first drawn. */
const MapView = lazy(() => import("../map/MapView").then((m) => ({ default: m.MapView })));

/** How a panel is drawn where it is put: a report's are still, in vector
 * graphics, on white paper; elsewhere they follow the screen. */
export type PanelLook = {
  /** No tooltips, highlighting or brushing (A4.4). */
  still?: boolean;
  renderer?: "canvas" | "svg";
  /** The colour scheme, when it is not the document's. */
  theme?: Theme;
};

export function PanelView({ panel, look = {} }: { panel: Panel; look?: PanelLook }) {
  const { t } = useT();
  const [answer, setAnswer] = useState<RenderPanelResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Bumped when a dataset or a model changes on the other side of a split.
  const [version, setVersion] = useState(0);
  useChanges(["dataset", "model"], () => setVersion((v) => v + 1));

  const key = JSON.stringify(panel);
  useEffect(() => {
    if (panel.kind === "text") return;
    let live = true;
    api
      .renderPanel({ panel })
      .then((a) => {
        if (!live) return;
        setAnswer(a);
        setError(null);
      })
      .catch((err: unknown) => live && setError(errorMessage(err, t("Could not draw this panel."))));
    return () => {
      live = false;
    };
    // The panel is compared by value (`key`).
  }, [key, version, t]);

  if (panel.kind === "text") return <TextView markdown={panel.content.markdown} />;
  if (error) return <Alert variant="danger" className="m-2 small">{error}</Alert>;
  // `an-panel-loading`: a report waits for it to go before printing.
  if (!answer) return <Spinner animation="border" size="sm" className="m-3 an-panel-loading" />;
  if (answer.error) return <Missing>{answer.error}</Missing>;

  switch (panel.kind) {
    case "plot":
      return <PlotAnswer spec={panel.content.spec} plot={answer.plot} categorical={answer.categorical ?? undefined} look={look} />;
    case "summary_table":
      return isRefused(answer.table) ? (
        <Missing>{answer.table.error}</Missing>
      ) : answer.table ? (
        <SummaryTable data={answer.table as unknown as TableData} />
      ) : null;
    case "test_result":
      return (
        <div className="an-panel-tests">
          {panel.content.plot && (
            <PlotAnswer spec={panel.content.plot} plot={answer.plot} categorical={answer.categorical ?? undefined} look={look} />
          )}
          {isAnalysis(answer.tests) ? (
            <AnalysisView analysis={answer.tests} />
          ) : (
            <Missing>{(answer.tests as { error?: string } | undefined)?.error ?? t("No test applies.")}</Missing>
          )}
        </div>
      );
    case "fit_table": {
      const table = (answer.output as { table?: OutputTable } | undefined)?.table;
      return table ? <OutputTableView table={table} /> : <Missing>{t("This fit has no such table.")}</Missing>;
    }
    case "map":
      return answer.map ? (
        <MapAnswer spec={panel.content.spec} data={answer.map as unknown as MapData} look={look} />
      ) : null;
    case "custom":
      return <Missing>{t("This panel's kind is not installed.")}</Missing>;
  }
}

/** A map panel (A5.13): in a report, drawn once and shown as an image. */
function MapAnswer({ spec, data, look }: { spec: MapSpec; data: MapData; look: PanelLook }) {
  const documentTheme = useDocumentTheme();
  return (
    <div className="an-panel-map">
      <Suspense fallback={<Spinner animation="border" size="sm" className="m-3 an-panel-loading" />}>
        <MapView spec={spec} data={data} theme={look.theme ?? documentTheme} still={look.still} />
      </Suspense>
    </div>
  );
}

function PlotAnswer({
  spec,
  plot,
  categorical,
  look,
}: {
  spec: PlotSpec;
  plot: unknown;
  categorical?: string[];
  look: PanelLook;
}) {
  const documentTheme = useDocumentTheme();
  if (isRefused(plot)) return <Missing>{plot.error}</Missing>;
  if (!plot) return null;
  return (
    <div className="an-panel-plot">
      <PlotView
        spec={spec}
        data={plot as PlotData}
        theme={look.theme ?? documentTheme}
        categorical={categorical}
        renderer={look.renderer}
        still={look.still}
      />
    </div>
  );
}

function Missing({ children }: { children: ReactNode }) {
  return (
    <Alert variant="secondary" className="m-2 small">
      {children}
    </Alert>
  );
}

/** A text panel, in Markdown. */
function TextView({ markdown }: { markdown: string }) {
  return <Markdown source={markdown} className="an-panel-text an-markdown" />;
}

/**
 * The handle a panel is dragged by: the panel made when the drag starts, so
 * what is dragged is what is on the screen at that moment. `make` answers
 * `null` while there is nothing to drag, and the drag is then refused.
 */
export function DragHandle({ make, label }: { make: () => Panel | null; label?: string }) {
  const { t } = useT();
  const onDragStart = (e: DragEvent) => {
    const panel = make();
    if (!panel) {
      e.preventDefault();
      return;
    }
    setPanelDrag(e.dataTransfer, panel);
  };
  return (
    <span
      className="an-drag-handle"
      draggable
      role="button"
      tabIndex={-1}
      onDragStart={onDragStart}
      title={label ?? t("Drag into a report")}
      aria-label={label ?? t("Drag into a report")}
    >
      <span aria-hidden>⠿</span> <T text="Drag" />
    </span>
  );
}
