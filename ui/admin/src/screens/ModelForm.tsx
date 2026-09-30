// One model: its dataset, its provider, its hyperparameter space, its split —
// and the fits it has had (TODO "Predictive models", tasks 6.2, 6.3, 6.4).
//
// Three cards and one form, and the order is the order the questions come in:
// **which data**, then **which provider and with what settings**, then **how the
// rows are divided**. The dataset is first because everything else is about it —
// a provider's own form is built over the dataset's columns, and its label
// picker cannot offer `price` until `price` is a column of this dataset.
//
// The dataset is a **named dataset** (analytics TODO A1.11), picked from the
// ones the Analytics UI's Dataset editor builds, with a link to edit it there.
// The preview under the picker is the first rows and the types they came back
// as — the *data's* types, which is what the provider's form is built from and
// what no schema carries.
//
// For a provider that **binds data** (a posterior — Stan TODO §18) the form
// grows the parts a program needs, and none of them names the provider: the
// program's place, related datasets, dimensions and the binding table
// (`ModelBindings.tsx`). The split and the hyperparameter grid are hidden for
// it, because a posterior is not divided and not searched.
//
// **Fit saves first.** A fit of what is on the screen and a save of what is on
// the screen are the same intention, and the alternative is a button that
// silently fits the last saved version of a form the admin has been editing.

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
import { navigate } from "../App";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
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
  readModelDataset,
  readMetrics,
  readOutcome,
  readPolicies,
  readProgress,
  readRelated,
  readSplit,
  relatedBody,
  type Interface,
  type InstanceItem,
  type ProviderItem,
} from "../models";
import { SettingsFields, buildConfig, readConfig } from "../settings";
import { DatasetPicker, type DatasetItem } from "./DatasetPicker";
import { BindingSection, type BindingState } from "./ModelBindings";
import { fitTone } from "./Models";
import { T, useT } from "../i18n";

/** How long the form waits after a keystroke before asking the server which
 * providers this dataset resolves. */
const DEBOUNCE_MS = 600;

/** How often a `fitting` instance is re-read (§8: the row is the registry, so
 * the screen polls — there is nothing to await). */
const POLL_MS = 1500;

/** The split as the form edits it: four boxes of text, so a half-typed `0.` is
 * not a number this form has to have an opinion about. */
type SplitForm = { train: string; validation: string; test: string; seed: string };

