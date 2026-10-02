// The model editor's half for a provider that binds data (Stan TODO §18;
// analytics TODO A3.6, moved from the admin UI): where the program is and the
// program itself, in an editor (`ProgramEditor.tsx`), the datasets beside the
// main one, the dimensions, and the
// **binding table** — one row per `data` variable the program declares, each
// with a kind picker offering only the kinds that can produce that
// declaration, the kind's fields, and what Preview data bound it to.
//
// **Nothing here names Stan.** A provider declares `binds_data`, and the form
// renders this for any provider that does — the rule `FileStoreForm` follows
// for git. The configuration keys it edits are the host's (`bindings`,
// `dimensions`, `policies`) plus the two that say where a program is, which are
// what `getProgramInterface` takes.
//
// The binder is the authority: the kind picker mirrors its save-time rule, and
// everything else — sizes against the data, bounds, nulls, unknown keys — is
// what **Preview data** asks it, with each sentence put back on the row it is
// about.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import { DatasetPicker, type DatasetItem } from "./DatasetPicker";
import {
  BINDING_KINDS,
  DIMENSION_KINDS,
  MAIN_DATASET,
  PROGRAM_KEY,
  PROGRAM_STORE_KEY,
  applySuggestions,
  dimensionNames,
  kindsFor,
  parseDraft,
  readInterface as parseInterface,
  shapeText,
  type Declaration,
  type Interface,
  type KindDraft,
  type KindField,
  type KindSpec,
  type NamedDataset,
  type Policies,
  type VariablePreview,
} from "./models";
import { ProgramEditor } from "./ProgramEditor";
import { asString, type FieldSpec } from "./settings";

/** Where a file store is edited as code: the IDE, a page of its own at
 * `/ide/`, opened on the program when one is named (Stan TODO §18). */
export function ideUrl(store: string, path?: string): string {
  const base = `/ide/?store=${encodeURIComponent(store)}`;
  return path && path.trim() !== "" ? `${base}&path=${encodeURIComponent(path.trim())}` : base;
}

/** How long the form waits after the program's path is typed before reading
 * its interface — a `stanc` run, about a second. */
const INTERFACE_DEBOUNCE_MS = 800;

/** One declared dimension, as the editor holds it: a name and a draft. */
export type DimensionRow = { name: string; draft: KindDraft };

/** Everything the binding half of the form edits, beside the program. */
export type BindingState = {
  related: NamedDataset[];
  dimensions: DimensionRow[];
  policies: Record<string, Policies>;
  bindings: Record<string, KindDraft>;
};

/** What "Check program" (`getProgramInterface`) answered. */
type ProgramCheck = {
  warnings?: string | null;
  notice?: string | null;
  error?: string | null;
};

/** What Preview data (`previewModelData`) answered. */
type DataPreview = {
  variables: VariablePreview[];
  report?: BindReport | null;
  errors: string[];
};

/** The binder's report on what it bound (`sc_model::BindReport`). */
export type BindReport = {
  datasets: { name: string; read: number; bound: number }[];
  dimensions: Record<string, number>;
  drops?: { sentence: string }[];
  values: number;
  warnings?: string[];
};

