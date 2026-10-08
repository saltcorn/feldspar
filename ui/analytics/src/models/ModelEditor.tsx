// The model editor (analytics TODO A3.5–A3.6), at `#/models/<id>` and
// `#/models/new`: one model — its dataset, its provider, its settings and
// hyperparameters, its split — and below it the fit being looked at, with its
// outputs, and the model's earlier fits.
//
// A model is not a workspace: it is a global, named entity that formulas,
// actions and panels refer to, as a dataset is. What a workspace would have
// given — reopening as it was left — is the model's **view state**
// (`viewState.ts`): which outputs are folded, the optional plots chosen and
// the fit selected, written key by key as they change and read back on open.
// None of it touches the model: a fit does not record it, and nothing about
// "changed since this fit" looks at it.
//
// The form is the admin UI's model form, moved here (TODO "Predictive
// models" 6.2–6.4), and the order is the order the questions come in: **which
// data**, then **which provider and with what settings**, then **how the rows
// are divided**. For a provider that **binds data** (a posterior — Stan TODO
// §18) the form grows the program, its editor, the related datasets,
// dimensions and the binding table (`ModelBindings.tsx`), and the split and
// the grid are hidden, because a posterior is not divided and not searched.
//
// **Fit saves first**: a fit of what is on the screen and a save of what is on
// the screen are the same intention. While a fit runs its progress is pushed
// by the server (`progress.ts`); when it finishes it becomes the fit shown.

import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Spinner from "react-bootstrap/Spinner";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import { useAnnounce, useChanges, usePane } from "../panes";
import { routeHash } from "../router";
import { DatasetPicker, type DatasetItem } from "./DatasetPicker";
import { FitRunning } from "./FitRunning";
import { FitView } from "./FitView";
import { BindingSection, type BindingState } from "./ModelBindings";
import {
  BINDINGS_KEY,
  BINDING_FORM_KEYS,
  BINDING_KINDS,
  DEFAULT_SPLIT,
  DIMENSIONS_KEY,
  DIMENSION_KINDS,
  MAX_GRID_POINTS,
  POLICIES_KEY,
  buildHyperparameters,
  buildPolicies,
  formatTimestamp,
  gridPoints,
  headlineMetric,
  instanceLabel,
  orderInstances,
  outcomeSummary,
  parseDraft,
  parseDrafts,
  printDrafts,
  printGridValue,
  readHyperparameters,
  readMetrics,
  readModelDataset,
  readOutcome,
  readPolicies,
  readRelated,
  readSplit,
  relatedBody,
  type InstanceItem,
  type Interface,
  type ModelItem,
  type ProviderItem,
} from "./models";
import { useFitProgress } from "./progress";
import { SettingsFields, buildConfig, readConfig } from "./settings";
import { StatusBadge, fitTone } from "./StatusBadge";
import { chosenFit, editorPatch, readEditorView, toggled, type EditorView } from "./viewState";

/** How long the form waits after a keystroke before asking the server which
 * providers this dataset resolves. */
const DEBOUNCE_MS = 600;

/** The split as the form edits it: four boxes of text, so a half-typed `0.` is
 * not a number this form has to have an opinion about. */
type SplitForm = { train: string; validation: string; test: string; seed: string };

/** A split's part, as its box is labelled. */
function splitName(part: "train" | "validation" | "test", t: (s: string) => string): string {
  return part === "train" ? t("Train") : part === "validation" ? t("Validation") : t("Test");
}

