// The model editor (analytics TODO A3.5–A3.6; design A1 "Workbench"), at
// `#/models/<id>` and `#/models/new`: one model — its dataset, its provider,
// its settings and hyperparameters, its split — beside the model's fits.
//
// **The workbench.** A bar along the top holds the model's name, what a fit of
// it produces, how it is called (`predict("…")`), whether what is on the
// screen is saved, and Fit. Under it, a banner says when the dataset has
// changed since the active fit. Then two columns: **Setup** on the left, the
// questions in the order they come in — which data, which algorithm, which
// features, what to tune, how the rows are divided — each a numbered section;
// and the **Fits** on the right, each a card (`FitCard.tsx`) that keeps the
// fit table's columns and opens in place on what the fit came to. Its full
// report is still the modal (`FitReportModal.tsx`), opened from the card.
//
// For a provider that **binds data** (a posterior — Stan TODO §18) the setup
// has no Features, Tuning or Split, because a posterior is not divided and
// not searched: a formula model's sections are its Formula, its Priors and
// its Sampler, with the data binding and the generated program folded under
// them; a hand-written Stan model's are its program and data (the binding
// table, `ModelBindings.tsx`) and its Sampler. Its fits' Result column is a
// trust verdict built from the sampler's diagnostics, and an open card shows
// the checks against their thresholds.
//
// **Narrow.** The editor sizes itself by its own width (a container query),
// not the window's, because a split view gives it half of one: below the
// breakpoint the columns become two tabs, Setup and Fits, each section of the
// setup folds to a one-line summary, and a card's hyperparameters and report
// drop under its name.
//
// **Saving.** An existing model is saved as it is edited, a moment after the
// last change, and the bar says so ("All changes saved"); a new one is
// created by "Create model" or by its first Fit. **Fit saves first**: a fit of
// what is on the screen and a save of what is on the screen are the same
// intention. While a fit runs its progress is pushed by the server
// (`progress.ts`) into its card; when it finishes its card opens.
//
// A model is not a workspace: it is a global, named entity that formulas,
// actions and panels refer to, as a dataset is. What a workspace would have
// given — reopening as it was left — is the model's **view state**
// (`viewState.ts`): the optional plots chosen for the reports and the fit
// open, written key by key as they change and read back on open. None of it
// touches the model.

import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import { useAnnounce, useChanges, usePane } from "../panes";
import { routeHash } from "../router";
import { useShell } from "../shell";
import { DatasetPicker, datasetOptionLabel, type DatasetItem } from "./DatasetPicker";
import { FitCard, FitHeader, type FitCardContext } from "./FitCard";
import { fitNumbers, splitBar, tuningChanges, withValidation, type SplitForm } from "./fitCard";
import { FitReportModal, type ReportTab } from "./FitReportModal";
import { FormulaSection, type FormulaFrame } from "./FormulaEditor";
import { FORMULA_KEYS, FORMULA_PROVIDER, readRows, writeRows, type FormulaCheck } from "./formula";
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
  featureChoices,
  featuresBody,
  gridPoints,
  instanceLabel,
  orderInstances,
  outcomeLabel,
  outcomeSummary,
  parseDraft,
  parseDrafts,
  printDrafts,
  printGridValue,
  offered,
  readColumns,
  readFeatures,
  readHyperparameters,
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
import { StatusBadge } from "./StatusBadge";
import { chosenFit, editorPatch, readEditorView, type EditorView } from "./viewState";

/** How long the form waits after a keystroke before asking the server which
 * providers this dataset resolves. */
const DEBOUNCE_MS = 600;

/** How long an existing model waits after the last edit before it is saved. */
const AUTOSAVE_MS = 1200;

/** How many fits are listed before "Show older fits". */
const SHOWN_FITS = 6;

/** How many sampler settings are shown before "Advanced". */
const SAMPLER_SHOWN = 4;

/** A split's part, as its box is labelled. */
function splitName(part: "train" | "validation" | "test", t: (s: string) => string): string {
  return part === "train" ? t("Train") : part === "validation" ? t("Validation") : t("Test");
}