export function BindingSection({
  spec,
  config,
  setConfig,
  state,
  setState,
  iface,
  setIface,
  main,
  datasets: stored,
  modelBody,
  modelId,
}: {
  /** The provider's settings declaration, for the program store's options. */
  spec: FieldSpec[];
  config: Record<string, string>;
  setConfig: (key: string, value: string) => void;
  state: BindingState;
  setState: (update: (s: BindingState) => BindingState) => void;
  iface: Interface | null;
  setIface: (iface: Interface | null) => void;
  /** The model's own dataset, as the stored datasets list it; `null` while
   * none is picked. */
  main: DatasetItem | null;
  /** Every stored dataset, for the related datasets' pickers. */
  datasets: DatasetItem[];
  /** Everything on the form as `saveModel` takes it — what Preview data and
   * Bind automatically are asked about. */
  modelBody: () => Record<string, unknown>;
  /** The saved model's id, which Compile needs; `null` for a new one. */
  modelId: string | null;
}) {
  const { t } = useT();
  const store = config[PROGRAM_STORE_KEY] ?? "";
  const path = config[PROGRAM_KEY] ?? "";
  const [check, setCheck] = useState<ProgramCheck | null>(null);
  const [checking, setChecking] = useState(false);
  const [programs, setPrograms] = useState<string[]>([]);
  const [preview, setPreview] = useState<DataPreview | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [busy, setBusy] = useState<"preview" | "suggest" | "compile" | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [compiled, setCompiled] = useState<string | null>(null);

  // --- the program -----------------------------------------------------------

  const readInterface = async (explicit: boolean) => {
    if (store.trim() === "" || path.trim() === "") return;
    if (explicit) setChecking(true);
    try {
      const answer = await api.getProgramInterface({ store, path });
      setCheck(answer);
      setIface(parseInterface(answer.interface));
    } catch (err) {
      setCheck({ error: errorMessage(err, "Could not read the program.") });
      setIface(null);
    } finally {
      if (explicit) setChecking(false);
    }
  };

  // The interface follows the program: the binding table is one row per
  // variable it declares, so a path typed is a table redrawn.
  useEffect(() => {
    if (store.trim() === "" || path.trim() === "") {
      setIface(null);
      setCheck(null);
      return undefined;
    }
    const timer = window.setTimeout(() => void readInterface(false), INTERFACE_DEBOUNCE_MS);
    return () => window.clearTimeout(timer);
    // `readInterface` is a function of these two.
  }, [store, path]);

  // The `.stan` files of the chosen store, offered as the path's suggestions.
  useEffect(() => {
    if (store.trim() === "") {
      setPrograms([]);
      return undefined;
    }
    let cancelled = false;
    void api
      .findFiles(store, { query: ".stan", max_results: 100 })
      .then((found) => {
        if (!cancelled) setPrograms(found.entries.filter((e) => !e.is_dir).map((e) => e.path));
      })
      .catch(() => {
        if (!cancelled) setPrograms([]);
      });
    return () => {
      cancelled = true;
    };
  }, [store]);

  const compile = async () => {
    if (!modelId) return;
    setBusy("compile");
    setCompiled(null);
    try {
      const answer = await api.compileModel(modelId);
      setCompiled(
        answer.cached
          ? t("Already compiled (CmdStan {version}).", { version: answer.cmdstan })
          : t("Compiled (CmdStan {version}); a fit will not wait for it.", {
              version: answer.cmdstan,
            }),
      );
    } catch (err) {
      setCompiled(errorMessage(err, "Could not compile the program."));
    } finally {
      setBusy(null);
    }
  };

  // --- the datasets and dimensions ------------------------------------------

  // Each dataset's columns, from the stored datasets: what a binding may name.
  const columnsOfStored = (id: string | undefined) =>
    ((stored.find((d) => d.id === id)?.columns ?? []) as { name: string }[]).map((c) => c.name);
  const datasets: { name: string; columns: string[] }[] = [
    { name: MAIN_DATASET, columns: columnsOfStored(main?.id) },
    ...state.related.map((r) => ({ name: r.name, columns: columnsOfStored(r.dataset_id) })),
  ];
  const datasetNames = datasets.map((d) => d.name);
  const columnsOf = (name: string) => datasets.find((d) => d.name === name)?.columns ?? [];
  const dimensions = dimensionNames(
    datasetNames,
    Object.fromEntries(state.dimensions.map((d) => [d.name, d.draft])),
  );

  const editRelated = (index: number, over: Partial<NamedDataset>) =>
    setState((s) => ({
      ...s,
      related: s.related.map((r, i) => (i === index ? { ...r, ...over } : r)),
    }));

  const editDimension = (index: number, over: Partial<DimensionRow>) =>
    setState((s) => ({
      ...s,
      dimensions: s.dimensions.map((d, i) => (i === index ? { ...d, ...over } : d)),
    }));

  // --- the bindings ----------------------------------------------------------

  const setBinding = (name: string, draft: KindDraft) =>
    setState((s) => ({ ...s, bindings: { ...s.bindings, [name]: draft } }));

  const suggest = async () => {
    setBusy("suggest");
    setNotice(null);
    try {
      const answer = await api.suggestBindings(modelBody() as never);
      let filled: string[] = [];
      setState((s) => {
        const applied = applySuggestions(s.bindings, answer.bindings);
        filled = applied.filled;
        return { ...s, bindings: applied.drafts };
      });
      const reasons = (answer.reasons ?? {}) as Record<string, string>;
      setNotice(
        filled.length === 0
          ? t("Nothing to suggest: every variable the names or the keys say anything about is bound.")
          : filled.map((name) => `${name}: ${reasons[name] ?? ""}`).join("\n"),
      );
    } catch (err) {
      setNotice(errorMessage(err, "Could not suggest bindings."));
    } finally {
      setBusy(null);
    }
  };

  const runPreview = async () => {
    setBusy("preview");
    setPreviewError(null);
    try {
      setPreview((await api.previewModelData(modelBody() as never)) as DataPreview);
    } catch (err) {
      setPreview(null);
      setPreviewError(errorMessage(err, "Could not preview the data."));
    } finally {
      setBusy(null);
    }
  };

  const storeField = spec.find((f) => f.name === PROGRAM_STORE_KEY);
  const storeOptions = (storeField?.options ?? []).map(asString);
  const declared = iface?.data ?? [];

  return (
    <>
      {/* --- the program ----------------------------------------------------- */}
      <Card className="mb-3">
        <Card.Header><T text="Program" /></Card.Header>
        <Card.Body>
          <Row>
            <Col md={4}>
              <Form.Group className="mb-3" controlId="model-program-store">
                <Form.Label>
                  <T text="File store" /><span className="text-danger"> *</span>
                </Form.Label>
                <Form.Select
                  value={store}
                  onChange={(e) => setConfig(PROGRAM_STORE_KEY, e.target.value)}
                >
                  <option value="">—</option>
                  {store !== "" && !storeOptions.includes(store) && (
                    <option value={store}>{t("{name} (missing)", { name: store })}</option>
                  )}
                  {storeOptions.map((name) => (
                    <option key={name} value={name}>
                      {name}
                    </option>
                  ))}
                </Form.Select>
              </Form.Group>
            </Col>
            <Col md={8}>
              <Form.Group className="mb-3" controlId="model-program-path">
                <Form.Label>
                  <T text="Program" /><span className="text-danger"> *</span>
                </Form.Label>
                <Form.Control
                  className="font-monospace"
                  value={path}
                  list="model-program-paths"
                  placeholder="radon.stan"
                  onChange={(e) => setConfig(PROGRAM_KEY, e.target.value)}
                />
                <datalist id="model-program-paths">
                  {programs.map((p) => (
                    <option key={p} value={p} />
                  ))}
                </datalist>
                <Form.Text muted>
                  <T text="The program lives in the file store and nowhere else: edited below or in the IDE, versioned with the store, and a fit keeps a copy of what it ran." />
                </Form.Text>
              </Form.Group>
            </Col>
          </Row>
          <div className="btn-list">
            <Button
              variant="outline-primary"
              disabled={checking || store === "" || path === ""}
              onClick={() => void readInterface(true)}
            >
              {checking ? <T text="Checking…" /> : <T text="Check program" />}
            </Button>
            <Button
              variant="outline-secondary"
              disabled={store === ""}
              href={store === "" ? undefined : ideUrl(store, path)}
            >
              <T text="Open in IDE" />
            </Button>
            <Button
              variant="outline-secondary"
              disabled={!modelId || busy !== null}
              title={modelId ? undefined : t("Save the model first.")}
              onClick={() => void compile()}
            >
              {busy === "compile" ? <T text="Compiling…" /> : <T text="Compile" />}
            </Button>
          </div>
          {store.trim() !== "" && path.trim() !== "" && (
            <ProgramEditor key={`${store}:${path}`} store={store} path={path} onSaved={() => void readInterface(true)} />
          )}
          {compiled && <p className="small mt-3 mb-0">{compiled}</p>}
          {check?.error && (
            <Alert variant="danger" className="mt-3 mb-0">
              <pre className="text-pre-wrap mb-0 small">{check.error}</pre>
            </Alert>
          )}
          {check?.notice && (
            <Alert variant="secondary" className="mt-3 mb-0">
              {check.notice}
            </Alert>
          )}
          {check?.warnings && (
            <Alert variant="warning" className="mt-3 mb-0">
              <pre className="text-pre-wrap mb-0 small">{check.warnings}</pre>
            </Alert>
          )}
          {iface && !check?.error && (
            <p className="text-muted small mt-3 mb-0">
              {t(
                "It declares {data} data variables, {parameters} parameters, {transformed} transformed parameters and {generated} generated quantities.",
                {
                  data: iface.data.length,
                  parameters: iface.parameters.length,
                  transformed: iface.transformed.length,
                  generated: iface.generated.length,
                },
              )}
            </p>
          )}
        </Card.Body>
      </Card>

      {/* --- related datasets ------------------------------------------------ */}
      <Card className="mb-3">
        <Card.Header><T text="Related datasets" /></Card.Header>
        <Card.Body>
          <p className="text-muted small">
            <T text="Datasets beside the main one, over other tables: the groups the observations belong to, a junction table of neighbours, a table of covariates. Bindings address the main dataset as" /> <code>main</code> <T text="and each of these by its name. Each is also a dimension: its rows, in its order, labelled by its label formula." />
          </p>
          {state.related.map((r, index) => (
            <Card className="mb-3" key={index}>
              <Card.Header className="d-flex gap-2 align-items-center">
                <Form.Control
                  size="sm"
                  className="font-monospace w-auto"
                  value={r.name}
                  placeholder={t("counties")}
                  aria-label={t("Related dataset {n} name", { n: index + 1 })}
                  onChange={(e) => editRelated(index, { name: e.target.value })}
                />
                <Form.Control
                  size="sm"
                  className="font-monospace w-auto"
                  value={r.label ?? ""}
                  placeholder={t("label formula, e.g. name")}
                  aria-label={t("Related dataset {n} label", { n: index + 1 })}
                  onChange={(e) => editRelated(index, { label: e.target.value })}
                />
                <Button
                  size="sm"
                  variant="outline-danger"
                  className="ms-auto"
                  aria-label={t("Remove related dataset {n}", { n: index + 1 })}
                  onClick={() =>
                    setState((s) => ({ ...s, related: s.related.filter((_, i) => i !== index) }))
                  }
                >
                  ×
                </Button>
              </Card.Header>
              <Card.Body>
                <DatasetPicker
                  value={r.dataset_id}
                  onChange={(dataset_id) => editRelated(index, { dataset_id })}
                  datasets={stored}
                  idPrefix={`model-related-${index}`}
                  preview={false}
                />
              </Card.Body>
            </Card>
          ))}
          <Button
            variant="outline-secondary"
            onClick={() =>
              setState((s) => ({
                ...s,
                related: [
                  ...s.related,
                  { name: "", dataset_id: "", label: null },
                ],
              }))
            }
          >
            + 
            <T text="Add a related dataset" />
          </Button>
        </Card.Body>
      </Card>

      {/* --- dimensions ------------------------------------------------------ */}
      <Card className="mb-3">
        <Card.Header><T text="Dimensions" /></Card.Header>
        <Card.Body>
          <p className="text-muted small">
            {t("Every dataset is already a dimension of its rows: {names}.", {
              names: datasetNames.filter((n) => n.trim() !== "").join(", "),
            })}{" "}
            <T text="Declare the others here: the distinct values of a column (only the values present — a group with no rows gets no position), or a time grid over a date column, which also offers its horizon alone as" /> <code>name.future</code>.
          </p>
          {state.dimensions.map((d, index) => (
            <div className="border rounded p-2 mb-2" key={index}>
              <div className="d-flex gap-2 align-items-start flex-wrap">
                <Form.Control
                  size="sm"
                  className="font-monospace w-auto"
                  value={d.name}
                  placeholder={t("day")}
                  aria-label={t("Dimension {n} name", { n: index + 1 })}
                  onChange={(e) => editDimension(index, { name: e.target.value })}
                />
                <Form.Select
                  size="sm"
                  className="w-auto"
                  value={d.draft.kind}
                  aria-label={t("Dimension {n} kind", { n: index + 1 })}
                  onChange={(e) =>
                    editDimension(index, { draft: { kind: e.target.value, fields: d.draft.fields } })
                  }
                >
                  <option value="">—</option>
                  <option value="values">{t("values of a column")}</option>
                  <option value="time_grid">{t("time grid")}</option>
                </Form.Select>
                <KindFields
                  specs={DIMENSION_KINDS}
                  draft={d.draft}
                  onChange={(draft) => editDimension(index, { draft })}
                  datasets={datasetNames}
                  columnsOf={columnsOf}
                  dimensions={dimensions}
                  variables={[]}
                  idPrefix={`model-dimension-${index}`}
                />
                <Button
                  size="sm"
                  variant="outline-danger"
                  className="ms-auto"
                  aria-label={t("Remove dimension {n}", { n: index + 1 })}
                  onClick={() =>
                    setState((s) => ({
                      ...s,
                      dimensions: s.dimensions.filter((_, i) => i !== index),
                    }))
                  }
                >
                  ×
                </Button>
              </div>
              <DraftProblems specs={DIMENSION_KINDS} draft={d.draft} />
            </div>
          ))}
          <Button
            size="sm"
            variant="outline-secondary"
            onClick={() =>
              setState((s) => ({
                ...s,
                dimensions: [...s.dimensions, { name: "", draft: { kind: "values", fields: {} } }],
              }))
            }
          >
            + 
            <T text="Declare a dimension" />
          </Button>
          <p className="text-muted small mt-2 mb-0">
            <T text="A time grid's step is a count and a unit (1 day, 3 hours, 1 month); months, quarters and years step by the calendar, in UTC." />
          </p>
        </Card.Body>
      </Card>

      {/* --- the binding table ----------------------------------------------- */}
      <Card className="mb-3">
        <Card.Header className="d-flex align-items-center gap-2">
          <span><T text="Data" /></span>
          <div className="ms-auto btn-list">
            <Button
              size="sm"
              variant="outline-primary"
              disabled={busy !== null || declared.length === 0}
              onClick={() => void suggest()}
            >
              {busy === "suggest" ? <T text="Suggesting…" /> : <T text="Bind automatically" />}
            </Button>
            <Button
              size="sm"
              variant="outline-primary"
              disabled={busy !== null || declared.length === 0}
              onClick={() => void runPreview()}
            >
              {busy === "preview" ? <T text="Reading…" /> : <T text="Preview data" />}
            </Button>
          </div>
        </Card.Header>
        {notice && (
          <Card.Body className="border-bottom py-2">
            <div className="text-pre-wrap small text-secondary">{notice}</div>
          </Card.Body>
        )}
        {declared.length === 0 ? (
          <Card.Body className="text-muted">
            <T text="Choose a program, and its data block's variables are listed here, one row each." />
          </Card.Body>
        ) : (
          <div className="table-responsive">
            <Table size="sm" className="card-table table-vcenter">
              <thead>
                <tr>
                  <th><T text="Variable" /></th>
                  <th><T text="Binding" /></th>
                  <th><T text="Bound" /></th>
                </tr>
              </thead>
              <tbody>
                {declared.map((decl) => (
                  <BindingRow
                    key={decl.name}
                    decl={decl}
                    draft={state.bindings[decl.name] ?? { kind: "", fields: {} }}
                    onChange={(draft) => setBinding(decl.name, draft)}
                    preview={preview?.variables.find((v) => v.name === decl.name) ?? null}
                    datasets={datasetNames}
                    columnsOf={columnsOf}
                    dimensions={dimensions}
                    variables={declared.map((d) => d.name).filter((n) => n !== decl.name)}
                  />
                ))}
              </tbody>
            </Table>
          </div>
        )}
        {previewError && (
          <Card.Body>
            <Alert variant="danger" className="mb-0">{previewError}</Alert>
          </Card.Body>
        )}
        {preview && <PreviewReport preview={preview} />}
        <Card.Body className="border-top">
          <h4 className="h5"><T text="When a row cannot be bound" /></h4>
          <p className="text-muted small">
            <T text="A null in a bound column, or a key that is not a position of its dimension, refuses the fit by default and names the first row. Dropping takes the row out before anything of its dataset is counted, and the fit says how many went." />
          </p>
          <Table size="sm" className="mb-0 w-auto">
            <thead>
              <tr>
                <th><T text="Dataset" /></th>
                <th><T text="A null" /></th>
                <th><T text="An unknown key" /></th>
              </tr>
            </thead>
            <tbody>
              {datasetNames
                .filter((n) => n.trim() !== "")
                .map((name) => {
                  const p = state.policies[name] ?? { nulls: "refuse", unknown: "refuse" };
                  const set = (over: Partial<Policies>) =>
                    setState((s) => ({
                      ...s,
                      policies: { ...s.policies, [name]: { ...p, ...over } },
                    }));
                  return (
                    <tr key={name}>
                      <td className="font-monospace">{name}</td>
                      {(["nulls", "unknown"] as const).map((which) => (
                        <td key={which}>
                          <Form.Select
                            size="sm"
                            value={p[which]}
                            aria-label={`${name} ${which}`}
                            onChange={(e) => set({ [which]: e.target.value as "refuse" | "drop" })}
                          >
                            <option value="refuse">{t("refuse the fit")}</option>
                            <option value="drop">{t("drop the row")}</option>
                          </Form.Select>
                        </td>
                      ))}
                    </tr>
                  );
                })}
            </tbody>
          </Table>
        </Card.Body>
      </Card>
    </>
  );
}

