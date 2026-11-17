// The Simulation workspace (analytics TODO A7.6–A7.8; the goals document's
// "Simulation workspace"): a fitted model's inputs as controls, the
// prediction and its interval at them, and a profile curve per input — all
// from one `profileModel` call, asked again a moment after a control moves.
//
// The inputs can be saved as named **scenarios**, kept in the workspace's
// state, and compared by their predicted distributions (`compareScenarios`),
// the current inputs beside them when they are not one of them. **Score**
// adds the model's predictions to a dataset: a new dataset based on the one
// chosen, with a Model predictions operation, which opens in the Dataset
// editor or in a new Data explorer. The profiler and the comparison are drag
// sources (A7.8): interactive on a dashboard, still in a report.

import { useCallback, useEffect, useMemo, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import InputGroup from "react-bootstrap/InputGroup";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type { ListDatasetsResponse, ListModelsResponse } from "../client";
import { T, useT } from "../i18n";
import { DragHandle } from "../panels/PanelView";
import { useAnnounce, useChanges, usePane } from "../panes";
import { useDocumentTheme } from "../theme";
import type { WorkspaceProps, WorkspaceState } from "../workspaces/WorkspaceFrame";
import {
  ComparisonView,
  FitNote,
  InputControls,
  PredictionView,
  ProfileCurves,
  SimError,
  useChartLook,
  useComparison,
  useProfile,
  type Target,
} from "./SimulationViews";
import {
  DEFAULT_LEVEL,
  LEVELS,
  applyScenario,
  freeName,
  nextScenarioName,
  pickModel,
  profilerPanel,
  readSimState,
  removeScenario,
  resetValues,
  saveScenario,
  scenariosPanel,
  scenariosToCompare,
  scoreDefinition,
  setValue,
  simulatable,
  type SimulationState,
} from "./simulation";

type ModelItem = ListModelsResponse[number];
type DatasetItem = ListDatasetsResponse[number];

export function SimulationWorkspace({ state: raw, setState, readOnly }: WorkspaceProps) {
  const { t } = useT();
  const theme = useDocumentTheme();
  const look = useChartLook(theme);
  const state = useMemo(() => readSimState(raw), [raw]);
  const update = useCallback(
    (change: (s: SimulationState) => SimulationState) =>
      setState((current) => change(readSimState(current)) as unknown as WorkspaceState),
    [setState],
  );

  const [models, setModels] = useState<ModelItem[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  // A model refitted, or its dataset edited, on the other side of a split.
  const [version, setVersion] = useState(0);
  useChanges(["model", "dataset"], () => setVersion((v) => v + 1));
  useEffect(() => {
    let live = true;
    api
      .listModels()
      .then((ms) => live && setModels(ms))
      .catch((err: unknown) => live && setLoadError(errorMessage(err, t("Could not load the models."))));
    return () => {
      live = false;
    };
  }, [t, version]);

  const choices = useMemo(() => simulatable(models ?? []), [models]);
  const model = models?.find((m) => m.id === state.model);
  const target: Target | null = useMemo(
    () => (state.model ? { model: state.model, fit: state.fit, level: state.level } : null),
    [state.model, state.fit, state.level],
  );
  const { profile, error, stale } = useProfile(target, state.values, version);
  const compared = useMemo(() => scenariosToCompare(state, t), [state, t]);
  const { comparison, error: compareError } = useComparison(
    state.scenarios.length > 0 ? target : null,
    compared,
    version,
  );

  const [scenarioName, setScenarioName] = useState("");
  const [scenarioError, setScenarioError] = useState<string | null>(null);
  const save = () => {
    const name = scenarioName.trim() === "" ? nextScenarioName(state, t) : scenarioName;
    const { state: next, error: why } = saveScenario(state, name, t);
    if (why) {
      setScenarioError(why);
      return;
    }
    setScenarioError(null);
    setScenarioName("");
    update(() => next);
  };

  if (loadError) return <Alert variant="danger" className="m-3">{loadError}</Alert>;
  if (!models) return <Spinner animation="border" size="sm" className="m-3" />;

  return (
    <div className="an-explorer an-sim">
      <aside className="an-columns an-sim-side" aria-label={t("Inputs")}>
        <Form.Group className="mb-3">
          <Form.Label htmlFor="sim-model" className="small fw-semibold">
            <T text="Model" />
          </Form.Label>
          <Form.Select
            id="sim-model"
            size="sm"
            value={state.model ?? ""}
            disabled={readOnly}
            onChange={(e) => update((s) => pickModel(s, e.target.value))}
          >
            <option value="" disabled>
              {t("Pick a fitted model")}
            </option>
            {choices.map((m) => (
              <option key={m.id} value={m.id}>
                {m.name}
              </option>
            ))}
            {state.model && !choices.some((m) => m.id === state.model) && (
              <option value={state.model}>{model?.name ?? t("(a model that is gone)")}</option>
            )}
          </Form.Select>
          {choices.length === 0 && (
            <Form.Text>
              <T text="No model has been fitted yet. Fit one in the model editor first." />
            </Form.Text>
          )}
        </Form.Group>
        {profile && (
          <>
            <div className="d-flex align-items-center mb-2">
              <span className="small fw-semibold">
                <T text="Inputs" />
              </span>
              <Button
                size="sm"
                variant="link"
                className="ms-auto p-0"
                disabled={Object.keys(state.values).length === 0}
                onClick={() => update(resetValues)}
              >
                <T text="All typical" />
              </Button>
            </div>
            <InputControls
              inputs={profile.inputs}
              values={state.values}
              onChange={(input, value) => update((s) => setValue(s, input, value))}
            />
          </>
        )}
      </aside>

      <section className="an-explorer-main">
        {!state.model ? (
          <p className="text-secondary p-3">
            <T text="Pick a fitted model: its inputs become controls, and its prediction and profile curves follow them." />
          </p>
        ) : (
          <>
            <div className="d-flex flex-wrap align-items-center gap-2 mb-2">
              {model && (
                <a href={`#/models/${encodeURIComponent(model.id)}`} className="fw-semibold">
                  {model.name}
                </a>
              )}
              {profile && <FitNote profile={profile} />}
              <Form.Select
                size="sm"
                className="w-auto ms-auto"
                aria-label={t("Interval")}
                value={String(state.level ?? DEFAULT_LEVEL)}
                onChange={(e) => update((s) => ({ ...s, level: Number(e.target.value) }))}
              >
                {LEVELS.map((l) => (
                  <option key={l} value={String(l)}>
                    {t("{level} % interval", { level: Math.round(l * 100) })}
                  </option>
                ))}
              </Form.Select>
              {profile && (
                <DragHandle
                  make={() => profilerPanel(state, model?.name, t)}
                  label={t("Drag the profiler into a report or a dashboard")}
                />
              )}
            </div>
            {error && <SimError>{error}</SimError>}
            {profile && (
              <div className={stale ? "an-sim-profile an-stale" : "an-sim-profile"}>
                <PredictionView prediction={profile.prediction} outcome={profile.outcome} />
                <ProfileCurves profile={profile} values={state.values} look={look} />
              </div>
            )}
            {!profile && !error && <Spinner animation="border" size="sm" className="m-3" />}

            <h2 className="h6 mt-4 d-flex align-items-center gap-2">
              <T text="Scenarios" />
              {comparison && (
                <span className="ms-auto">
                  <DragHandle
                    make={() => scenariosPanel(state, model?.name, t)}
                    label={t("Drag the comparison into a report or a dashboard")}
                  />
                </span>
              )}
            </h2>
            {!readOnly && (
              <InputGroup size="sm" className="mb-2 an-sim-save">
                <Form.Control
                  placeholder={nextScenarioName(state, t)}
                  aria-label={t("Scenario name")}
                  value={scenarioName}
                  onChange={(e) => setScenarioName(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") save();
                  }}
                />
                <Button variant="outline-primary" onClick={save} disabled={!profile}>
                  <T text="Save scenario" />
                </Button>
              </InputGroup>
            )}
            {scenarioError && <SimError>{scenarioError}</SimError>}
            {state.scenarios.length === 0 ? (
              <p className="small text-secondary">
                <T text="Save the inputs as a scenario, change them, and save another to compare the two." />
              </p>
            ) : (
              <ul className="an-sim-scenarios list-unstyled mb-2">
                {state.scenarios.map((s) => (
                  <li key={s.name} className="d-flex align-items-center gap-2">
                    <span className="text-truncate">{s.name}</span>
                    <Button size="sm" variant="link" className="ms-auto p-0" onClick={() => update((x) => applyScenario(x, s.name))}>
                      <T text="Use" />
                    </Button>
                    {!readOnly && (
                      <Button
                        size="sm"
                        variant="link"
                        className="p-0 text-danger"
                        onClick={() => update((x) => removeScenario(x, s.name))}
                      >
                        <T text="Delete" />
                      </Button>
                    )}
                  </li>
                ))}
              </ul>
            )}
            {compareError && <SimError>{compareError}</SimError>}
            {comparison && <ComparisonView comparison={comparison} look={look} />}

            {model && !readOnly && <ScorePanel model={model} target={target} />}
          </>
        )}
      </section>
    </div>
  );
}

/** Score (A7.7): a dataset with the model's predictions added to every row,
 * based on the one chosen, opened where the person asks. */
function ScorePanel({ model, target }: { model: ModelItem; target: Target | null }) {
  const { t } = useT();
  const pane = usePane();
  const changed = useAnnounce();
  const [datasets, setDatasets] = useState<DatasetItem[] | null>(null);
  const [source, setSource] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [made, setMade] = useState<{ id: string; name: string } | null>(null);
  useEffect(() => {
    let live = true;
    api
      .listDatasets()
      .then((ds) => live && setDatasets(ds))
      .catch((err: unknown) => live && setError(errorMessage(err, t("Could not load the datasets."))));
    return () => {
      live = false;
    };
  }, [t]);

  const score = async () => {
    const chosen = datasets?.find((d) => d.id === source);
    if (!chosen || !datasets) return;
    setBusy(true);
    setError(null);
    try {
      const def = scoreDefinition(
        { id: chosen.id, name: chosen.name, columns: chosen.columns as { name: string }[] },
        model,
        { fit: target?.fit, level: target?.level },
        t,
      );
      def.name = freeName(def.name, datasets.map((d) => d.name));
      const answer = await api.createDataset(def);
      const id = (answer.dataset as { id: string }).id;
      changed("dataset", id);
      setMade({ id, name: def.name });
      setDatasets([...datasets, { ...chosen, id, name: def.name }]);
    } catch (err) {
      setError(errorMessage(err, t("Could not score the dataset.")));
    } finally {
      setBusy(false);
    }
  };

  const explore = async () => {
    if (!made) return;
    try {
      const ws = await api.createWorkspace({ name: made.name, kind: "data_explorer" });
      await api.saveWorkspaceState(ws.id, { state: { dataset: made.id } });
      if (pane.split) pane.beside({ name: "workspace", id: ws.id });
      else pane.go({ name: "workspace", id: ws.id });
    } catch (err) {
      setError(errorMessage(err, t("Could not open the explorer.")));
    }
  };

  return (
    <section className="mt-4" aria-label={t("Score a dataset")}>
      <h2 className="h6">
        <T text="Score a dataset" />
      </h2>
      <p className="small text-secondary mb-2">
        <T text="Adds the prediction and its interval to every row of a dataset, as a new dataset based on it." />
      </p>
      <InputGroup size="sm" className="an-sim-save">
        <Form.Select aria-label={t("Dataset to score")} value={source} onChange={(e) => setSource(e.target.value)}>
          <option value="">{t("Pick a dataset")}</option>
          {(datasets ?? []).map((d) => (
            <option key={d.id} value={d.id}>
              {d.name}
            </option>
          ))}
        </Form.Select>
        <Button variant="outline-primary" disabled={!source || busy} onClick={() => void score()}>
          <T text="Score" />
        </Button>
      </InputGroup>
      {error && <SimError>{error}</SimError>}
      {made && (
        <Alert variant="success" className="py-2 small mt-2 d-flex flex-wrap align-items-center gap-2">
          <span>{t("Made the dataset “{name}”.", { name: made.name })}</span>
          <a className="ms-auto" href={pane.href({ name: "dataset", id: made.id })}>
            <T text="Open in the Dataset editor" />
          </a>
          <Button size="sm" variant="link" className="p-0" onClick={() => void explore()}>
            <T text="Open in a Data explorer" />
          </Button>
        </Alert>
      )}
    </section>
  );
}