/** Whether the model on the screen is saved. */
type SaveState = "saved" | "dirty" | "saving" | "error";

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
  const { application } = useShell();
  const mayCreate = !application || application.createWorkspaces;
  // Bumped when the model is changed on the other side of a split view, to
  // read it again.
  const [reloads, setReloads] = useState(0);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);

  const [id, setId] = useState<string | null>(modelId ?? null);
  const [model, setModel] = useState<ModelItem | null>(null);
  const [name, setNameRaw] = useState("");
  const [description, setDescriptionRaw] = useState("");
  const [datasetId, setDatasetIdRaw] = useState("");
  const [provider, setProviderRaw] = useState("");
  const [config, setConfigRaw] = useState<Record<string, string>>({});
  const [hyper, setHyperRaw] = useState<Record<string, string>>({});
  // The columns a fit encodes, or null for every column but the label.
  const [features, setFeaturesRaw] = useState<string[] | null>(null);
  const [split, setSplitRaw] = useState<SplitForm>({
    train: String(DEFAULT_SPLIT.train),
    validation: String(DEFAULT_SPLIT.validation),
    test: String(DEFAULT_SPLIT.test),
    seed: "0",
  });

  const [datasets, setDatasets] = useState<DatasetItem[]>([]);
  const [providers, setProviders] = useState<ProviderItem[]>([]);
  const [resolved, setResolved] = useState(false);
  const [instances, setInstances] = useState<InstanceItem[]>([]);
  const [binding, setBindingRaw] = useState<BindingState>({
    related: [],
    dimensions: [],
    policies: {},
    bindings: {},
  });
  const [iface, setIface] = useState<Interface | null>(null);
  const [programCheck, setProgramCheck] = useState<string | null>(null);
  const [formulaCheck, setFormulaCheck] = useState<FormulaCheck | null>(null);

  // The view state's part that is the editor's.
  const [view, setView] = useState<EditorView>({ collapsed: [], plots: [], fit: null });
  // The fit cards open.
  const [openFits, setOpenFits] = useState<string[]>([]);
  // The fit running now, followed over its progress socket.
  const [running, setRunning] = useState<string | null>(null);
  // The fit whose full report is open, and on which tab.
  const [reporting, setReporting] = useState<{ id: string; tab: ReportTab } | null>(null);
  const [showOlder, setShowOlder] = useState(false);
  const [showDescription, setShowDescription] = useState(false);
  const [showFeatures, setShowFeatures] = useState(false);
  // Narrow: which tab is shown, and which setup sections are unfolded.
  const [tab, setTab] = useState<"setup" | "fits">(modelId ? "fits" : "setup");
  const [unfolded, setUnfolded] = useState<string[]>([]);

  // --- edits, and saving them ------------------------------------------------

  // Every edit the admin makes bumps `edits`; the autosave writes the form and
  // records the generation it wrote. Loading the model is not an edit.
  const [edits, setEdits] = useState(0);
  const [savedEdits, setSavedEdits] = useState(0);
  const [saveState, setSaveState] = useState<SaveState>("saved");
  const [saveError, setSaveError] = useState<string | null>(null);
  const touch = () => setEdits((n) => n + 1);
  const setName = (v: string) => (setNameRaw(v), touch());
  const setDescription = (v: string) => (setDescriptionRaw(v), touch());
  const setDatasetId = (v: string) => (setDatasetIdRaw(v), touch());
  const setProvider = (v: string) => (setProviderRaw(v), touch());
  const setConfigKey = (key: string, value: string) => (setConfigRaw((c) => ({ ...c, [key]: value })), touch());
  const setHyperKey = (key: string, value: string) => (setHyperRaw((h) => ({ ...h, [key]: value })), touch());
  const setFeatures = (u: string[] | null | ((c: string[] | null) => string[] | null)) => (setFeaturesRaw(u), touch());
  const setSplit = (u: SplitForm | ((s: SplitForm) => SplitForm)) => (setSplitRaw(u), touch());
  const setBinding = (u: BindingState | ((b: BindingState) => BindingState)) => (setBindingRaw(u), touch());

  const dataset = useMemo(() => ({ dataset_id: datasetId }), [datasetId]);
  const datasetJson = JSON.stringify(dataset);
  const mainDataset = datasets.find((d) => d.id === datasetId) ?? null;
  const bindsData = Boolean(providers.find((p) => p.name === provider)?.binds_data);
  // A formula provider binds data too, but derives its bindings from the
  // formula (TODO-bayesian-regression §7): it has no binding table.
  const formulaProvider = provider === FORMULA_PROVIDER;
  const bindingTable = bindsData && !formulaProvider;
  const configJson = JSON.stringify(
    configurationOf(specOf(providers, provider), config, bindingTable ? binding : null),
  );

  // --- loading ---------------------------------------------------------------

  const fill = useCallback((existing: ModelItem) => {
    const stored = readModelDataset(existing.dataset);
    setModel(existing);
    setNameRaw(existing.name);
    setDescriptionRaw(existing.description);
    setDatasetIdRaw(stored?.dataset_id ?? "");
    setProviderRaw(existing.provider);
    setConfigRaw(readConfig(existing.configuration));
    const configuration = (existing.configuration ?? {}) as Record<string, unknown>;
    setBindingRaw({
      related: readRelated(existing.related),
      dimensions: Object.entries(printDrafts(DIMENSION_KINDS, configuration[DIMENSIONS_KEY])).map(
        ([dimension, draft]) => ({ name: dimension, draft }),
      ),
      policies: readPolicies(configuration[POLICIES_KEY]),
      bindings: printDrafts(BINDING_KINDS, configuration[BINDINGS_KEY]),
    });
    setHyperRaw(readHyperparameters(existing.hyperparameters));
    setFeaturesRaw(readFeatures(existing.features));
    const storedSplit = readSplit(existing.split);
    setSplitRaw({
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
          // The card open: the fit the address names, else the one the view
          // state remembers, else the active fit, else the newest fitted.
          const open =
            chosenFit(routeFit, remembered.fit, ordered) ??
            ordered.find((f) => f.active)?.id ??
            ordered.find((f) => f.status === "fitted")?.id ??
            null;
          setOpenFits(open ? [open] : []);
          if (ordered.length === 0) setTab("setup");
          setRunning(ordered.find((f) => f.status === "fitting")?.id ?? null);
        } else {
          setDatasetIdRaw(askedDataset ?? "");
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

  /** Open or close a fit's card; the one opened last is remembered. */
  const toggleFit = (fit: string) => {
    const opening = !openFits.includes(fit);
    setOpenFits((open) => (opening ? [...open, fit] : open.filter((f) => f !== fit)));
    if (opening) changeView({ fit });
  };

  const openFit = (fit: string) => {
    setOpenFits((open) => (open.includes(fit) ? open : [...open, fit]));
    changeView({ fit });
  };

  // The fit that finished has its card opened, and the model is read again
  // for its last fit.
  const live = useFitProgress(running, () => {
    const finished = running;
    setRunning(null);
    if (!id) return;
    changed("model", id);
    void loadInstances(id);
    void api.getModel(id).then(setModel).catch(() => undefined);
    if (finished) openFit(finished);
  });

  // --- what the form knows ---------------------------------------------------

  const chosen = providers.find((p) => p.name === provider);
  const outcome = readOutcome(chosen?.outcome);
  const hyperSpace = buildHyperparameters(chosen?.hyperparameters ?? [], hyper);
  const points = gridPoints(hyperSpace);
  const validationRows = Number(split.validation) > 0;
  const takesFeatures = Boolean(chosen?.takes_features);
  const label = outcomeLabel(outcome);
  const datasetColumns = readColumns(mainDataset?.columns);
  const choices = featureChoices(datasetColumns, label);
  /** Tick or untick one feature, keeping the dataset's order. */
  const toggleFeature = (column: string, on: boolean) =>
    setFeatures((current) => {
      const next = new Set(current ?? []);
      if (on) next.add(column);
      else next.delete(column);
      return choices.map((c) => c.name).filter((name) => next.has(name));
    });
  // A binding provider's own controls edit these settings; the rest (the
  // sampler's, the runs store) are rendered as settings like any provider's.
  const settingsSpec = (chosen?.config_spec ?? []).filter(
    (field) =>
      (!bindsData || !BINDING_FORM_KEYS.includes(field.name)) &&
      (!formulaProvider || !FORMULA_KEYS.includes(field.name)),
  );

  // The fits' numbers, and the fit the setup is compared with: the active
  // one, else the newest that fitted.
  const numbers = useMemo(() => fitNumbers(instances), [instances]);
  const activeFit = instances.find((f) => f.active) ?? null;
  const reference = activeFit ?? instances.find((f) => f.status === "fitted") ?? null;
  const tuningWas = reference && !bindsData ? tuningChanges(hyper, reference.hyperparameters) : {};
  const tuningChanged = Object.keys(tuningWas).length > 0;

  // --- saving and fitting ----------------------------------------------------

  /** Everything on this form as `saveModel` takes it. */
  const body = () => ({
    id,
    name: name.trim(),
    description: description.trim(),
    provider,
    dataset,
    related: bindsData ? relatedBody(binding.related) : [],
    configuration: configurationOf(specOf(providers, provider), config, bindingTable ? binding : null),
    hyperparameters: bindsData ? {} : hyperSpace,
    split: bindsData
      ? DEFAULT_SPLIT
      : {
          train: Number(split.train),
          validation: Number(split.validation),
          test: Number(split.test),
          seed: Math.trunc(Number(split.seed)) || 0,
        },
    features: featuresBody(features, label, takesFeatures, datasetColumns),
    attributes: {},
  });

  /** Save, and answer the model's id — the same path Fit takes. `over`
   * replaces parts of the form's body, for a change made in the same breath.
   * `movePasted` moves prior calls pasted into a formula into the prior
   * table first, as committing the field does (§3.7) — not while the admin
   * is still typing, which is when the autosave runs. */
  const save = async (
    over: Partial<ReturnType<typeof body>> = {},
    { movePasted = true }: { movePasted?: boolean } = {},
  ): Promise<string> => {
    const generation = edits;
    let current = { ...body(), ...over };
    if (formulaProvider && movePasted) {
      const answer = (await api.checkFormula(current as never)) as unknown as FormulaCheck;
      if (answer.pasted > 0) {
        setConfigRaw((c) => ({ ...c, formula: answer.formula, priors: writeRows(answer.priors) }));
        current = {
          ...current,
          configuration: { ...current.configuration, formula: answer.formula, priors: answer.priors },
        };
      }
    }
    setSaveState("saving");
    try {
      const saved = await api.saveModel(current);
      setId(saved.id);
      setModel(saved);
      setError(saved.error ?? null);
      setProgramCheck(programCheckText(saved.program_check));
      setSavedEdits((g) => Math.max(g, generation));
      setSaveError(null);
      setSaveState("saved");
      changed("model", saved.id);
      // A new model now has an address of its own, so a reload comes back to
      // it. Replaced rather than navigated to, so the form is not built again
      // under the fit that is about to start.
      if (!modelId && !id) pane.replace({ name: "model", id: saved.id });
      return saved.id;
    } catch (err) {
      setSaveState("error");
      setSaveError(errorMessage(err, t("Could not save the model.")));
      throw err;
    }
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

  // The autosave: an existing model, a moment after the last edit. A refusal
  // is shown in the bar and tried again at the next edit.
  const latestSave = useRef(save);
  latestSave.current = save;
  useEffect(() => {
    if (!id || edits === savedEdits) return undefined;
    setSaveState((s) => (s === "saving" ? s : "dirty"));
    const timer = window.setTimeout(() => {
      void latestSave.current({}, { movePasted: false }).catch(() => undefined);
    }, AUTOSAVE_MS);
    return () => window.clearTimeout(timer);
  }, [id, edits, savedEdits]);

  /** "Create model": a new model saved for the first time. */
  const create = async () => {
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
      setTab("fits");
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

  const cancel = async (instance: InstanceItem) => {
    try {
      await api.cancelModelFit(instance.id);
      if (id) await loadInstances(id);
    } catch (err) {
      setError(errorMessage(err, t("Could not cancel the fit.")));
    }
  };

  const removeInstance = async (instance: InstanceItem) => {
    const question = instance.report
      ? t('Remove the fit "{name}"? Its report "{report}" stays, but its plots and tables will say the fit is gone.', {
          name: instanceLabel(instance),
          report: instance.report.name,
        })
      : t('Remove the fit "{name}"?', { name: instanceLabel(instance) });
    if (!window.confirm(question)) return;
    try {
      await api.deleteModelInstance(instance.id);
      setOpenFits((open) => open.filter((f) => f !== instance.id));
      if (view.fit === instance.id) changeView({ fit: null });
      if (reporting?.id === instance.id) setReporting(null);
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

  // --- what stops a fit ------------------------------------------------------

  const unmatched = formulaProvider ? (formulaCheck?.table?.unmatched.length ?? 0) : 0;
  const blocker =
    name.trim() === ""
      ? t("Name the model first")
      : datasetId === "" || provider === ""
        ? t("Choose a dataset and a provider first")
        : unmatched > 0
          ? unmatched === 1
            ? t("Resolve 1 unmatched prior first")
            : t("Resolve {count} unmatched priors first", { count: unmatched })
          : !bindsData && !splitSums(split)
            ? t("The split must sum to 1")
            : !bindsData && points === 0
              ? t("A tuning list is empty")
              : null;

  // --- the setup column ------------------------------------------------------

  const section = (key: string) => ({
    folded: !unfolded.includes(key),
    onFold: () => setUnfolded((u) => (u.includes(key) ? u.filter((k) => k !== key) : [...u, key])),
  });
  let step = 0;
  const next = () => ++step;
  const providerName = chosen?.name ?? provider;

  const dataSection = (
    <SetupSection
      n={next()}
      title={<T text="Data" />}
      summary={mainDataset ? datasetOptionLabel(mainDataset) : t("No dataset chosen")}
      {...section("data")}
    >
      <DatasetPicker
        compact
        value={datasetId}
        onChange={setDatasetId}
        datasets={datasets}
        idPrefix="model-main"
        back={id ? routeHash({ name: "model", id }) : undefined}
        onCopied={async (copy) => {
          setDatasets((list) => [...list.filter((d) => d.id !== copy.id), copy]);
          setDatasetIdRaw(copy.id);
          // Saved at once: the copy is this model's from now on, and the next
          // thing done with it is usually to edit it.
          if (id) await saveQuietly({ dataset: { dataset_id: copy.id } });
          else touch();
        }}
        beforeEdit={id ? () => saveQuietly({}) : undefined}
        previewNote={() => (
          <T text="The first rows, and the type each column’s values came back as — which is what the provider’s form is built from." />
        )}
      />
    </SetupSection>
  );

  const algorithmSection = (
    <SetupSection
      n={next()}
      title={<T text="Algorithm" />}
      summary={[providerName || t("No provider chosen"), label && t("label {label}", { label })]
        .filter(Boolean)
        .join(" · ")}
      {...section("algorithm")}
    >
      <Form.Group className="mb-2" controlId="modelProvider">
        <Form.Label>
          <T text="Provider" />
        </Form.Label>
        <Form.Select value={provider} onChange={(e) => setProvider(e.target.value)}>
          <option value="">—</option>
          {/* A provider whose module was uninstalled under a saved model is
              still shown, or saving this form would silently repoint the
              model at another one. */}
          {provider !== "" && providers.every((p) => p.name !== provider) && (
            <option value={provider}>{t("{name} (not on this server)", { name: provider })}</option>
          )}
          {/* An unlisted provider (Stan, for now) is offered only to the model
              that already uses it. */}
          {providers.filter((p) => offered(p, provider, model)).map((p) => (
            <option key={p.name} value={p.name}>
              {p.name}
              {p.module ? ` — ${p.module}` : ""}
              {p.unavailable ? ` — ${t("unavailable here")}` : ""}
            </option>
          ))}
        </Form.Select>
        {chosen && <Form.Text muted>{chosen.description}</Form.Text>}
        {/* Listed rather than hidden (Stan TODO §4): the model can still be
            written and saved, and the reason is what the admin needs to fix
            the server. */}
        {chosen?.unavailable && (
          <Alert variant="warning" className="mt-2 mb-0 py-2 small">
            {chosen.unavailable}
          </Alert>
        )}
      </Form.Group>
      {chosen && !bindsData && settingsSpec.length > 0 && (
        <>
          {!resolved && (
            <p className="text-muted small">
              <T text="The dataset could not be read, so a setting that would offer this dataset's columns is a text box here." />
            </p>
          )}
          <div className="an-wb-fields mt-3">
            <SettingsFields spec={settingsSpec} values={config} onChange={setConfigKey} idPrefix="model-config" />
          </div>
        </>
      )}
      <div className="small an-wb-outcome">
        {outcome ? (
          <>
            → <T text="Outcome:" /> <strong>{outcomeSummary(outcome)}</strong>
          </>
        ) : (
          <span className="text-muted">
            {chosen?.outcome_error ??
              (resolved ? t("Pick a provider.") : t("Pick a dataset that reads, and this says what a fit would produce."))}
          </span>
        )}
      </div>
      {chosen?.standardise && (
        <Form.Text muted className="d-block">
          <T text="This provider is handed standardised features, so its parameters are in standard deviations rather than the data's own units." />
        </Form.Text>
      )}
    </SetupSection>
  );

  const featureCount = features === null ? choices.length : features.length;
  const featuresSection = chosen && takesFeatures && !bindsData && (
    <SetupSection
      n={next()}
      title={<T text="Features" />}
      summary={
        features === null
          ? t("All but the label · {count}", { count: choices.length })
          : t("{count} chosen", { count: features.length })
      }
      aside={
        <Button size="sm" variant="outline-secondary" onClick={() => setShowFeatures((v) => !v)}>
          {showFeatures ? <T text="Done" /> : <T text="Choose…" />}
        </Button>
      }
      {...section("features")}
    >
      <div className="small">
        <strong>{features === null ? t("All columns but the label") : t("The columns chosen")}</strong>
        <span className="text-secondary"> · {t("{count} columns", { count: featureCount })}</span>
      </div>
      {showFeatures && (
        <div className="mt-2">
          <Form.Check
            type="radio"
            id="model-features-all"
            name="model-features"
            label={t("Every column but the label")}
            checked={features === null}
            onChange={() => setFeatures(null)}
          />
          <Form.Check
            type="radio"
            id="model-features-chosen"
            name="model-features"
            label={t("Only the columns ticked below")}
            checked={features !== null}
            // Starts from every column, so choosing is unticking the few to
            // leave out.
            onChange={() => setFeatures(choices.map((c) => c.name))}
          />
          {features !== null && (
            <div className="an-wb-colchecks mt-2 ms-4">
              {choices.length === 0 && (
                <span className="text-muted small">
                  <T text="The dataset's columns are not known yet: pick a dataset that reads." />
                </span>
              )}
              {choices.map((column) => (
                <Form.Check
                  key={column.name}
                  type="checkbox"
                  id={`model-feature-${column.name}`}
                  label={
                    <>
                      {column.name}
                      {column.type && column.type !== "unknown" && (
                        <span className="text-muted small"> {column.type}</span>
                      )}
                    </>
                  }
                  checked={features.includes(column.name)}
                  onChange={(e) => toggleFeature(column.name, e.target.checked)}
                />
              ))}
            </div>
          )}
        </div>
      )}
      <p className="text-secondary small mt-2 mb-0">
        <T text="A row with a blank in a chosen column, or in the label, is left out of the fit." />
      </p>
    </SetupSection>
  );

  const tuningSection = chosen && !bindsData && chosen.hyperparameters.length > 0 && (
    <SetupSection
      n={next()}
      title={<T text="Tuning" />}
      changed={tuningChanged}
      tint={tuningChanged}
      summary={
        chosen.hyperparameters
          .map((f) => `${f.label.toLowerCase()} ${hyper[f.name]?.trim() || printGridValue(f.default) || t("default")}`)
          .join(" · ")
      }
      aside={
        points > 1 ? (
          <span className="badge bg-secondary-lt">
            {points > MAX_GRID_POINTS
              ? t("{count} combinations — over {cap}", { count: points, cap: MAX_GRID_POINTS })
              : t("{count} combinations", { count: points })}
          </span>
        ) : undefined
      }
      {...section("tuning")}
    >
      <div className="an-wb-fields">
        {chosen.hyperparameters.map((field) => (
          <Form.Group
            key={field.name}
            controlId={`model-hyper-${field.name}`}
            className={(hyper[field.name] ?? "").includes(",") || tuningWas[field.name] ? "an-wb-wide" : undefined}
          >
            <Form.Label>{field.label}</Form.Label>
            <Form.Control
              className="font-monospace"
              value={hyper[field.name] ?? ""}
              placeholder={printGridValue(field.default) || t("the default")}
              onChange={(e) => setHyperKey(field.name, e.target.value)}
            />
            {(hyper[field.name] ?? "").includes(",") || tuningWas[field.name] ? (
              <Form.Text muted>
                <T text="One value, or a list to try each." />
                {tuningWas[field.name] && reference && (
                  <>
                    {" "}
                    {t("Was {value} in fit #{fit}.", {
                      value: tuningWas[field.name],
                      fit: numbers.get(reference.id) ?? "",
                    })}
                  </>
                )}
              </Form.Text>
            ) : null}
          </Form.Group>
        ))}
      </div>
      {points > MAX_GRID_POINTS && (
        <p className="text-danger small mt-2 mb-0">
          {t("{count} combinations, and each one is a fit — more than the {cap} a fit will run.", {
            count: points,
            cap: MAX_GRID_POINTS,
          })}
        </p>
      )}
      {points === 0 && (
        <p className="text-danger small mt-2 mb-0">
          <T text="One of these is an empty list, which is a search over nothing." />
        </p>
      )}
      {points > 1 && !validationRows && (
        <div className="an-wb-note mt-3">
          <p className="mb-2">
            <T text="A list needs validation rows to pick the best value. None are set." />
          </p>
          <Button size="sm" variant="outline-secondary" onClick={() => setSplit((s) => withValidation(s))}>
            <T text="Use 10% for validation" />
          </Button>
        </div>
      )}
    </SetupSection>
  );

  const bar = splitBar(split);
  const splitSection = !bindsData && (
    <SetupSection
      n={next()}
      title={<T text="Split" />}
      summary={[...bar.map((b) => `${splitName(b.part, t).toLowerCase()} ${b.percent}%`), t("seed {seed}", { seed: split.seed })].join(
        " · ",
      )}
      {...section("split")}
    >
      <div className="an-wb-splitbar" aria-hidden="true">
        {bar.map((b) => (
          <span key={b.part} className={`is-${b.part}`} style={{ flexBasis: `${b.percent}%` }}>
            {splitName(b.part, t)} {b.percent}%
          </span>
        ))}
      </div>
      <div className="an-wb-split">
        {(["train", "validation", "test"] as const).map((part) => (
          <Form.Group key={part} controlId={`model-split-${part}`}>
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
        ))}
        <Form.Group controlId="model-split-seed">
          <Form.Label>
            <T text="Seed" />
          </Form.Label>
          <Form.Control type="number" value={split.seed} onChange={(e) => setSplit((s) => ({ ...s, seed: e.target.value }))} />
        </Form.Group>
      </div>
      {!splitSums(split) && (
        <p className="text-danger small mt-2 mb-0">
          <T text="The three fractions must sum to 1." />
        </p>
      )}
      <p className="text-secondary small mt-2 mb-1">
        <T text="Each row keeps its side between fits: it is set by the row's key and the seed." />
      </p>
      <details className="an-wb-details small">
        <summary>
          <T text="How?" />
        </summary>
        <T text="Which side of the split a row falls on is a hash of its primary key and the seed, not a shuffle — so new rows arriving keep every old row where it was, and the test metric of this fit is comparable with the test metric of the last one. The fractions are therefore approximate; each fit records the counts it got." />
      </details>
    </SetupSection>
  );

  /** The sampler's settings: the first few, the rest under Advanced. */
  const samplerSection = (n: number) =>
    chosen &&
    settingsSpec.length > 0 && (
      <SetupSection
        n={n}
        title={<T text="Sampler" />}
        summary={settingsSpec
          .slice(0, SAMPLER_SHOWN)
          .map((f) => `${f.label.toLowerCase()} ${config[f.name] ?? printGridValue(f.default)}`)
          .join(" · ")}
        {...section("sampler")}
      >
        <div className="an-wb-fields">
          <SettingsFields
            spec={settingsSpec.slice(0, SAMPLER_SHOWN)}
            values={config}
            onChange={setConfigKey}
            idPrefix="model-config"
          />
        </div>
        {settingsSpec.length > SAMPLER_SHOWN && (
          <details className="an-wb-details">
            <summary>
              {t("Advanced · {count} more", { count: settingsSpec.length - SAMPLER_SHOWN })}{" "}
              {settingsSpec.slice(SAMPLER_SHOWN).every((f) => (config[f.name] ?? "") === "") && (
                <span className="badge bg-secondary-lt ms-1">
                  <T text="all defaults" />
                </span>
              )}
            </summary>
            <div className="an-wb-fields mt-2">
              <SettingsFields
                spec={settingsSpec.slice(SAMPLER_SHOWN)}
                values={config}
                onChange={setConfigKey}
                idPrefix="model-config"
              />
            </div>
          </details>
        )}
      </SetupSection>
    );

  // A formula model's own parts, numbered on from the algorithm.
  const formulaStart = step;
  const formulaFrame = (f: FormulaFrame): ReactNode =>
    f.part === "data" ? (
      <details className="an-wb-section an-wb-section-details">
        <summary>{f.title}</summary>
        <div className="d-flex justify-content-end px-3 pt-2">{f.aside}</div>
        {f.children}
      </details>
    ) : (
      <SetupSection
        n={formulaStart + (f.part === "formula" ? 1 : 2)}
        title={f.title}
        aside={f.aside}
        problem={f.problem}
        flush={f.flush}
        summary={
          f.part === "formula"
            ? (config.formula ?? "").split("\n")[0]
            : t("{count} set by you", { count: readRows(config.priors).length })
        }
        {...section(f.part)}
      >
        {f.children}
      </SetupSection>
    );

  const setupBody = (
    <>
      {dataSection}
      {algorithmSection}
      {formulaProvider && chosen && (
        <FormulaSection
          config={config}
          setConfig={setConfigKey}
          modelBody={body}
          savedId={id}
          onConverted={(made) => pane.go({ name: "model", id: made })}
          frame={formulaFrame}
          between={samplerSection(formulaStart + 3)}
          onCheck={setFormulaCheck}
        />
      )}
      {bindingTable && chosen && (
        <>
          <SetupSection n={next()} title={<T text="Program and data" />} summary={t("The program and its bindings")} {...section("program")}>
            <BindingSection
              spec={chosen.config_spec}
              config={config}
              setConfig={setConfigKey}
              state={binding}
              setState={setBinding}
              iface={iface}
              setIface={setIface}
              main={mainDataset}
              datasets={datasets}
              modelBody={body}
              modelId={id}
            />
          </SetupSection>
          {samplerSection(next())}
        </>
      )}
      {!bindsData && (
        <>
          {featuresSection}
          {tuningSection}
          {splitSection}
        </>
      )}
    </>
  );

  // --- the fits column -------------------------------------------------------

  // Running first, then the active fit, then newest first.
  const ordered = [...instances].sort(
    (a, b) => Number(b.status === "fitting") - Number(a.status === "fitting"),
  );
  const listed = showOlder ? ordered : ordered.slice(0, SHOWN_FITS);
  const posteriorModel = bindsData;
  const cardCtx: FitCardContext | null = model
    ? {
        model,
        datasetName: mainDataset?.name,
        providerDescription: chosen?.description,
        formula: formulaProvider ? { formula: config.formula ?? "", family: config.family ?? "gaussian" } : null,
        plots: view.plots,
        onPlots: (plots) => changeView({ plots }),
        mayCreate,
        posterior: posteriorModel,
      }
    : null;

  const savedNote =
    !id ? null : saveState === "saving" ? (
      <span className="text-secondary">
        <T text="Saving…" />
      </span>
    ) : saveState === "error" ? (
      <span className="text-danger" title={saveError ?? undefined}>
        <T text="Not saved" />
      </span>
    ) : saveState === "dirty" ? (
      <span className="text-secondary">
        <T text="Unsaved changes" />
      </span>
    ) : (
      <span className="text-secondary">
        ✓ <T text="All changes saved" />
      </span>
    );

  const classes = ["an-wb", `an-wb-tab-${tab}`];

  return (
    <>
    <div className={classes.join(" ")}>
      {/* --- the bar ---------------------------------------------------------- */}
      <div className="an-wb-bar">
        <a
          href={pane.href({ name: "home" })}
          className="an-wb-back"
          onClick={(e) => {
            e.preventDefault();
            pane.go({ name: "home" });
          }}
        >
          ← <span className="an-wb-back-text"><T text="All models" /></span>
        </a>
        <div className="an-wb-title">
          <label htmlFor="modelName" className="an-wb-title-label">
            <T text="Model name" />
          </label>
          <input
            id="modelName"
            className="an-wb-name"
            value={name}
            required
            placeholder={t("New model")}
            size={Math.max(8, name.length + 1)}
            onChange={(e) => setName(e.target.value)}
          />
        </div>
        {outcome && <span className="badge bg-blue-lt an-wb-outcome-badge">{outcomeSummary(outcome)}</span>}
        {model?.error && (
          <StatusBadge tone="red" title={model.error}>
            <T text="Cannot be fitted" />
          </StatusBadge>
        )}
        {id && name.trim() !== "" && (
          <code className="an-wb-call" title={t('What predict("…") in a formula and models.get("…") in code name this model by.')}>
            predict({JSON.stringify(name.trim())})
          </code>
        )}
        <div className="an-wb-bar-end">
          <div className="an-wb-saved small">
            {savedNote}
            {!id && (
              <Button size="sm" variant="outline-primary" disabled={busy || name.trim() === ""} onClick={() => void create()}>
                <T text="Create model" />
              </Button>
            )}
          </div>
          <div className="an-wb-fit">
            <Button
              variant="primary"
              disabled={busy || running !== null || blocker !== null}
              onClick={() => void fit()}
            >
              {running !== null ? <T text="Fitting…" /> : <>▶ <T text="Fit" /></>}
            </Button>
            {(blocker || (saveState === "error" && saveError)) && (
              <div className="an-wb-blocker small text-danger" title={saveError ?? undefined}>
                {blocker ?? saveError}
              </div>
            )}
          </div>
          <Dropdown align="end">
            <Dropdown.Toggle variant="outline-secondary" className="an-more" aria-label={t("More")}>
              ⋯
            </Dropdown.Toggle>
            <Dropdown.Menu>
              <Dropdown.Item onClick={() => setShowDescription((v) => !v)}>
                <T text="Description" />
              </Dropdown.Item>
              {id && (
                <Dropdown.Item onClick={() => void save().catch(() => undefined)}>
                  <T text="Save now" />
                </Dropdown.Item>
              )}
            </Dropdown.Menu>
          </Dropdown>
        </div>
      </div>

      <div className="an-wb-page">
        {(showDescription || description.trim() !== "") && (
          <Form.Group className="mb-3" controlId="modelDescription">
            <Form.Control
              size="sm"
              value={description}
              placeholder={t("Description")}
              aria-label={t("Description")}
              onChange={(e) => setDescription(e.target.value)}
            />
          </Form.Group>
        )}
        {error && <Alert variant="danger">{error}</Alert>}
        {activeFit?.dataset_changed && (
          <div className="an-wb-banner">
            <span>
              <strong>
                <T text="Dataset changed since the active fit." />
              </strong>{" "}
              {t("Fit #{fit} was trained on an earlier version of the dataset.", {
                fit: numbers.get(activeFit.id) ?? "",
              })}
            </span>
            <Button
              size="sm"
              variant="outline-secondary"
              className="ms-auto"
              disabled={busy || running !== null || blocker !== null}
              onClick={() => void fit()}
            >
              <T text="Refit now" />
            </Button>
          </div>
        )}
        {programCheck && (
          <Alert variant="secondary">
            <pre className="text-pre-wrap small mb-0">{programCheck}</pre>
          </Alert>
        )}

        {/* --- narrow: two tabs ------------------------------------------------ */}
        <div className="an-wb-tabs" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={tab === "setup"}
            className={tab === "setup" ? "is-active" : undefined}
            onClick={() => setTab("setup")}
          >
            <T text="Setup" />
            {tuningChanged && <span className="an-wb-dot" aria-label={t("changed since the fit")} />}
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === "fits"}
            className={tab === "fits" ? "is-active" : undefined}
            onClick={() => setTab("fits")}
          >
            {t("Fits · {count}", { count: instances.length })}
          </button>
        </div>

        <div className="an-wb-columns">
          {/* --- setup ------------------------------------------------------- */}
          <div className="an-wb-setup">
            <div className="an-wb-setup-head">
              <span>
                <T text="Setup" />
              </span>
              {tuningChanged && reference && (
                <span className="ms-auto small text-secondary">
                  <span className="an-wb-dot" /> {t("changed since fit #{fit}", { fit: numbers.get(reference.id) ?? "" })}
                </span>
              )}
            </div>
            {setupBody}
          </div>

          {/* --- fits -------------------------------------------------------- */}
          <div className="an-wb-fits">
            <div className="an-wb-fits-head">
              <h2>
                <T text="Fits" />
              </h2>
              <span className="badge bg-secondary-lt">{instances.length}</span>
              <span className="an-wb-order small text-secondary">
                <T text="Running first, then the active fit, then newest" />
              </span>
            </div>
            {instances.length === 0 ? (
              <div className="an-wb-empty">
                <T text="Not fitted yet. A fit reads every row of the dataset and runs on the server; its card here says how it went." />
              </div>
            ) : (
              cardCtx && (
                <>
                  <FitHeader posterior={posteriorModel} />
                  {listed.map((instance) => (
                    <FitCard
                      key={instance.id}
                      ctx={cardCtx}
                      instance={instance}
                      number={numbers.get(instance.id) ?? 0}
                      open={openFits.includes(instance.id)}
                      onToggle={() => toggleFit(instance.id)}
                      live={instance.id === running ? live : null}
                      onReport={(reportTab) => setReporting({ id: instance.id, tab: reportTab })}
                      onActivate={() => void activate(instance)}
                      onRemove={() => void removeInstance(instance)}
                      onCancel={() => void cancel(instance)}
                      onChanged={() => id && void loadInstances(id)}
                    />
                  ))}
                  {ordered.length > SHOWN_FITS && (
                    <a
                      href="#"
                      className="small"
                      onClick={(e) => {
                        e.preventDefault();
                        setShowOlder((v) => !v);
                      }}
                    >
                      {showOlder
                        ? t("Show fewer fits")
                        : t("Show {count} older fits", { count: ordered.length - SHOWN_FITS })}
                    </a>
                  )}
                  <p className="text-secondary small mt-3 mb-0">
                    <T text='The active fit is the one predict("…") and models.get("…") answer with. A fit still running when the server stops is failed at boot.' />
                  </p>
                </>
              )
            )}
          </div>
        </div>
      </div>
    </div>

      {model && reporting && (
        <FitReportModal
          key={`${reporting.id}:${reporting.tab}`}
          model={model}
          instanceId={reporting.id}
          initialTab={reporting.tab}
          providerDescription={providers.find((p) => p.name === model.provider)?.description}
          datasetName={datasets.find((d) => d.id === readModelDataset(model.dataset)?.dataset_id)?.name}
          plots={view.plots}
          onPlots={(plots) => changeView({ plots })}
          onChanged={() => id && void loadInstances(id)}
          onClose={() => setReporting(null)}
        />
      )}
    </>
  );
}

/**
 * One numbered section of the setup column. Below the editor's breakpoint it
 * folds to its title and a one-line summary, unfolded by its caret; above it,
 * it is always open and the summary and caret are not drawn.
 */
function SetupSection({
  n,
  title,
  aside,
  summary,
  changed = false,
  tint = false,
  problem = false,
  flush = false,
  folded,
  onFold,
  children,
}: {
  n: number;
  title: ReactNode;
  aside?: ReactNode;
  /** What the section comes to, for its folded form. */
  summary?: string;
  /** Differs from the fit the setup is compared with: a dot by the title. */
  changed?: boolean;
  /** Tinted, as a changed section is. */
  tint?: boolean;
  problem?: boolean;
  /** The body runs to the section's edges (a table). */
  flush?: boolean;
  folded: boolean;
  onFold: () => void;
  children: ReactNode;
}) {
  const { t } = useT();
  const classes = ["an-wb-section"];
  if (folded) classes.push("is-folded");
  if (tint) classes.push("is-changed");
  if (problem) classes.push("has-problem");
  return (
    <section className={classes.join(" ")}>
      <header className="an-wb-section-head">
        <span className="an-wb-step">{n}</span>
        <div className="an-wb-section-title">
          <h3>
            {title}
            {changed && <span className="an-wb-dot" aria-label={t("changed since the fit")} />}
          </h3>
          {summary && <div className="an-wb-section-summary">{summary}</div>}
        </div>
        {aside && <div className="an-wb-section-aside">{aside}</div>}
        <button
          type="button"
          className="an-wb-fold"
          aria-expanded={!folded}
          aria-label={folded ? t("Open") : t("Close")}
          onClick={onFold}
        >
          {folded ? "▾" : "▴"}
        </button>
      </header>
      <div className={flush ? "an-wb-section-body is-flush" : "an-wb-section-body"}>{children}</div>
    </section>
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