/** One `data` variable's row: its declaration, its binding, and what Preview
 * data bound it to — or why it could not. */
function BindingRow({
  decl,
  draft,
  onChange,
  preview,
  datasets,
  columnsOf,
  dimensions,
  variables,
}: {
  decl: Declaration;
  draft: KindDraft;
  onChange: (draft: KindDraft) => void;
  preview: VariablePreview | null;
  datasets: string[];
  columnsOf: (dataset: string) => string[];
  dimensions: string[];
  variables: string[];
}) {
  const { t } = useT();
  const kinds = kindsFor(decl);
  return (
    <tr>
      <td className="align-top">
        <div className="font-monospace fw-bold">{decl.name}</div>
        <div className="font-monospace small text-muted">{decl.stan_type}</div>
      </td>
      <td className="align-top">
        <div className="d-flex gap-2 align-items-start flex-wrap">
          <Form.Select
            size="sm"
            className="w-auto"
            value={draft.kind}
            aria-label={t("How {name} is bound", { name: decl.name })}
            onChange={(e) =>
              // The dataset carries over between kinds, since most of them
              // read one and it is usually the same one.
              onChange({
                kind: e.target.value,
                fields: draft.fields.dataset ? { dataset: draft.fields.dataset } : {},
              })
            }
          >
            <option value="">—</option>
            {draft.kind !== "" && !kinds.includes(draft.kind) && (
              <option value={draft.kind}>{t("{kind} (does not fit)", { kind: draft.kind })}</option>
            )}
            {kinds.map((k) => (
              <option key={k} value={k}>
                {k}
              </option>
            ))}
          </Form.Select>
          <KindFields
            specs={BINDING_KINDS}
            draft={draft}
            onChange={onChange}
            datasets={datasets}
            columnsOf={columnsOf}
            dimensions={dimensions}
            variables={variables}
            idPrefix={`model-binding-${decl.name}`}
          />
        </div>
        <DraftProblems specs={BINDING_KINDS} draft={draft} />
      </td>
      <td className="align-top small">
        {preview?.error ? (
          <span className="text-danger">{preview.error}</span>
        ) : preview?.shape ? (
          <>
            <div className="text-nowrap">
              {shapeText(preview.shape)}
            </div>
            <div className="text-muted font-monospace text-truncate" title={JSON.stringify(preview.first)}>
              {preview.first.map((v) => JSON.stringify(v)).join(", ")}
              {preview.shape.reduce((n, d) => n * d, 1) > preview.first.length && " …"}
            </div>
          </>
        ) : null}
      </td>
    </tr>
  );
}