export function ModelEditor({
  modelId,
  fit: routeFit,
  dataset: askedDataset,
}: {
  /** The model; none for a new one. */
  modelId?: string;
  /** The fit the address names (`?fit=`). */
  fit?: string;
  /** A new model's dataset, when it was made from one (`?dataset=`). */
  dataset?: string | null;
}) {
  const { t } = useT();
  const pane = usePane();
  const changed = useAnnounce();
  // Bumped when the model is changed on the other side of a split view, to
  // read it again.
  const [reloads, setReloads] = useState(0);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);

  const [id, setId] = useState<string | null>(modelId ?? null);
  const [model, setModel] = useState<ModelItem | null>(null);
  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [datasetId, setDatasetId] = useState("");
  const [provider, setProvider] = useState("");
  const [config, setConfig] = useState<Record<string, string>>({});
  const [hyper, setHyper] = useState<Record<string, string>>({});
  const [split, setSplit] = useState<SplitForm>({
    train: String(DEFAULT_SPLIT.train),
    validation: String(DEFAULT_SPLIT.validation),
    test: String(DEFAULT_SPLIT.test),
    seed: "0",
  });

  const [datasets, setDatasets] = useState<DatasetItem[]>([]);
  const [providers, setProviders] = useState<ProviderItem[]>([]);
  const [resolved, setResolved] = useState(false);
  const [instances, setInstances] = useState<InstanceItem[]>([]);
  const [binding, setBinding] = useState<BindingState>({
    related: [],
    dimensions: [],
    policies: {},
    bindings: {},
  });
  const [iface, setIface] = useState<Interface | null>(null);
  const [programCheck, setProgramCheck] = useState<string | null>(null);

  // The view state's part that is the editor's, and the fit on the screen.
  const [view, setView] = useState<EditorView>({ collapsed: [], plots: [], fit: null });
  const [selected, setSelected] = useState<string | null>(null);
  // The fit running now, followed over its progress socket.
  const [running, setRunning] = useState<string | null>(null);

  const dataset = useMemo(() => ({ dataset_id: datasetId }), [datasetId]);
  const datasetJson = JSON.stringify(dataset);
  const mainDataset = datasets.find((d) => d.id === datasetId) ?? null;
  const bindsData = Boolean(providers.find((p) => p.name === provider)?.binds_data);
  const configJson = JSON.stringify(
    configurationOf(specOf(providers, provider), config, bindsData ? binding : null),
  );

  // --- loading ---------------------------------------------------------------

  const fill = useCallback((existing: ModelItem) => {
    const stored = readModelDataset(existing.dataset);
    setModel(existing);
    setName(existing.name);
    setDescription(existing.description);
    setDatasetId(stored?.dataset_id ?? "");
    setProvider(existing.provider);
    setConfig(readConfig(existing.configuration));
    const configuration = (existing.configuration ?? {}) as Record<string, unknown>;
    setBinding({
      related: readRelated(existing.related),
      dimensions: Object.entries(printDrafts(DIMENSION_KINDS, configuration[DIMENSIONS_KEY])).map(
        ([dimension, draft]) => ({ name: dimension, draft }),
      ),
      policies: readPolicies(configuration[POLICIES_KEY]),
      bindings: printDrafts(BINDING_KINDS, configuration[BINDINGS_KEY]),
    });
    setHyper(readHyperparameters(existing.hyperparameters));
    const storedSplit = readSplit(existing.split);
    setSplit({
      train: String(storedSplit.train),
      validation: String(storedSplit.validation),
      test: String(storedSplit.test),
      seed: String(storedSplit.seed),
    });
    if (existing.error) setError(existing.error);
  }, []);

  useEffect(() => {
    let cancelled = false;
    const run = async () => {
      try {
        const [datasetList, existing, fits] = await Promise.all([
          api.listDatasets(),
          modelId ? api.getModel(modelId) : Promise.resolve(null),
          modelId ? api.listModelInstances(modelId) : Promise.resolve([]),
        ]);
        if (cancelled) return;
        setDatasets(datasetList);
        if (existing) {
          fill(existing);
          const remembered = readEditorView(existing.view_state);
          const ordered = orderInstances(fits);
          setView(remembered);
          setInstances(ordered);
          setSelected(chosenFit(routeFit, remembered.fit, ordered));
          setRunning(ordered.find((f) => f.status === "fitting")?.id ?? null);
        } else {
          setDatasetId(askedDataset ?? "");
        }
        setReady(true);
      } catch (err) {
        if (!cancelled) setLoadError(errorMessage(err, t("Could not load this model.")));
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [modelId, routeFit, askedDataset, fill, t, reloads]);

  // The providers, resolved against this dataset and this configuration where
  // they can be: `config_spec` then offers *these* columns and `outcome` says
  // what a fit would produce. A dataset that cannot be read falls back to the
  // unresolved declaration rather than an empty picker.
  useEffect(() => {
    if (!ready) return undefined;
    let cancelled = false;
    const parsed = JSON.parse(datasetJson) as typeof dataset;
    const usable = parsed.dataset_id !== "";
    const timer = window.setTimeout(() => {
      const query = usable ? { dataset: datasetJson, configuration: configJson } : undefined;
      void api
        .listModelProviders(query)
        .then((listed) => {
          if (cancelled) return;
          setProviders(listed.providers);
          setResolved(Boolean(usable));
        })
        .catch(() => {
          if (!usable) return;
          void api
            .listModelProviders()
            .then((listed) => {
              if (cancelled) return;
              setProviders(listed.providers);
              setResolved(false);
            })
            .catch(() => undefined);
        });
    }, DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
  }, [datasetJson, configJson, ready]);

  const loadInstances = useCallback(async (model: string) => {
    try {
      const fits = orderInstances(await api.listModelInstances(model));
      setInstances(fits);
      return fits;
    } catch {
      return [];
    }
  }, []);

  // Split view (A4.1): a dataset edited beside this model changes the
  // picker's columns and each fit's "dataset changed"; this model saved or
  // fitted on the other side is read again.
  useChanges(["dataset", "model"], (change) => {
    if (change.kind === "model") {
      if (change.id === id) setReloads((n) => n + 1);
      return;
    }
    void api
      .listDatasets()
      .then(setDatasets)
      .catch(() => undefined);
    if (id) void loadInstances(id);
  });

  // --- the view state --------------------------------------------------------

  /** Record a change to what the editor shows, here and in the model's view
   * state. Only the keys that changed are sent. */
  const changeView = (change: Partial<EditorView>) => {
    setView((v) => ({ ...v, ...change }));
    if (id) {
      void api.patchModelViewState(id, { patch: editorPatch(change) }).catch(() => undefined);
    }
  };

  const select = (fit: string) => {
    setSelected(fit);
    changeView({ fit });
  };

  // The fit that finished becomes the fit shown, and the model is read again
  // for its last fit.
  const live = useFitProgress(running, () => {
    const finished = running;
    setRunning(null);
    if (!id) return;
    changed("model", id);
    void loadInstances(id);
    void api.getModel(id).then(setModel).catch(() => undefined);
    if (finished) select(finished);
  });

  // --- what the form knows ---------------------------------------------------

  const chosen = providers.find((p) => p.name === provider);
  const outcome = readOutcome(chosen?.outcome);
  const hyperSpace = buildHyperparameters(chosen?.hyperparameters ?? [], hyper);
  const points = gridPoints(hyperSpace);
  const validationRows = Number(split.validation) > 0;
  // A binding provider's own controls edit these settings; the rest (the
  // sampler's, the runs store) are rendered as settings like any provider's.
  const settingsSpec = (chosen?.config_spec ?? []).filter(
    (field) => !bindsData || !BINDING_FORM_KEYS.includes(field.name),
  );
  // The fit on the screen: the one chosen, else the active one, else the
  // newest that fitted.
  const shown =
    selected ?? instances.find((f) => f.active)?.id ?? instances.find((f) => f.status === "fitted")?.id ?? null;

  // --- saving and fitting ----------------------------------------------------

  /** Everything on this form as `saveModel` takes it. */
  const body = () => ({
    id,
    name: name.trim(),
    description: description.trim(),
    provider,
    dataset,
    related: bindsData ? relatedBody(binding.related) : [],
    configuration: configurationOf(specOf(providers, provider), config, bindsData ? binding : null),
    hyperparameters: bindsData ? {} : hyperSpace,
    split: bindsData
      ? DEFAULT_SPLIT
      : {
          train: Number(split.train),
          validation: Number(split.validation),
          test: Number(split.test),
          seed: Math.trunc(Number(split.seed)) || 0,
        },
    attributes: {},
  });

  /** Save, and answer the model's id — the same path Fit takes. `over`
   * replaces parts of the form's body, for a change made in the same breath. */
  const save = async (over: Partial<ReturnType<typeof body>> = {}): Promise<string> => {
    const saved = await api.saveModel({ ...body(), ...over });
    setId(saved.id);
    setModel(saved);
    setError(saved.error ?? null);
    setProgramCheck(programCheckText(saved.program_check));
    changed("model", saved.id);
    // A new model now has an address of its own, so a reload comes back to it.
    // Replaced rather than navigated to, so the form is not built again under
    // the fit that is about to start.
    if (!modelId && !id) pane.replace({ name: "model", id: saved.id });
    return saved.id;
  };

  /** Save, showing a refusal on the form and rejecting with it. */
  const saveQuietly = async (over: Partial<ReturnType<typeof body>>) => {
    try {
      await save(over);
    } catch (err) {
      setError(errorMessage(err, t("Could not save the model.")));
      throw err;
    }
  };

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await save();
    } catch (err) {
      // The server's own refusal names the dataset column, the setting or the
      // hyperparameter that is wrong, which is the message to show.
      setError(errorMessage(err, t("Could not save the model.")));
    } finally {
      setBusy(false);
    }
  };

  const fit = async () => {
    setBusy(true);
    setError(null);
    try {
      const saved = await save();
      const started = await api.fitModel(saved, { name: null, description: null });
      await loadInstances(saved);
      setRunning(started.id);
    } catch (err) {
      setError(errorMessage(err, t("Could not start the fit.")));
    } finally {
      setBusy(false);
    }
  };

  const activate = async (instance: InstanceItem) => {
    try {
      await api.activateModelInstance(instance.id);
      if (id) await loadInstances(id);
    } catch (err) {
      setError(errorMessage(err, t("Could not activate that fit.")));
    }
  };

  const removeInstance = async (instance: InstanceItem) => {
    if (!window.confirm(t('Remove the fit "{name}"?', { name: instanceLabel(instance) }))) return;
    try {
      await api.deleteModelInstance(instance.id);
      if (selected === instance.id) {
        setSelected(null);
        changeView({ fit: null });
      }
      if (id) await loadInstances(id);
    } catch (err) {
      setError(errorMessage(err, t("Could not remove that fit.")));
    }
  };

  if (loadError) {
    return (
      <div className="an-page">
        <Alert variant="danger">{loadError}</Alert>
      </div>
    );
  }
  if (!ready) {
    return (
      <div className="an-page">
        <Spinner animation="border" size="sm" />
      </div>
    );
  }

  return (
    <div className="an-page an-model-editor">
      <div className="d-flex align-items-center gap-2 mb-3 flex-wrap">
        <Button variant="outline-secondary" size="sm" onClick={() => pane.go({ name: "home" })}>
          ← <T text="All models" />
        </Button>
        <h2 className="h3 mb-0">{id ? name || t("Model") : t("New model")}</h2>
        {model?.error && (
          <StatusBadge tone="red" title={model.error}>
            <T text="Cannot be fitted" />
          </StatusBadge>
        )}
      </div>
      {error && <Alert variant="danger">{error}</Alert>}

        <Form onSubmit={(e) => void submit(e)}>
          <Row>
            <Col md={6}>
              <Form.Group className="mb-3" controlId="modelName">
                <Form.Label>
                  <T text="Name" /><span className="text-danger"> *</span>
                </Form.Label>
                <Form.Control value={name} required onChange={(e) => setName(e.target.value)} />
                <Form.Text muted>
                  <T text='What predict("…") in a formula and models.get("…") in code name this model by.' />
                </Form.Text>
              </Form.Group>
            </Col>
            <Col md={6}>
              <Form.Group className="mb-3" controlId="modelDescription">
                <Form.Label><T text="Description" /></Form.Label>
                <Form.Control
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                />
              </Form.Group>
            </Col>
          </Row>

          {/* --- the dataset ------------------------------------------------ */}
          <Card className="mb-3">
            <Card.Header><T text="Dataset" /></Card.Header>
            <Card.Body>
              <DatasetPicker
                value={datasetId}
                onChange={setDatasetId}
                datasets={datasets}
                idPrefix="model-main"
                back={id ? routeHash({ name: "model", id }) : undefined}
                onCopied={async (copy) => {
                  setDatasets((list) => [...list.filter((d) => d.id !== copy.id), copy]);
                  setDatasetId(copy.id);
                  // Saved at once: the copy is this model's from now on, and
                  // the next thing done with it is usually to edit it.
                  if (id) await saveQuietly({ dataset: { dataset_id: copy.id } });
                }}
                beforeEdit={id ? () => saveQuietly({}) : undefined}
                previewNote={(preview) => (
                  <>
                    <T text="The first rows, and the type each column’s values came back as — which is what the provider’s form below is built from." />
                    {!bindsData &&
                      preview.primary_key &&
                      ` ${t("Split by {column}.", { column: preview.primary_key })}`}
                  </>
                )}
              />
            </Card.Body>
          </Card>

          {/* --- the provider ----------------------------------------------- */}
          <Card className="mb-3">
            <Card.Header><T text="Provider" /></Card.Header>
            <Card.Body>
              <Row>
                <Col md={5}>
                  <Form.Group className="mb-3" controlId="modelProvider">
                    <Form.Label>
                      <T text="Model provider" /><span className="text-danger"> *</span>
                    </Form.Label>
                    <Form.Select value={provider} onChange={(e) => setProvider(e.target.value)}>
                      <option value="">—</option>
                      {/* A provider whose module was uninstalled under a saved
                          model is still shown, or saving this form would
                          silently repoint the model at another one. */}
                      {provider !== "" && providers.every((p) => p.name !== provider) && (
                        <option value={provider}>
                          {t("{name} (not on this server)", { name: provider })}
                        </option>
                      )}
                      {providers.map((p) => (
                        <option key={p.name} value={p.name}>
                          {p.name}
                          {p.module ? ` (${p.module})` : ""}
                          {p.unavailable ? ` — ${t("unavailable here")}` : ""}
                        </option>
                      ))}
                    </Form.Select>
                    {chosen && <Form.Text muted>{chosen.description}</Form.Text>}
                    {/* Listed rather than hidden (Stan TODO §4): the model can
                        still be written and saved, and the reason is what the
                        admin needs to fix the server. */}
                    {chosen?.unavailable && (
                      <Alert variant="warning" className="mt-2 mb-0 py-2 small">
                        {chosen.unavailable}
                      </Alert>
                    )}
                  </Form.Group>
                </Col>
                <Col md={7}>
                  <Form.Label><T text="Outcome" /></Form.Label>
                  <div className="mb-3">
                    {outcome ? (
                      <StatusBadge tone="blue">{outcomeSummary(outcome)}</StatusBadge>
                    ) : (
                      <span className="text-muted">
                        {chosen?.outcome_error ??
                          (resolved
                            ? t("Pick a provider.")
                            : t("Pick a dataset that reads, and this says what a fit would produce."))}
                      </span>
                    )}
                  </div>
                  {chosen?.standardise && (
                    <Form.Text muted className="d-block">
                      <T text="This provider is handed standardised features, so its parameters are in standard deviations rather than the data's own units." />
                    </Form.Text>
                  )}
                </Col>
              </Row>

              {chosen && !bindsData && settingsSpec.length > 0 && (
                <>
                  <hr />
                  <h4 className="h5"><T text="Settings" /></h4>
                  {!resolved && !bindsData && (
                    <p className="text-muted small">
                      <T text="The dataset could not be read, so a setting that would offer this dataset's columns is a text box here." />
                    </p>
                  )}
                  <SettingsFields
                    spec={settingsSpec}
                    values={config}
                    onChange={(key, value) => setConfig((c) => ({ ...c, [key]: value }))}
                    idPrefix="model-config"
                  />
                </>
              )}

              {chosen && !bindsData && chosen.hyperparameters.length > 0 && (
                <>
                  <hr />
                  <h4 className="h5"><T text="Hyperparameters" /></h4>
                  <p className="text-muted small">
                    <T text="A value, or several separated by commas — a fit runs every combination of the lists, scores each on the validation rows and reports the winner. A blank box leaves the provider's own default." />
                  </p>
                  <Row>
                    {chosen.hyperparameters.map((field) => (
                      <Col md={4} key={field.name}>
                        <Form.Group className="mb-3" controlId={`model-hyper-${field.name}`}>
                          <Form.Label>{field.label}</Form.Label>
                          <Form.Control
                            className="font-monospace"
                            value={hyper[field.name] ?? ""}
                            placeholder={printGridValue(field.default) || t("the default")}
                            onChange={(e) =>
                              setHyper((h) => ({ ...h, [field.name]: e.target.value }))
                            }
                          />
                        </Form.Group>
                      </Col>
                    ))}
                  </Row>
                  {points > 1 && (
                    <p className="text-muted small mb-0">
                      {points > MAX_GRID_POINTS
                        ? t(
                            "{count} combinations, and each one is a fit — more than the {cap} a fit will run.",
                            { count: points, cap: MAX_GRID_POINTS },
                          )
                        : t("{count} combinations, and each one is a fit.", {
                            count: points,
                          })}{" "}
                      {!validationRows &&
                        t(
                          "A search scores its points on the validation rows, and this split has none — give it some below.",
                        )}
                    </p>
                  )}
                  {points === 0 && (
                    <p className="text-danger small mb-0">
                      <T text="One of these is an empty list, which is a search over nothing." />
                    </p>
                  )}
                </>
              )}
            </Card.Body>
          </Card>

          {/* --- the program and its data (a binding provider) --------------- */}
          {bindsData && chosen && (
            <BindingSection
              spec={chosen.config_spec}
              config={config}
              setConfig={(key, value) => setConfig((c) => ({ ...c, [key]: value }))}
              state={binding}
              setState={setBinding}
              iface={iface}
              setIface={setIface}
              main={mainDataset}
              datasets={datasets}
              modelBody={body}
              modelId={id}
            />
          )}
          {/* The sampler's settings come after the program and its data, which
              are what the admin is answering first. */}
          {bindsData && chosen && settingsSpec.length > 0 && (
            <Card className="mb-3">
              <Card.Header><T text="Settings" /></Card.Header>
              <Card.Body>
                <SettingsFields
                  spec={settingsSpec}
                  values={config}
                  onChange={(key, value) => setConfig((c) => ({ ...c, [key]: value }))}
                  idPrefix="model-config"
                />
              </Card.Body>
            </Card>
          )}

          {/* --- the split --------------------------------------------------- */}
          {!bindsData && (
          <Card className="mb-3">
            <Card.Header><T text="Split" /></Card.Header>
            <Card.Body>
              <Row>
                {(["train", "validation", "test"] as const).map((part) => (
                  <Col md={3} key={part}>
                    <Form.Group className="mb-3" controlId={`model-split-${part}`}>
                      <Form.Label>{splitName(part, t)}</Form.Label>
                      <Form.Control
                        type="number"
                        step="0.05"
                        min="0"
                        max="1"
                        value={split[part]}
                        onChange={(e) => setSplit((s) => ({ ...s, [part]: e.target.value }))}
                      />
                    </Form.Group>
                  </Col>
                ))}
                <Col md={3}>
                  <Form.Group className="mb-3" controlId="model-split-seed">
                    <Form.Label><T text="Seed" /></Form.Label>
                    <Form.Control
                      type="number"
                      value={split.seed}
                      onChange={(e) => setSplit((s) => ({ ...s, seed: e.target.value }))}
                    />
                  </Form.Group>
                </Col>
              </Row>
              {!splitSums(split) && (
                <p className="text-danger small mb-2">
                  <T text="The three fractions must sum to 1." />
                </p>
              )}
              <p className="text-muted small mb-0">
                <T text="Which side of the split a row falls on is a hash of its primary key and the seed, not a shuffle — so new rows arriving keep every old row where it was, and the test metric of this fit is comparable with the test metric of the last one. The fractions are therefore approximate; each fit records the counts it got." />
              </p>
            </Card.Body>
          </Card>
          )}

          {programCheck && (
            <Alert variant="secondary">
              <pre className="text-pre-wrap small mb-0">{programCheck}</pre>
            </Alert>
          )}

          <div className="btn-list mb-4">
            <Button type="submit" disabled={busy}>
              {busy ? <T text="Saving…" /> : id ? <T text="Save changes" /> : <T text="Create model" />}
            </Button>
            <Button variant="success" disabled={busy || running !== null} onClick={() => void fit()}>
              <T text="Fit" />
            </Button>
            <span className="text-muted small align-self-center">
              <T text="Fitting saves this model first, so what is fitted is what is on the screen." />
            </span>
          </div>
        </Form>

      {/* --- the fit ------------------------------------------------------------ */}
      {running && <FitRunning instance={running} live={live} />}
      {model && shown && (
        <section className="mb-4" aria-label={t("The fit")}>
          <FitView
            key={shown}
            model={model}
            instanceId={shown}
            collapsed={view.collapsed}
            plots={view.plots}
            onToggle={(output) => changeView({ collapsed: toggled(view.collapsed, output) })}
            onPlots={(plots) => changeView({ plots })}
            onChanged={() => id && void loadInstances(id)}
          />
        </section>
      )}

      {/* --- the fits ----------------------------------------------------------- */}
      {id && (
        <Card className="mb-3">
          <Card.Header>
            <T text="Fits" />
          </Card.Header>
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th>
                  <T text="Fit" />
                </th>
                <th>
                  <T text="Status" />
                </th>
                <th>
                  <T text="Result" />
                </th>
                <th>
                  <T text="Hyperparameters" />
                </th>
                <th className="text-end">
                  <T text="Actions" />
                </th>
              </tr>
            </thead>
            <tbody>
              {instances.length === 0 && (
                <tr>
                  <td colSpan={5} className="text-muted">
                    <T text="Not fitted yet. A fit reads every row of the dataset and runs on the server; this list says how it went." />
                  </td>
                </tr>
              )}
              {instances.map((instance) => (
                <tr key={instance.id} className={instance.id === shown ? "table-active" : undefined}>
                  <td>
                    <Button variant="link" className="p-0" onClick={() => select(instance.id)}>
                      {instanceLabel(instance)}
                    </Button>
                    {instance.name.trim() !== "" && (
                      <div className="text-muted small">{formatTimestamp(instance.created)}</div>
                    )}
                  </td>
                  <td>
                    <div className="d-flex align-items-center gap-2">
                      <StatusBadge tone={fitTone(instance.status)}>{instance.status}</StatusBadge>
                      {instance.active && (
                        <StatusBadge tone="green">
                          <T text="active" />
                        </StatusBadge>
                      )}
                      {instance.dataset_changed && (
                        <StatusBadge tone="yellow" title={t("The dataset has changed since this fit.")}>
                          <T text="dataset changed" />
                        </StatusBadge>
                      )}
                    </div>
                    {instance.error && <div className="text-danger small">{instance.error}</div>}
                    {instance.warnings.length > 0 && (
                      <div className="text-warning small">{t("{count} warnings", { count: instance.warnings.length })}</div>
                    )}
                  </td>
                  <td>{headlineMetric(readMetrics(instance.metrics)) ?? outcomeSummary(readOutcome(instance.outcome))}</td>
                  <td className="text-muted small font-monospace">{hyperparameterText(instance.hyperparameters)}</td>
                  <td className="text-end">
                    <div className="btn-list justify-content-end flex-nowrap">
                      {instance.status === "fitted" && !instance.active && (
                        <Button size="sm" variant="outline-primary" onClick={() => void activate(instance)}>
                          <T text="Activate" />
                        </Button>
                      )}
                      <Button size="sm" variant="outline-danger" onClick={() => void removeInstance(instance)}>
                        <T text="Remove" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
          <Card.Footer className="text-muted small">
            <T text='The active fit is the one predict("…") and models.get("…") answer with. A fit still running when the server stops is failed at boot.' />
          </Card.Footer>
        </Card>
      )}
    </div>
  );
}

/** The chosen provider's settings declaration, or none while nothing is chosen. */
function specOf(providers: ProviderItem[], provider: string) {
  return providers.find((p) => p.name === provider)?.config_spec ?? [];
}

/** Whether the three fractions sum to 1, which the server requires — a split
 * that silently normalised them would hold out a different fraction than the
 * instance says it did. */
function splitSums(split: SplitForm): boolean {
  const sum = Number(split.train) + Number(split.validation) + Number(split.test);
  return Number.isFinite(sum) && Math.abs(sum - 1) < 1e-9;
}

/**
 * The configuration `saveModel` takes: the settings as the spec types them,
 * and for a provider that binds data, the binding half's own keys built from
 * their editors rather than from the settings' text — each draft parsed, the
 * rows with no kind left for the server to name as unbound.
 */
function configurationOf(
  spec: ProviderItem["config_spec"],
  values: Record<string, string>,
  binding: BindingState | null,
): Record<string, unknown> {
  if (!binding) return buildConfig(spec, values);
  const owned = [BINDINGS_KEY, DIMENSIONS_KEY, POLICIES_KEY];
  const config = buildConfig(
    spec.filter((field) => !owned.includes(field.name)),
    values,
  );
  const dimensions: Record<string, unknown> = {};
  for (const row of binding.dimensions) {
    const parsed = parseDraft(DIMENSION_KINDS, row.draft).value;
    if (row.name.trim() !== "" && parsed) dimensions[row.name.trim()] = parsed;
  }
  config[BINDINGS_KEY] = parseDrafts(BINDING_KINDS, binding.bindings);
  config[DIMENSIONS_KEY] = dimensions;
  config[POLICIES_KEY] = buildPolicies(binding.policies);
  return config;
}

/** What `saveModel` said about the program (Stan TODO §5): `stanc`'s warnings,
 * or the notice that it was not checked. */
function programCheckText(raw: unknown): string | null {
  if (!raw || typeof raw !== "object") return null;
  const check = raw as { warnings?: unknown; notice?: unknown };
  const parts = [check.notice, check.warnings].filter(
    (p): p is string => typeof p === "string" && p.trim() !== "",
  );
  return parts.length > 0 ? parts.join("\n\n") : null;
}

/** The hyperparameter point a fit used, on one line. */
function hyperparameterText(raw: unknown): string {
  if (!raw || typeof raw !== "object") return "—";
  const entries = Object.entries(raw as Record<string, unknown>);
  if (entries.length === 0) return "—";
  return entries.map(([key, value]) => `${key}=${printGridValue(value)}`).join(" ");
}
