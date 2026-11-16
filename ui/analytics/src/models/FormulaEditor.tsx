// A `bayesian_regression` model's own half of the form
// (TODO-bayesian-regression §7, R1.8): the formula, typed as an R user types
// it, the family and its links, the prior table, and — collapsed — the Stan
// program and the data it generates. There is no binding table: the bindings
// are derived from the formula and shown read-only under "Data".
//
// The formula is a Monaco editor (the program editor's chunk, loaded on
// demand) with completion of the dataset's columns and the family's
// distributional parameters, and each mistake `checkFormula` names underlined
// at its span. Should Monaco not load, it is a text area and the mistakes are
// listed under it, as they always are.
//
// Prior calls pasted into the formula (`prior(normal(0, 1), class = b)`) are
// moved into the prior table when the field is committed — on blur, and on
// save — and the rows they replaced are named.
//
// What the editor deliberately does not show is the compile cache (§5):
// whether a structure happens to be compiled already must not steer which
// model an admin writes, so there is no "compiled" badge and no Compile button
// here. A fit that misses the cache shows its Compiling stage, as any Stan
// fit does.

import { useEffect, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import {
  PRIOR_NAMES,
  clearRow,
  coefText,
  completionsFor,
  greyed,
  priorProblem,
  readRows,
  retarget,
  retargets,
  sameTarget,
  setRow,
  sourceText,
  wordAt,
  writeRows,
  type FormulaCheck,
  type FormulaError,
  type PriorRow,
  type TableRow,
} from "./formula";
import { PreviewReport, type DataPreview } from "./ModelBindings";
import { monacoModule, monacoTheme } from "./ProgramEditor";

type Monaco = typeof import("monaco-editor");
type Editor = import("monaco-editor").editor.IStandaloneCodeEditor;

/** How long the form waits after a keystroke before checking the formula. */
const CHECK_DEBOUNCE_MS = 400;

/** What the completion provider offers: the latest check's, shared by every
 * formula editor on the page (Monaco's providers are per language). */
let currentCheck: FormulaCheck | null = null;
let registered = false;

/** The formula language: comments, numbers, the operators, names. */
function registerFormulaLanguage(monaco: Monaco) {
  if (registered) return;
  registered = true;
  monaco.languages.register({ id: "brms-formula" });
  monaco.languages.setMonarchTokensProvider("brms-formula", {
    tokenizer: {
      root: [
        [/#.*$/, "comment"],
        [/"[^"]*"/, "string"],
        [/\d+(\.\d*)?([eE][-+]?\d+)?/, "number"],
        [/~|\|\||\||%in%|[+\-*/:^]/, "operator"],
        [/[a-zA-Z_.][\w.]*(?=\s*\()/, "keyword"],
        [/[a-zA-Z_.][\w.]*/, "identifier"],
      ],
    },
  });
  monaco.languages.setLanguageConfiguration("brms-formula", {
    comments: { lineComment: "#" },
    brackets: [["(", ")"]],
    autoClosingPairs: [{ open: "(", close: ")" }],
  });
  monaco.languages.registerCompletionItemProvider("brms-formula", {
    provideCompletionItems(model, position) {
      const word = model.getWordUntilPosition(position);
      const range = {
        startLineNumber: position.lineNumber,
        endLineNumber: position.lineNumber,
        startColumn: word.startColumn,
        endColumn: word.endColumn,
      };
      const check = currentCheck;
      if (!check) return { suggestions: [] };
      const columns = new Set(check.completions.columns);
      const dpars = new Set(check.completions.dpars);
      const all = [...columns, ...dpars, ...PRIOR_NAMES];
      return {
        suggestions: [...new Set(all)].map((label) => ({
          label,
          insertText: label,
          range,
          kind: columns.has(label)
            ? monaco.languages.CompletionItemKind.Field
            : dpars.has(label)
              ? monaco.languages.CompletionItemKind.Variable
              : monaco.languages.CompletionItemKind.Function,
          detail: columns.has(label)
            ? "column"
            : dpars.has(label)
              ? "distributional parameter"
              : "prior",
        })),
      };
    },
  });
}

/** The formula's text editor: Monaco, or a text area should it not load. */
function FormulaField({
  value,
  onChange,
  onCommit,
  errors,
  check,
}: {
  value: string;
  onChange: (text: string) => void;
  onCommit: () => void;
  errors: FormulaError[];
  check: FormulaCheck | null;
}) {
  const { t } = useT();
  const container = useRef<HTMLDivElement>(null);
  const editor = useRef<Editor | null>(null);
  const monacoRef = useRef<Monaco | null>(null);
  const [failed, setFailed] = useState(false);
  // The handlers the editor calls, kept current without re-creating it.
  const handlers = useRef({ onChange, onCommit });
  handlers.current = { onChange, onCommit };

  useEffect(() => {
    if (failed) return undefined;
    let disposed = false;
    void monacoModule()
      .then((monaco) => {
        if (disposed || !container.current) return;
        registerFormulaLanguage(monaco);
        monacoRef.current = monaco;
        const instance = monaco.editor.create(container.current, {
          value,
          language: "brms-formula",
          theme: monacoTheme(),
          automaticLayout: true,
          minimap: { enabled: false },
          scrollBeyondLastLine: false,
          lineNumbers: "on",
          fontSize: 14,
          wordWrap: "on",
          scrollbar: { alwaysConsumeMouseWheel: false },
        });
        instance.onDidChangeModelContent(() => handlers.current.onChange(instance.getValue()));
        instance.onDidBlurEditorText(() => handlers.current.onCommit());
        editor.current = instance;
      })
      .catch(() => setFailed(true));
    return () => {
      disposed = true;
      editor.current?.dispose();
      editor.current = null;
    };
    // Created once; the value is pushed in below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [failed]);

  // A value changed from outside (pasted priors moved out on commit).
  useEffect(() => {
    const instance = editor.current;
    if (instance && instance.getValue() !== value) instance.setValue(value);
  }, [value]);

  // The mistakes, underlined where they are.
  useEffect(() => {
    const monaco = monacoRef.current;
    const model = editor.current?.getModel();
    if (!monaco || !model) return;
    monaco.editor.setModelMarkers(
      model,
      "formula",
      errors
        .filter((e) => e.line > 0)
        .map((e) => ({
          startLineNumber: e.line,
          startColumn: e.column,
          endLineNumber: e.line,
          endColumn: Math.max(e.end_column, e.column + 1),
          message: e.message,
          severity: monaco.MarkerSeverity.Error,
        })),
    );
  }, [errors]);

  // The text area's completion: the word being typed, offered below it.
  const area = useRef<HTMLTextAreaElement>(null);
  const [cursor, setCursor] = useState(0);
  const typed = wordAt(value, cursor);
  const offers = failed ? completionsFor(typed.word, check).slice(0, 8) : [];

  if (failed) {
    return (
      <>
        <Form.Control
          as="textarea"
          ref={area}
          rows={5}
          className="font-monospace"
          value={value}
          aria-label={t("Formula")}
          onChange={(e) => {
            onChange(e.target.value);
            setCursor(e.target.selectionStart ?? 0);
          }}
          onKeyUp={(e) => setCursor(e.currentTarget.selectionStart ?? 0)}
          onClick={(e) => setCursor(e.currentTarget.selectionStart ?? 0)}
          onBlur={onCommit}
        />
        {offers.length > 0 && (
          <div className="d-flex flex-wrap gap-1 mt-1">
            {offers.map((o) => (
              <Button
                key={o}
                size="sm"
                variant="outline-secondary"
                className="font-monospace py-0"
                onMouseDown={(e) => {
                  e.preventDefault();
                  const next = value.slice(0, typed.start) + o + value.slice(cursor);
                  onChange(next);
                  setCursor(typed.start + o.length);
                }}
              >
                {o}
              </Button>
            ))}
          </div>
        )}
      </>
    );
  }
  return (
    <div
      ref={container}
      className="border rounded"
      style={{ height: "9rem" }}
      aria-label={t("Formula")}
    />
  );
}

export function FormulaSection({
  config,
  setConfig,
  modelBody,
  savedId,
  onConverted,
}: {
  /** The form's settings, as text. */
  config: Record<string, string>;
  setConfig: (key: string, value: string) => void;
  /** Everything on the form as `saveModel` takes it. */
  modelBody: () => Record<string, unknown>;
  /** The model's id once it is saved: what can be converted to Stan. */
  savedId?: string | null;
  /** Open the Stan model a conversion made. */
  onConverted?: (id: string) => void;
}) {
  const { t } = useT();
  const formula = config.formula ?? "";
  const family = config.family ?? "gaussian";
  const center = (config.center ?? "true") !== "false";
  const rows = readRows(config.priors);
  const links = readLinks(config.links);

  const [check, setCheck] = useState<FormulaCheck | null>(null);
  const [checkError, setCheckError] = useState<string | null>(null);
  // After Preview data: the table with the data's numbers, and the preview.
  const [fromData, setFromData] = useState<FormulaCheck | null>(null);
  const [preview, setPreview] = useState<DataPreview | null>(null);
  const [previewing, setPreviewing] = useState(false);
  const [moved, setMoved] = useState<{ count: number; replaced: PriorRow[]; rows: PriorRow[] } | null>(
    null,
  );

  // Checked as it is typed, after a pause. The body is the key: anything on
  // the form that changes the answer changes it.
  const bodyKey = JSON.stringify(modelBody());
  useEffect(() => {
    let cancelled = false;
    const timer = window.setTimeout(() => {
      void api
        .checkFormula(JSON.parse(bodyKey) as never)
        .then((answer) => {
          if (cancelled) return;
          const c = answer as unknown as FormulaCheck;
          setCheck(c);
          currentCheck = c;
          setCheckError(null);
        })
        .catch((err) => {
          if (!cancelled) setCheckError(errorMessage(err, t("Could not check the formula.")));
        });
      // Numbers read from the data go stale with any edit; read them again.
      if (fromData) {
        void api
          .checkPriors(JSON.parse(bodyKey) as never)
          .then((answer) => !cancelled && setFromData(answer as unknown as FormulaCheck))
          .catch(() => !cancelled && setFromData(null));
      }
    }, CHECK_DEBOUNCE_MS);
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
    };
    // `fromData` is read, not watched: it is what this effect refreshes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bodyKey]);

  /** The field committed: pasted prior calls move into the table. */
  const commit = async () => {
    try {
      const answer = (await api.checkFormula(modelBody() as never)) as unknown as FormulaCheck;
      setCheck(answer);
      currentCheck = answer;
      if (answer.pasted > 0) {
        setConfig("formula", answer.formula);
        setConfig("priors", writeRows(answer.priors));
        setMoved({
          count: answer.pasted,
          replaced: answer.replaced ?? [],
          rows: answer.priors.filter((r) => !rows.some((old) => sameTarget(old, r) && old.prior === r.prior)),
        });
        window.setTimeout(() => setMoved(null), 6000);
      }
    } catch {
      // The debounced check says what is wrong.
    }
  };

  const previewData = async () => {
    setPreviewing(true);
    try {
      const [data, priors] = await Promise.all([
        api.previewModelData(modelBody() as never),
        api.checkPriors(modelBody() as never),
      ]);
      setPreview(data as unknown as DataPreview);
      setFromData(priors as unknown as FormulaCheck);
    } catch (err) {
      setPreview({ variables: [], errors: [errorMessage(err, t("Could not preview the data."))] });
    } finally {
      setPreviewing(false);
    }
  };

  const shown = fromData?.table ? fromData : check;
  const table = shown?.table ?? null;
  const errors = check?.errors ?? [];
  const familyLinks = check?.completions.links ?? {};
  const families = check?.completions.families ?? [family];

  const editRows = (next: PriorRow[]) => setConfig("priors", writeRows(next));
  const highlighted = (r: TableRow) => moved?.rows.some((m) => sameTarget(m, r)) ?? false;

  return (
    <>
      <Card className="mb-3">
        <Card.Header><T text="Formula" /></Card.Header>
        <Card.Body>
          <FormulaField
            value={formula}
            onChange={(text) => setConfig("formula", text)}
            onCommit={() => void commit()}
            errors={errors}
            check={check}
          />
          <Form.Text muted className="d-block">
            <T text="In lme4/brms syntax, one formula per line: the response and its terms, then any distributional parameter (`sigma ~ x`). Priors pasted from R move into the table below." />
          </Form.Text>
          {errors.map((e) => (
            <div className="text-danger small mt-1" key={`${e.span.start}-${e.message}`}>
              {e.line > 0 && (
                <span className="font-monospace">
                  {t("line {line}, column {column}", { line: e.line, column: e.column })}:{" "}
                </span>
              )}
              {e.message}
            </div>
          ))}
          {(check?.problems ?? []).map((p) => (
            <div className="text-danger small mt-1" key={p}>{p}</div>
          ))}
          {(check?.notes ?? []).map((n) => (
            <div className="text-muted small mt-1" key={n}>{n}</div>
          ))}
          {checkError && <div className="text-danger small mt-1">{checkError}</div>}
          {moved && (
            <Alert variant="info" className="mt-2 mb-0 py-2 small">
              {t("{count} pasted priors were moved into the table.", { count: moved.count })}
              {moved.replaced.map((r) => (
                <div key={JSON.stringify(r)}>
                  {t("Replaced {row}, which was {prior}.", {
                    row: rowName(r),
                    prior: r.prior || "flat",
                  })}
                </div>
              ))}
            </Alert>
          )}

          <Row className="mt-3">
            <Col md={4}>
              <Form.Group controlId="formula-family" className="mb-2">
                <Form.Label><T text="Family" /></Form.Label>
                <Form.Select value={family} onChange={(e) => setConfig("family", e.target.value)}>
                  {families.map((f) => (
                    <option key={f} value={f}>{f}</option>
                  ))}
                </Form.Select>
              </Form.Group>
              <Form.Check
                id="formula-center"
                className="mb-2"
                label={t("Centre the predictors")}
                checked={center}
                onChange={(e) => setConfig("center", e.target.checked ? "true" : "false")}
              />
            </Col>
            <Col md={8}>
              <Form.Label><T text="Links" /></Form.Label>
              <div className="d-flex flex-wrap gap-2">
                {Object.entries(familyLinks).map(([dpar, choices]) => (
                  <Form.Group key={dpar} controlId={`formula-link-${dpar}`}>
                    <Form.Label className="small text-muted font-monospace mb-0">{dpar}</Form.Label>
                    <Form.Select
                      size="sm"
                      value={links[dpar] ?? ""}
                      onChange={(e) =>
                        setConfig("links", writeLinks({ ...links, [dpar]: e.target.value }))
                      }
                    >
                      <option value="">{t("{link} (default)", { link: choices[0] })}</option>
                      {choices.slice(1).map((l) => (
                        <option key={l} value={l}>{l}</option>
                      ))}
                    </Form.Select>
                  </Form.Group>
                ))}
              </div>
            </Col>
          </Row>
        </Card.Body>
      </Card>

      <Card className="mb-3">
        <Card.Header className="d-flex align-items-center gap-2">
          <T text="Priors" />
          {shown && !shown.from_data && (
            <span className="text-muted small">
              <T text="— the defaults computed from the data show their rule until Preview data reads it." />
            </span>
          )}
        </Card.Header>
        <Card.Body className="p-0">
          {table ? (
            <PriorTable
              table={table.rows}
              rows={rows}
              center={center}
              onChange={editRows}
              highlighted={highlighted}
            />
          ) : (
            <p className="text-muted m-3">
              <T text="The table lists every parameter once the formula reads." />
            </p>
          )}
          {table && table.unmatched.length > 0 && (
            <div className="m-3">
              <h5 className="h6"><T text="Not matched by the formula" /></h5>
              <p className="text-muted small">
                <T text="These rows target nothing the formula has now. They are kept, never dropped — a fit is refused until each is deleted or pointed at a current parameter." />
              </p>
              {table.unmatched.map((u) => (
                <div className="d-flex align-items-center gap-2 mb-2 flex-wrap" key={JSON.stringify(u.row)}>
                  <span className="font-monospace">{rowName(u.row)}: {u.row.prior || "flat"}</span>
                  <span className="text-danger small">{u.reason}</span>
                  {retargets(table.rows, u.row).length > 0 && (
                    <Form.Select
                      size="sm"
                      style={{ width: "auto" }}
                      value=""
                      aria-label={t("Point at")}
                      onChange={(e) => e.target.value && editRows(retarget(rows, u.row, e.target.value))}
                    >
                      <option value="">{t("Point at…")}</option>
                      {retargets(table.rows, u.row).map((c) => (
                        <option key={c} value={c}>{c}</option>
                      ))}
                    </Form.Select>
                  )}
                  <Button size="sm" variant="outline-danger" onClick={() => editRows(clearRow(rows, u.row))}>
                    <T text="Delete" />
                  </Button>
                </div>
              ))}
            </div>
          )}
        </Card.Body>
      </Card>

      <Card className="mb-3">
        <Card.Header className="d-flex align-items-center gap-2">
          <T text="Data" />
          <Button
            size="sm"
            variant="outline-primary"
            className="ms-auto"
            disabled={previewing}
            onClick={() => void previewData()}
          >
            {previewing ? <T text="Reading…" /> : <T text="Preview data" />}
          </Button>
        </Card.Header>
        {preview && (
          <>
            {preview.variables.length > 0 && (
              <Table size="sm" className="mb-0">
                <tbody>
                  {preview.variables.map((v) => (
                    <tr key={v.name}>
                      <td className="font-monospace">{v.name}</td>
                      <td className="font-monospace text-muted small">{v.stan_type}</td>
                      <td className="small">
                        {v.error ? (
                          <span className="text-danger">{v.error}</span>
                        ) : (
                          <span className="font-monospace text-muted">
                            {(v.first ?? []).slice(0, 6).map((x) => JSON.stringify(x)).join(", ")}
                          </span>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </Table>
            )}
            <PreviewReport preview={preview} />
          </>
        )}
        <Card.Body className="py-2">
          <details>
            <summary className="small"><T text="How the formula's data is bound" /></summary>
            <pre className="small mb-0 mt-2">{JSON.stringify(check?.data ?? null, null, 2)}</pre>
          </details>
          <details className="mt-2">
            <summary className="small"><T text="Show Stan program" /></summary>
            <p className="text-muted small mt-2 mb-1">
              <T text="Generated from the formula, read-only. Column names, counts and prior numbers are data, so models of the same form share one program." />
            </p>
            <pre className="small border rounded p-2 mb-0" style={{ maxHeight: "28rem", overflow: "auto" }}>
              {check?.program ?? t("No program until the formula reads.")}
            </pre>
          </details>
          {savedId && <ConvertToStan modelId={savedId} onConverted={onConverted} />}
        </Card.Body>
      </Card>
    </>
  );
}

/** "Convert to a Stan model" (§9): the generated program written into a file
 * store, and a new `stan` model binding it as the formula does — the escape
 * hatch for anything a formula cannot say. The formula model is kept. */
function ConvertToStan({
  modelId,
  onConverted,
}: {
  modelId: string;
  onConverted?: (id: string) => void;
}) {
  const { t } = useT();
  const [stores, setStores] = useState<string[] | null>(null);
  const [store, setStore] = useState("");
  const [path, setPath] = useState("models/model.stan");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const load = () => {
    if (stores) return;
    api
      .listFileStores()
      .then((list) => {
        const names = list.filter((s) => s.connected).map((s) => s.name);
        setStores(names);
        setStore((current) => current || names[0] || "");
      })
      .catch((err) => setError(errorMessage(err, t("Could not list the file stores."))));
  };
  const convert = async () => {
    setBusy(true);
    setError(null);
    try {
      const made = await api.convertModelToStan(modelId, { store, path });
      onConverted?.(made.id);
    } catch (err) {
      setError(errorMessage(err, t("Could not convert the model.")));
    } finally {
      setBusy(false);
    }
  };
  return (
    <details className="mt-2" onToggle={(e) => (e.currentTarget as HTMLDetailsElement).open && load()}>
      <summary className="small"><T text="Convert to a Stan model" /></summary>
      <p className="text-muted small mt-2 mb-2">
        <T text="Writes the generated program into a file store and makes a new Stan model that binds it as this formula does, so it fits to the same posterior. Edit it freely from there; this model is kept." />
      </p>
      <div className="d-flex flex-wrap gap-2 align-items-end">
        <Form.Group controlId="convert-store">
          <Form.Label className="small mb-0"><T text="File store" /></Form.Label>
          <Form.Select size="sm" value={store} onChange={(e) => setStore(e.target.value)}>
            {(stores ?? []).map((s) => (
              <option key={s} value={s}>{s}</option>
            ))}
          </Form.Select>
        </Form.Group>
        <Form.Group controlId="convert-path">
          <Form.Label className="small mb-0"><T text="Path" /></Form.Label>
          <Form.Control
            size="sm"
            className="font-monospace"
            value={path}
            onChange={(e) => setPath(e.target.value)}
          />
        </Form.Group>
        <Button size="sm" variant="outline-primary" disabled={busy || !store || !path} onClick={convert}>
          {busy ? t("Converting…") : t("Convert")}
        </Button>
      </div>
      {stores && stores.length === 0 && (
        <div className="text-muted small mt-1"><T text="No file store is connected." /></div>
      )}
      {error && <div className="text-danger small mt-1">{error}</div>}
    </details>
  );
}

/** The resolved prior table (§7): every parameter, defaults and inherited
 * rows greyed, explicit rows in full with a ✕ that clears them. */
function PriorTable({
  table,
  rows,
  center,
  onChange,
  highlighted,
}: {
  table: TableRow[];
  rows: PriorRow[];
  center: boolean;
  onChange: (rows: PriorRow[]) => void;
  highlighted: (row: TableRow) => boolean;
}) {
  const { t } = useT();
  const anyBounds = table.some((r) => r.class === "b");
  return (
    <Table size="sm" className="mb-0 align-middle">
      <thead>
        <tr>
          <th><T text="class" /></th>
          <th><T text="coef" /></th>
          <th><T text="group" /></th>
          <th><T text="dpar" /></th>
          <th><T text="prior" /></th>
          {anyBounds && <th><T text="lb" /></th>}
          {anyBounds && <th><T text="ub" /></th>}
          <th><T text="source" /></th>
          <th />
        </tr>
      </thead>
      <tbody>
        {table.map((r) => (
          <PriorTableRow
            key={`${r.class}|${r.dpar}|${r.group}|${r.coef}|${r.whole_class}`}
            row={r}
            explicit={rows.find((x) => sameTarget(x, r)) ?? null}
            center={center}
            showBounds={anyBounds}
            highlighted={highlighted(r)}
            onSet={(change) => onChange(setRow(rows, r, change))}
            onClear={() => onChange(clearRow(rows, r))}
            clearLabel={t("Clear back to the default")}
          />
        ))}
      </tbody>
    </Table>
  );
}

function PriorTableRow({
  row,
  explicit,
  center,
  showBounds,
  highlighted,
  onSet,
  onClear,
  clearLabel,
}: {
  row: TableRow;
  explicit: PriorRow | null;
  center: boolean;
  showBounds: boolean;
  highlighted: boolean;
  onSet: (change: Partial<PriorRow>) => void;
  onClear: () => void;
  clearLabel: string;
}) {
  const { t } = useT();
  // The text being typed, kept here so an invalid prior is not written to
  // the configuration until it reads.
  const [draft, setDraft] = useState<string | null>(null);
  const value = draft ?? explicit?.prior ?? "";
  const problem = value.trim() === "" ? null : priorProblem(value, row.class, center);
  const grey = greyed(row) && !explicit;
  const bound = (key: "lb" | "ub", text: string) => {
    const x = text.trim() === "" ? undefined : Number(text);
    onSet({ [key]: x !== undefined && Number.isFinite(x) ? x : undefined, prior: explicit?.prior ?? "" });
  };
  return (
    <tr className={highlighted ? "table-info" : undefined} style={grey ? { opacity: 0.6 } : undefined}>
      <td className="font-monospace">{row.class}</td>
      <td className="font-monospace">{coefText(row)}</td>
      <td className="font-monospace">{row.group}</td>
      <td className="font-monospace">{row.dpar}</td>
      <td style={{ minWidth: "13rem" }}>
        <Form.Control
          size="sm"
          className="font-monospace"
          list="formula-prior-names"
          value={value}
          placeholder={row.rule ?? row.prior}
          isInvalid={problem !== null}
          aria-label={t("prior")}
          onChange={(e) => {
            const text = e.target.value;
            setDraft(text);
            if (text.trim() === "" || priorProblem(text, row.class, center) === null) {
              onSet({ prior: text.trim() });
            }
          }}
          onBlur={() => setDraft(null)}
        />
        <datalist id="formula-prior-names">
          {PRIOR_NAMES.map((n) => (
            <option key={n} value={`${n}(`} />
          ))}
        </datalist>
        {problem && <Form.Control.Feedback type="invalid">{problem}</Form.Control.Feedback>}
      </td>
      {showBounds &&
        (["lb", "ub"] as const).map((key) =>
          row.class === "b" ? (
            <td key={key} style={{ width: "6rem" }}>
              <Form.Control
                size="sm"
                type="number"
                value={explicit?.[key] ?? row[key] ?? ""}
                aria-label={key}
                title={t("A bound changes the program: it is part of the parameter's declaration.")}
                onChange={(e) => bound(key, e.target.value)}
              />
            </td>
          ) : (
            <td key={key} />
          ),
        )}
      <td className="small text-muted">
        {sourceText(row)}
        {row.structural && (
          <Badge bg="secondary" className="ms-1" title={t("This prior changes the program, not only its data.")}>
            <T text="structural" />
          </Badge>
        )}
      </td>
      <td>
        {explicit && (
          <Button size="sm" variant="link" className="p-0 text-danger" title={clearLabel} onClick={onClear}>
            ✕
          </Button>
        )}
      </td>
    </tr>
  );
}

/** `b` of `x` in `county` for `sigma`. */
function rowName(r: PriorRow): string {
  let text = r.class;
  if (r.coef) text += ` ${r.coef}`;
  if (r.group) text += ` | ${r.group}`;
  if (r.dpar) text += ` (${r.dpar})`;
  return text;
}

function readLinks(text: string | undefined): Record<string, string> {
  if (!text || text.trim() === "") return {};
  try {
    const parsed: unknown = JSON.parse(text);
    return parsed && typeof parsed === "object" ? (parsed as Record<string, string>) : {};
  } catch {
    return {};
  }
}

function writeLinks(links: Record<string, string>): string {
  const set = Object.fromEntries(Object.entries(links).filter(([, l]) => l !== ""));
  return Object.keys(set).length === 0 ? "" : JSON.stringify(set);
}