/** The problems this form can see in a row without the data, under it. */
function DraftProblems({ specs, draft }: { specs: KindSpec[]; draft: KindDraft }) {
  const { problems } = parseDraft(specs, draft);
  if (problems.length === 0) return null;
  return <div className="text-warning small mt-1">{problems.join("; ")}</div>;
}

/** A kind's fields, each as the control its content asks for. */
function KindFields({
  specs,
  draft,
  onChange,
  datasets,
  columnsOf,
  dimensions,
  variables,
  idPrefix,
}: {
  specs: KindSpec[];
  draft: KindDraft;
  onChange: (draft: KindDraft) => void;
  datasets: string[];
  columnsOf: (dataset: string) => string[];
  dimensions: string[];
  variables: string[];
  idPrefix: string;
}) {
  const { t } = useT();
  const spec = specs.find((k) => k.kind === draft.kind);
  if (!spec) {
    // A kind this form does not know: shown as its JSON, kept whole.
    return draft.raw ? (
      <code className="small">{JSON.stringify(draft.raw)}</code>
    ) : null;
  }
  const set = (path: string, value: string) =>
    onChange({ ...draft, fields: { ...draft.fields, [path]: value } });
  const dataset = draft.fields.dataset ?? "";
  return (
    <>
      {spec.fields.map((field) => (
        <FieldControl
          key={field.path}
          field={field}
          value={draft.fields[field.path] ?? ""}
          onChange={(v) => set(field.path, v)}
          options={optionsFor(field, { datasets, columns: columnsOf(dataset), dimensions, variables })}
          id={`${idPrefix}-${field.path}`}
          placeholder={
            field.kind === "columns"
              ? columnsOf(dataset).join(", ")
              : field.path === "fill"
                ? t("0, or \"NaN\"")
                : undefined
          }
        />
      ))}
    </>
  );
}