export function ModelForm({ modelId }: { modelId?: string }) {
  const { t } = useT();
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);

  const [id, setId] = useState<string | null>(modelId ?? null);
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
  // The binding half, for a provider that binds data (Stan TODO §18).
  const [binding, setBinding] = useState<BindingState>({
    related: [],
    dimensions: [],
    policies: {},
    bindings: {},
  });
  const [iface, setIface] = useState<Interface | null>(null);
  const [programCheck, setProgramCheck] = useState<string | null>(null);

  // The dataset as the API takes it — a reference — and as the provider
  // lookup keys off. Stringified because that is what a query parameter
  // carries and what an effect can compare.
  const dataset = useMemo(() => ({ dataset_id: datasetId }), [datasetId]);
  const datasetJson = JSON.stringify(dataset);
  const mainDataset = datasets.find((d) => d.id === datasetId) ?? null;
  const bindsData = Boolean(providers.find((p) => p.name === provider)?.binds_data);
  const configJson = JSON.stringify(
    configurationOf(specOf(providers, provider), config, bindsData ? binding : null),
  );

  // --- loading ---------------------------------------------------------------

  useEffect(() => {
    let cancelled = false;
    const run = async () => {
      try {
        const datasetList = await api.listDatasets();
        let existing = null;
        if (modelId) existing = await api.getModel(modelId);
        if (cancelled) return;
        setDatasets(datasetList);
        if (existing) {
          const stored = readModelDataset(existing.dataset);
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
        } else {
          // A link from the Analytics UI's dataset list names the dataset.
          const asked = new URLSearchParams(window.location.hash.split("?")[1] ?? "").get("dataset");
          setDatasetId(asked ?? "");
        }
        setReady(true);
      } catch (err) {
        if (!cancelled) setLoadError(errorMessage(err, "Could not load this model."));
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [modelId]);

  // The providers, resolved against this dataset and this configuration where
  // they can be: `config_spec` then offers *these* columns and `outcome` says
  // what a fit would produce. A dataset that cannot be read falls back to the
  // unresolved declaration rather than an empty picker — the form still works,
  // and the preview beside it is where the reason is.
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
      setInstances(orderInstances(await api.listModelInstances(model)));
    } catch {
      // A model that has just been created has no instances and no endpoint
      // trouble worth a banner; the list simply stays empty.
    }
  }, []);

  useEffect(() => {
    if (id) void loadInstances(id);
  }, [id, loadInstances]);

  // The poll (§8). A fit is a spawned task and the row is the registry, so the
  // screen asks again until nothing says `fitting` — and stops, rather than
  // holding a timer open on a screen where nothing is happening.
  const fitting = instances.some((instance) => instance.status === "fitting");
  useEffect(() => {
    if (!id || !fitting) return undefined;
    const timer = window.setTimeout(() => void loadInstances(id), POLL_MS);
    return () => window.clearTimeout(timer);
  }, [id, fitting, instances, loadInstances]);

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
    // A posterior is not searched and not divided (Stan TODO §13): nothing of
    // the hidden controls is sent for one.
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

  /** Save, and answer the model's id — the same path the Fit button takes,
   * because fitting what is on the screen means saving it first. */
  const save = async (): Promise<string> => {
    const saved = await api.saveModel(body());
    setId(saved.id);
    setError(saved.error ?? null);
    setProgramCheck(programCheckText(saved.program_check));
    // A new model now has a URL of its own, so a reload comes back to it rather
    // than to an empty form.
    if (!modelId) window.location.hash = `/models/${encodeURIComponent(saved.id)}`;
    return saved.id;
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
      setError(errorMessage(err, "Could not save the model."));
    } finally {
      setBusy(false);
    }
  };

  const fit = async () => {
    setBusy(true);
    setError(null);
    try {
      const saved = await save();
      await api.fitModel(saved, { name: null, description: null });
      await loadInstances(saved);
    } catch (err) {
      setError(errorMessage(err, "Could not start the fit."));
    } finally {
      setBusy(false);
    }
  };

  const activate = async (instance: InstanceItem) => {
    try {
      await api.activateModelInstance(instance.id);
      if (id) await loadInstances(id);
    } catch (err) {
      setError(errorMessage(err, "Could not activate that fit."));
    }
  };

  const removeInstance = async (instance: InstanceItem) => {
    if (
      !window.confirm(
        t('Remove the fit "{name}"?', { name: instanceLabel(instance) }),
      )
    ) {
      return;
    }
    try {
      await api.deleteModelInstance(instance.id);
      if (id) await loadInstances(id);
    } catch (err) {
      setError(errorMessage(err, "Could not remove that fit."));
    }
  };

  if (loadError) {
    return (
      <PageBody>
        <Alert variant="danger">{loadError}</Alert>
      </PageBody>
    );
  }
  if (!ready) {
    return (
      <PageBody>
        <div className="py-5 text-center">
          <Spinner animation="border" role="status" />
        </div>
      </PageBody>
    );
  }

  return (
    <>
      <PageHeader
        pretitle="Models"
        title={modelId ? name || "Model" : "New model"}
        actions={
          <Button variant="outline-secondary" onClick={() => navigate("/models")}>
            <IconArrowLeft className="icon-2" />
            <T text="Back" />
          </Button>
        }
      />
      <PageBody>
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
                            ? "Pick a provider."
                            : "Build a dataset that reads, and this says what a fit would produce.")}
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
                            placeholder={printGridValue(field.default) || "the default"}
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
                      <Form.Label className="text-capitalize">{part}</Form.Label>
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
              {busy ? "Saving…" : id ? "Save changes" : "Create model"}
            </Button>
            <Button variant="success" disabled={busy} onClick={() => void fit()}>
              <T text="Fit" />
            </Button>
            <span className="text-muted small align-self-center">
              <T text="Fitting saves this model first, so what is fitted is what is on the screen." />
            </span>
          </div>
        </Form>

        {/* --- the fits ----------------------------------------------------- */}
        {id && (
          <Card className="mb-3">
            <Card.Header><T text="Fits" /></Card.Header>
            <Table hover responsive className="card-table table-vcenter">
              <thead>
                <tr>
                  <th><T text="Fit" /></th>
                  <th><T text="Status" /></th>
                  <th><T text="Result" /></th>
                  <th><T text="Hyperparameters" /></th>
                  <th className="text-end"><T text="Actions" /></th>
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
                  <tr key={instance.id}>
                    <td>
                      <a href={`#/model-instances/${encodeURIComponent(instance.id)}`}>
                        {instanceLabel(instance)}
                      </a>
                      {/* An unnamed fit is already addressed by when it
                          happened, so the time is not printed under itself. */}
                      {instance.name.trim() !== "" && (
                        <div className="text-muted small">
                          {formatTimestamp(instance.created)}
                        </div>
                      )}
                    </td>
                    <td>
                      <div className="d-flex align-items-center gap-2">
                        <StatusBadge tone={fitTone(instance.status)}>
                          {instance.status}
                        </StatusBadge>
                        {instance.active && <StatusBadge tone="green"><T text="active" /></StatusBadge>}
                      </div>
                      {/* A fit that failed says so **here**, because the request
                          that started it returned long before it failed. */}
                      {instance.error && (
                        <div className="text-danger small">{instance.error}</div>
                      )}
                      {instance.status === "fitting" && readProgress(instance.progress) && (
                        <div className="text-muted small">
                          {stageText(t, readProgress(instance.progress)?.stage)}
                        </div>
                      )}
                      {instance.warnings.length > 0 && (
                        <div className="text-warning small">
                          {t("{count} warnings", { count: instance.warnings.length })}
                        </div>
                      )}
                    </td>
                    <td>
                      {headlineMetric(readMetrics(instance.metrics)) ??
                        outcomeSummary(readOutcome(instance.outcome))}
                    </td>
                    <td className="text-muted small font-monospace">
                      {hyperparameterText(instance.hyperparameters)}
                    </td>
                    <td className="text-end">
                      <div className="btn-list justify-content-end flex-nowrap">
                        {instance.status === "fitted" && !instance.active && (
                          <Button
                            size="sm"
                            variant="outline-primary"
                            onClick={() => void activate(instance)}
                          >
                            <T text="Activate" />
                          </Button>
                        )}
                        <Button
                          size="sm"
                          variant="outline-secondary"
                          href={`#/model-instances/${encodeURIComponent(instance.id)}`}
                        >
                          <T text="Open" />
                        </Button>
                        <Button
                          size="sm"
                          variant="outline-danger"
                          onClick={() => void removeInstance(instance)}
                        >
                          <T text="Remove" />
                        </Button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </Table>
            <Card.Footer className="text-muted small">
              {chosen?.cancellable ? (
                <T text="A fit runs on the server and this list polls until it finishes; its screen can cancel it. Nothing survives a restart: an instance still fitting when the server stops is failed at boot." />
              ) : (
                <T text="A fit runs on the server and this list polls until it finishes. Nothing survives a restart: an instance still fitting when the server stops is failed at boot, because there is no cancel and no way to pick it back up." />
              )}
            </Card.Footer>
          </Card>
        )}
      </PageBody>
    </>
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

/** A running fit's stage, as a word. */
export function stageText(
  t: (text: string) => string,
  stage: string | undefined,
): string {
  switch (stage) {
    case "queued":
      return t("queued for a process");
    case "compiling":
      return t("compiling");
    case "sampling":
      return t("sampling");
    case "summarising":
      return t("summarising");
    default:
      return "";
  }
}

/** The hyperparameter point a fit used, on one line. */
function hyperparameterText(raw: unknown): string {
  if (!raw || typeof raw !== "object") return "—";
  const entries = Object.entries(raw as Record<string, unknown>);
  if (entries.length === 0) return "—";
  return entries.map(([key, value]) => `${key}=${printGridValue(value)}`).join(" ");
}
