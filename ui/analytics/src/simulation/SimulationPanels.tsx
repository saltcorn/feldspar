// The profiler and the scenario comparison as panels (analytics TODO A7.8).
//
// A panel holds the Simulation workspace's model, level, inputs and scenarios
// as they were when it was dragged (`simulation.ts`'s `SimulationContent`).
// On a dashboard the profiler is **interactive**: its inputs can be moved,
// and the prediction and curves follow, as in the workspace; what is moved
// is the viewer's until they leave, and the panel keeps what it was dropped
// with. In a report (`still`) it is drawn once at the inputs it was dragged
// with, beside the saved scenarios' predictions; the comparison is drawn at
// its saved scenarios in either.

import { useMemo, useState } from "react";
import Spinner from "react-bootstrap/Spinner";

import type { PanelLook } from "../panels/PanelView";
import { useDocumentTheme } from "../theme";
import {
  ComparisonView,
  InputControls,
  PredictionView,
  ProfileCurves,
  SimError,
  useChartLook,
  useComparison,
  useProfile,
  type Target,
} from "./SimulationViews";
import { setValue, stateOfContent, type SimulationContent } from "./simulation";

function targetOf(content: SimulationContent): Target {
  return { model: content.model, fit: content.fit, level: content.level };
}

/** The profiler panel. */
export function ProfilerPanelView({ content, look }: { content: SimulationContent; look: PanelLook }) {
  const documentTheme = useDocumentTheme();
  const chartLook = useChartLook(look.theme ?? documentTheme, look.still);
  const start = useMemo(() => stateOfContent(content), [content]);
  const [state, setState] = useState(start);
  const target = useMemo(() => targetOf(content), [content]);
  const values = look.still ? start.values : state.values;
  const { profile, error, stale } = useProfile(target, values);
  const { comparison } = useComparison(look.still ? target : null, start.scenarios);
  if (error) return <SimError>{error}</SimError>;
  if (!profile) return <Spinner animation="border" size="sm" className="m-3 an-panel-loading" />;
  return (
    <div className={look.still ? "an-sim-panel an-sim-still" : "an-sim-panel"}>
      {!look.still && (
        <InputControls
          inputs={profile.inputs}
          values={state.values}
          onChange={(input, value) => setState((s) => setValue(s, input, value))}
        />
      )}
      <div className={stale && !look.still ? "an-sim-profile an-stale" : "an-sim-profile"}>
        <PredictionView prediction={profile.prediction} outcome={profile.outcome} />
        <ProfileCurves profile={profile} values={values} look={chartLook} renderer={look.renderer} />
      </div>
      {look.still && comparison && <ComparisonView comparison={comparison} look={chartLook} renderer={look.renderer} />}
    </div>
  );
}

/** The scenario comparison panel. */
export function ScenariosPanelView({ content, look }: { content: SimulationContent; look: PanelLook }) {
  const documentTheme = useDocumentTheme();
  const chartLook = useChartLook(look.theme ?? documentTheme, look.still);
  const start = useMemo(() => stateOfContent(content), [content]);
  const target = useMemo(() => targetOf(content), [content]);
  const { comparison, error } = useComparison(target, start.scenarios);
  if (error) return <SimError>{error}</SimError>;
  if (!comparison) return <Spinner animation="border" size="sm" className="m-3 an-panel-loading" />;
  return (
    <div className="an-sim-panel">
      <ComparisonView comparison={comparison} look={chartLook} renderer={look.renderer} />
    </div>
  );
}