/** The choices a field offers, by what it holds. */
function optionsFor(
  field: KindField,
  lists: { datasets: string[]; columns: string[]; dimensions: string[]; variables: string[] },
): string[] | null {
  switch (field.kind) {
    case "dataset":
      return lists.datasets.filter((n) => n.trim() !== "");
    case "column":
      return lists.columns;
    case "dimension":
      return lists.dimensions;
    case "variable":
      return lists.variables;
    case "choice":
      return field.options ?? [];
    default:
      return null;
  }
}

/** One field: a select over its options, a checkbox, or a text box. Its name is
 * the configuration's own (`over.dimension`), because that is what the
 * binder's sentences call it. */
function FieldControl({
  field,
  value,
  onChange,
  options,
  id,
  placeholder,
}: {
  field: KindField;
  value: string;
  onChange: (value: string) => void;
  options: string[] | null;
  id: string;
  placeholder?: string;
}) {
  const label = (
    <Form.Label htmlFor={id} className="small text-muted mb-0">
      {field.path}
      {field.required && <span className="text-danger"> *</span>}
    </Form.Label>
  );
  if (field.kind === "bool") {
    return (
      <Form.Check
        type="checkbox"
        id={id}
        className="align-self-end"
        label={field.path}
        checked={value === "true"}
        onChange={(e) => onChange(e.target.checked ? "true" : "")}
      />
    );
  }
  if (options) {
    return (
      <div>
        {label}
        <Form.Select id={id} size="sm" value={value} onChange={(e) => onChange(e.target.value)}>
          <option value="">—</option>
          {value !== "" && !options.includes(value) && <option value={value}>{value}</option>}
          {options.map((o) => (
            <option key={o} value={o}>
              {o}
            </option>
          ))}
        </Form.Select>
      </div>
    );
  }
  return (
    <div>
      {label}
      <Form.Control
        id={id}
        size="sm"
        className="font-monospace"
        value={value}
        placeholder={placeholder}
        onChange={(e) => onChange(e.target.value)}
      />
    </div>
  );
}

/** What Preview data said about the whole: each dataset's rows read and bound,
 * each dimension's size, the drops, the warnings, and the sentences about no
 * one variable. */
function PreviewReport({ preview }: { preview: DataPreview }) {
  const { t } = useT();
  const report = preview.report;
  return (
    <Card.Body className="border-top">
      {preview.errors.map((e) => (
        <Alert variant="danger" key={e}>
          {e}
        </Alert>
      ))}
      {report && (
        <>
          <div className="d-flex flex-wrap gap-4 mb-2">
            {Object.entries(report.dimensions).map(([name, size]) => (
              <div key={name}>
                <div className="text-muted small font-monospace">{name}</div>
                <div className="h3 mb-0">{size}</div>
              </div>
            ))}
          </div>
          <div className="text-muted small">
            {report.datasets
              .map((d) =>
                d.read === d.bound
                  ? t("{name}: {rows} rows", { name: d.name, rows: d.read })
                  : t("{name}: {bound} of {read} rows", { name: d.name, bound: d.bound, read: d.read }),
              )
              .join(" · ")}
            {" · "}
            {t("{count} values in the data file", { count: report.values })}
          </div>
          {(report.drops ?? []).map((d) => (
            <Alert variant="secondary" className="mt-2 mb-0 py-2" key={d.sentence}>
              {d.sentence}
            </Alert>
          ))}
          {(report.warnings ?? []).map((w) => (
            <Alert variant="warning" className="mt-2 mb-0 py-2" key={w}>
              {w}
            </Alert>
          ))}
        </>
      )}
      {!report && preview.errors.length === 0 && (
        <p className="text-muted mb-0">
          <T text="Nothing bound yet." />
        </p>
      )}
    </Card.Body>
  );
}
