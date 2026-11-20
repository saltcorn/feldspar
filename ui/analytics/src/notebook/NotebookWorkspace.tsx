// The Notebook workspace (analytics TODO A10.7–A10.8; the goals document's
// "Workspaces"): a Jupyter-style list of text and code cells in the language
// the notebook was made in — JavaScript, Python, or LLM prompts.
//
// The browser edits the cells and saves the state as every workspace does
// (the frame's saver); running is the server's. A run first saves what is
// pending — the server runs the cell as it is stored — then asks for it
// (`runNotebookCell`, Shift+Enter or ▶), and takes the outputs and the run the
// server stored (`withRun`). **Run all**, **Interrupt** and **Restart** act on
// the session, whose status arrives on its socket: which cell is running, the
// fit it waits for (drawn as the model editor draws a running fit), and when
// the session started, so outputs from before it — the notebook reopened, or
// restarted — are marked "not run in this session".
//
// Outputs are drawn by the panel renderers. A panel output is a drag source
// (A4.3): dragged into a report or a dashboard it is a copy, carrying its
// non-persisted dataset inside its spec. A dataset output offers **Save as
// dataset** — it is stored under a name, the notebook's references to it now
// name it, and it opens in the Dataset editor — and **Open in explorer**.

import { useCallback, useEffect, useMemo, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";
import { FitRunning } from "../models/FitRunning";
import { useFitProgress } from "../models/progress";
import { useAnnounce, usePane } from "../panes";
import { Markdown } from "../panels/MarkdownView";
import { DragHandle, PanelView } from "../panels/PanelView";
import { copyPanel } from "../panels/panel";
import type { DataRef } from "../plot/spec";
import type { WorkspaceProps } from "../workspaces/WorkspaceFrame";
import { CellEditor, type EditorLanguage, type Names } from "./CellEditor";
import { readSession, useNotebookSession } from "./session";
import {
  addCell,
  cellCalls,
  cellError,
  datasetsNamed,
  deleteCell,
  inlineId,
  moveCell,
  nameInline,
  ranInSession,
  readNotebook,
  setKind,
  setSource,
  toState,
  withRun,
  type Cell,
  type CellKind,
  type Language,
  type NotebookState,
  type Output,
  type SessionStatus,
} from "./state";

/** What a dataset output's Save is for: the Dataset editor, or the explorer. */
type SaveFor = { data: DataRef; then: "edit" | "explore" };

export function NotebookWorkspace({ id, state, setState, readOnly, flush }: WorkspaceProps) {
  const { t } = useT();
  const pane = usePane();
  const announce = useAnnounce();
  const nb = useMemo(() => readNotebook(state), [state]);
  const update = useCallback(
    (change: (nb: NotebookState) => NotebookState) => setState((s) => toState(change(readNotebook(s)))),
    [setState],
  );
  const [session, setSession] = useNotebookSession(id);
  /** Cells asked to run that have not answered, in the order asked. */
  const [waiting, setWaiting] = useState<string[]>([]);
  /** Why asking to run a cell failed (not why the cell failed: that is an output). */
  const [refused, setRefused] = useState<Record<string, string>>({});
  const [focus, setFocus] = useState<string | null>(null);
  const [busy, setBusy] = useState<"all" | "interrupt" | "restart" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState<SaveFor | null>(null);
  const names = useNames(nb);

  const run = async (cell: Cell, regenerate = false) => {
    if (!id || readOnly || cell.kind !== "code") return;
    setWaiting((w) => [...w.filter((c) => c !== cell.id), cell.id]);
    setRefused(({ [cell.id]: _, ...rest }) => rest);
    try {
      await flush?.();
      const answer = await api.runNotebookCell(id, cell.id, { source: cell.source, regenerate });
      update((n) => withRun(n, answer.cell));
      setSession(readSession(answer.session));
    } catch (err) {
      setRefused((r) => ({ ...r, [cell.id]: errorMessage(err, t("Could not run this cell.")) }));
    } finally {
      setWaiting((w) => w.filter((c) => c !== cell.id));
    }
  };

  /** Shift+Enter: run the cell and go to the next, made when there is none. */
  const runAndNext = (cell: Cell) => {
    if (cell.kind === "code") void run(cell);
    const at = nb.cells.findIndex((c) => c.id === cell.id);
    const next = nb.cells[at + 1];
    if (next) setFocus(next.id);
    else if (!readOnly) {
      const made = addCell(nb, "code", cell.id);
      update(() => made.nb);
      setFocus(made.id);
    }
  };

  const runAll = async () => {
    if (!id) return;
    setBusy("all");
    setError(null);
    try {
      await flush?.();
      const answer = await api.runNotebook(id);
      update((n) => answer.cells.reduce<NotebookState>((acc, c) => withRun(acc, c), n));
      setSession(readSession(answer.session));
    } catch (err) {
      setError(errorMessage(err, t("Could not run the notebook.")));
    } finally {
      setBusy(null);
    }
  };

  const interrupt = async () => {
    if (!id) return;
    setBusy("interrupt");
    try {
      setSession(readSession(await api.interruptNotebook(id)));
    } catch (err) {
      setError(errorMessage(err, t("Could not interrupt the cell.")));
    } finally {
      setBusy(null);
    }
  };

  const restart = async () => {
    if (!id || !window.confirm(t("Restart the session? Everything the cells defined is forgotten; their outputs stay."))) {
      return;
    }
    setBusy("restart");
    try {
      setSession(readSession(await api.restartNotebook(id)));
    } catch (err) {
      setError(errorMessage(err, t("Could not restart the session.")));
    } finally {
      setBusy(null);
    }
  };

  const add = (kind: CellKind, after?: string) => {
    const made = addCell(nb, kind, after);
    update(() => made.nb);
    setFocus(made.id);
  };

  const running = session.state === "running" || waiting.length > 0 || busy === "all";
  return (
    <div className="an-nb h-100 overflow-auto" data-notebook={nb.language}>
      <div className="an-nb-toolbar d-flex flex-wrap align-items-center gap-2 px-3 py-2 border-bottom">
        <Badge bg="secondary-lt" title={t("A notebook's language is chosen when it is made")}>
          {languageName(nb.language, t)}
        </Badge>
        {!readOnly && (
          <>
            <Button size="sm" variant="outline-primary" disabled={busy !== null || nb.cells.length === 0} onClick={() => void runAll()}>
              {busy === "all" ? <Spinner animation="border" size="sm" /> : "▶▶"} <T text="Run all" />
            </Button>
            <Button size="sm" variant="outline-danger" disabled={!running || busy === "interrupt"} onClick={() => void interrupt()}>
              ■ <T text="Interrupt" />
            </Button>
            <Button size="sm" variant="outline-secondary" disabled={busy !== null} onClick={() => void restart()}>
              ↻ <T text="Restart" />
            </Button>
          </>
        )}
        <span className="text-secondary small ms-auto" aria-live="polite" data-session={session.state}>
          <SessionText session={session} />
        </span>
      </div>
      {error && (
        <Alert variant="danger" className="m-3" dismissible onClose={() => setError(null)}>
          {error}
        </Alert>
      )}
      <div className="an-nb-cells px-3 py-3">
        {nb.cells.length === 0 && (
          <p className="text-secondary">
            <T text="An empty notebook. Add a cell to begin." />
          </p>
        )}
        {nb.cells.map((cell, i) => (
          <CellView
            key={cell.id}
            cell={cell}
            language={nb.language}
            session={session}
            waiting={waiting.includes(cell.id)}
            refused={refused[cell.id]}
            names={names}
            focus={focus === cell.id}
            readOnly={readOnly}
            first={i === 0}
            last={i === nb.cells.length - 1}
            onSource={(source) => update((n) => setSource(n, cell.id, source))}
            onRun={() => runAndNext(cell)}
            onRunHere={(regenerate) => void run(cell, regenerate)}
            onKind={(kind) => update((n) => setKind(n, cell.id, kind))}
            onMove={(by) => update((n) => moveCell(n, cell.id, by))}
            onDelete={() => update((n) => deleteCell(n, cell.id))}
            onAdd={(kind) => add(kind, cell.id)}
            onSave={(data, then) => setSaving({ data, then })}
            onExplore={(dataset) => void openInExplorer(dataset)}
          />
        ))}
        {!readOnly && (
          <div className="d-flex gap-2 mt-2">
            <Button size="sm" variant="outline-secondary" onClick={() => add("code")}>
              + {nb.language === "llm" ? <T text="Prompt" /> : <T text="Code" />}
            </Button>
            <Button size="sm" variant="outline-secondary" onClick={() => add("text")}>
              + <T text="Text" />
            </Button>
          </div>
        )}
      </div>
      {saving && id && (
        <SaveDatasetDialog
          workspace={id}
          what={saving}
          onClose={() => setSaving(null)}
          onSaved={(dataset) => {
            setSaving(null);
            update((n) => nameInline(n, dataset));
            announce("dataset", dataset);
            if (saving.then === "explore") void openInExplorer(dataset);
            else pane.beside({ name: "dataset", id: dataset });
          }}
        />
      )}
    </div>
  );

  /** A Data explorer on a stored dataset, beside the notebook. */
  async function openInExplorer(dataset: string) {
    try {
      const listed = (await api.listDatasets()).find((d) => d.id === dataset);
      const ws = await api.createWorkspace({
        name: t("{dataset} explorer", { dataset: listed?.name ?? t("Dataset") }),
        kind: "data_explorer",
      });
      await api.saveWorkspaceState(ws.id, { state: { dataset } });
      pane.beside({ name: "workspace", id: ws.id });
    } catch (err) {
      setError(errorMessage(err, t("Could not open the explorer.")));
    }
  }
}

function languageName(language: Language, t: (s: string) => string): string {
  switch (language) {
    case "javascript":
      return "JavaScript";
    case "python":
      return "Python";
    case "llm":
      return t("LLM prompt");
  }
}

function SessionText({ session }: { session: SessionStatus }) {
  if (session.state === "none") return <T text="No session: nothing is defined yet" />;
  if (session.state === "running") {
    return <T text="Running · {runs} run in this session" args={{ runs: session.runs }} />;
  }
  return <T text="Idle · {runs} run in this session" args={{ runs: session.runs }} />;
}

/** The names the cells' completions offer: every dataset and table, and the
 * columns of the ones the cells name. Read once, and again as cells name
 * others. */
function useNames(nb: NotebookState): Names {
  const [datasets, setDatasets] = useState<{ name: string; columns: string[] }[]>([]);
  const [tables, setTables] = useState<string[]>([]);
  const [tableColumns, setTableColumns] = useState<Record<string, string[]>>({});
  useEffect(() => {
    let live = true;
    void api
      .listDatasets()
      .then((ds) => live && setDatasets(ds.map((d) => ({ name: d.name, columns: columnNames(d.columns) }))))
      .catch(() => undefined);
    void api
      .listTables()
      .then((ts) => live && setTables(ts.map((t) => t.name)))
      .catch(() => undefined);
    return () => {
      live = false;
    };
  }, []);
  const named = useMemo(() => [...new Set(nb.cells.flatMap((c) => datasetsNamed(c.source)))], [nb.cells]);
  const namedKey = named.join("\u0000");
  useEffect(() => {
    for (const name of namedKey.split("\u0000")) {
      if (!name || !tables.includes(name) || tableColumns[name]) continue;
      void api
        .listFields(name)
        .then((fields) => setTableColumns((c) => ({ ...c, [name]: fields.map((f) => f.name) })))
        .catch(() => undefined);
    }
    // `tableColumns` is what this fills; reading it again must not loop.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [namedKey, tables]);
  return useMemo(() => {
    const columns = new Set<string>();
    for (const name of named) {
      for (const c of datasets.find((d) => d.name === name)?.columns ?? tableColumns[name] ?? []) columns.add(c);
    }
    return {
      datasets: [...new Set([...datasets.map((d) => d.name), ...tables])],
      tables,
      columns: [...columns],
    };
  }, [named, datasets, tables, tableColumns]);
}

function columnNames(columns: unknown[]): string[] {
  return columns.flatMap((c) =>
    typeof c === "string" ? [c] : c && typeof c === "object" && typeof (c as { name?: unknown }).name === "string" ? [(c as { name: string }).name] : [],
  );
}

// --- one cell ---------------------------------------------------------------------

function CellView({
  cell,
  language,
  session,
  waiting,
  refused,
  names,
  focus,
  readOnly,
  first,
  last,
  onSource,
  onRun,
  onRunHere,
  onKind,
  onMove,
  onDelete,
  onAdd,
  onSave,
  onExplore,
}: {
  cell: Cell;
  language: Language;
  session: SessionStatus;
  waiting: boolean;
  refused?: string;
  names: Names;
  focus: boolean;
  readOnly?: boolean;
  first: boolean;
  last: boolean;
  onSource: (source: string) => void;
  /** Shift+Enter: run, and go to the next cell. */
  onRun: () => void;
  /** ▶ (or Regenerate): run, and stay. */
  onRunHere: (regenerate: boolean) => void;
  onKind: (kind: CellKind) => void;
  onMove: (by: number) => void;
  onDelete: () => void;
  onAdd: (kind: CellKind) => void;
  onSave: (data: DataRef, then: SaveFor["then"]) => void;
  onExplore: (dataset: string) => void;
}) {
  const { t } = useT();
  const [editing, setEditing] = useState(cell.source === "");
  useEffect(() => {
    if (focus && cell.kind === "text") setEditing(true);
  }, [focus, cell.kind]);
  const runningHere = session.state === "running" && session.cell === cell.id;
  const stale = cell.kind === "code" && cell.run !== undefined && !ranInSession(cell, session);
  const error = cellError(cell);
  const calls = cellCalls(cell);
  const editorLanguage: EditorLanguage =
    cell.kind === "text" ? "markdown" : language === "llm" ? "plaintext" : language;

  return (
    <section className={`an-nb-cell an-nb-${cell.kind}${error ? " an-nb-failed" : ""}`} data-cell={cell.id} aria-label={t("Cell")}>
      <div className="an-nb-gutter text-secondary small font-monospace" title={stale ? t("Not run in this session") : undefined}>
        {cell.kind === "code" && (runningHere ? "[*]" : waiting ? "[…]" : cell.run ? `[${cell.run.count}]` : "[ ]")}
      </div>
      <div className="an-nb-body">
        {!readOnly && (
          <div className="an-nb-cell-tools d-flex align-items-center gap-1 small">
            {cell.kind === "code" && (
              <Button size="sm" variant="link" className="p-0 px-1" title={t("Run (Shift+Enter)")} aria-label={t("Run")} disabled={waiting} onClick={() => onRunHere(false)}>
                ▶
              </Button>
            )}
            {cell.kind === "code" && calls && (
              <Button size="sm" variant="link" className="p-0 px-1" disabled={waiting} onClick={() => onRunHere(true)} title={t("Ask the LLM again rather than replaying its calls")}>
                <T text="Regenerate" />
              </Button>
            )}
            <Form.Select
              size="sm"
              className="an-nb-kind w-auto"
              aria-label={t("Cell kind")}
              value={cell.kind}
              onChange={(e) => onKind(e.target.value as CellKind)}
            >
              <option value="code">{language === "llm" ? t("Prompt") : t("Code")}</option>
              <option value="text">{t("Text")}</option>
            </Form.Select>
            <span className="ms-auto" />
            <Button size="sm" variant="link" className="p-0 px-1" disabled={first} onClick={() => onMove(-1)} aria-label={t("Move up")} title={t("Move up")}>
              ↑
            </Button>
            <Button size="sm" variant="link" className="p-0 px-1" disabled={last} onClick={() => onMove(1)} aria-label={t("Move down")} title={t("Move down")}>
              ↓
            </Button>
            <Button size="sm" variant="link" className="p-0 px-1" onClick={() => onAdd("code")} title={t("Add a cell below")}>
              +
            </Button>
            <Button
              size="sm"
              variant="link"
              className="p-0 px-1 text-danger"
              aria-label={t("Delete cell")}
              title={t("Delete cell")}
              onClick={() => (cell.source.trim() === "" || window.confirm(t("Delete this cell?"))) && onDelete()}
            >
              ✕
            </Button>
          </div>
        )}
        {cell.kind === "text" && !editing ? (
          <div
            className="an-nb-markdown an-markdown"
            onDoubleClick={() => !readOnly && setEditing(true)}
            title={readOnly ? undefined : t("Double-click to edit")}
          >
            {cell.source.trim() === "" ? (
              <span className="text-secondary">
                <T text="Empty text" />
              </span>
            ) : (
              <Markdown source={cell.source} />
            )}
          </div>
        ) : (
          <CellEditor
            key={`${cell.kind}-${editorLanguage}`}
            value={cell.source}
            onChange={onSource}
            onRun={() => {
              if (cell.kind === "text") setEditing(false);
              onRun();
            }}
            language={editorLanguage}
            names={names}
            errorLine={error?.line}
            label={cell.kind === "text" ? t("Text") : language === "llm" ? t("Prompt") : t("Code")}
            autoFocus={focus}
            readOnly={readOnly}
          />
        )}
        {cell.kind === "text" && editing && !readOnly && (
          <div className="small text-secondary mt-1">
            <T text="Shift+Enter shows it." />{" "}
            <Button size="sm" variant="link" className="p-0" onClick={() => setEditing(false)}>
              <T text="Done" />
            </Button>
          </div>
        )}
        {refused && (
          <Alert variant="danger" className="mt-2 mb-0 small">
            {refused}
          </Alert>
        )}
        {runningHere && session.fit && <FitProgress fit={session.fit} />}
        {cell.kind === "code" && (cell.outputs?.length ?? 0) > 0 && (
          <div className={`an-nb-outputs${stale ? " an-nb-stale" : ""}`}>
            {stale && (
              <div className="small text-secondary fst-italic mb-1" data-stale>
                <T text="Not run in this session" />
              </div>
            )}
            {(cell.outputs ?? []).map((output, i) => (
              <OutputView
                key={i}
                output={output}
                prose={language === "llm"}
                readOnly={readOnly}
                onSave={onSave}
                onExplore={onExplore}
              />
            ))}
          </div>
        )}
      </div>
    </section>
  );
}

function FitProgress({ fit }: { fit: string }) {
  const live = useFitProgress(fit, () => undefined);
  return (
    <div className="mt-2">
      <FitRunning instance={fit} live={live} />
    </div>
  );
}

// --- outputs ----------------------------------------------------------------------

function OutputView({
  output,
  prose,
  readOnly,
  onSave,
  onExplore,
}: {
  output: Output;
  /** Text is the agent's words, in Markdown (an LLM notebook), not what code printed. */
  prose?: boolean;
  readOnly?: boolean;
  onSave: (data: DataRef, then: SaveFor["then"]) => void;
  onExplore: (dataset: string) => void;
}) {
  const { t } = useT();
  switch (output.kind) {
    case "panel":
      return (
        <div className="an-nb-output an-nb-panel" data-output="panel" data-panel-kind={output.panel.kind}>
          <div className="d-flex align-items-center gap-2 mb-1">
            {output.panel.title && <strong className="small">{output.panel.title}</strong>}
            <span className="ms-auto" />
            <DragHandle make={() => copyPanel(output.panel)} />
          </div>
          <PanelView panel={output.panel} />
        </div>
      );
    case "text":
      if (prose) return <Markdown source={output.text} className="an-nb-output an-markdown" />;
      return (
        <pre className="an-nb-output an-nb-print small mb-1" data-output="text">
          {output.text}
        </pre>
      );
    case "value":
      return (
        <pre className="an-nb-output an-nb-value small mb-1" data-output="value">
          {typeof output.value === "string" ? JSON.stringify(output.value) : JSON.stringify(output.value, null, 2)}
        </pre>
      );
    case "error":
      return (
        <Alert variant="danger" className="an-nb-output small py-2 mb-1" data-output="error">
          {output.line !== undefined && (
            <strong>
              <T text="Line {line}:" args={{ line: output.line }} />{" "}
            </strong>
          )}
          {output.message}
        </Alert>
      );
    case "dataset":
      return <DatasetOutput output={output} readOnly={readOnly} onSave={onSave} onExplore={onExplore} />;
    case "calls":
      return (
        <details className="an-nb-output an-nb-calls small" data-output="calls">
          <summary className="text-secondary">
            {t("The calls the agent made ({n})", { n: output.calls.filter((c) => c.function !== "say").length })}
          </summary>
          <pre className="mb-0">{output.calls.map(callText).join("\n")}</pre>
        </details>
      );
    default:
      return null;
  }
}

/** A recorded call, as a line of code. */
export function callText(call: { function: string; arguments?: unknown }): string {
  return `nb.${call.function}(${call.arguments === undefined ? "" : JSON.stringify(call.arguments)})`;
}

function DatasetOutput({
  output,
  readOnly,
  onSave,
  onExplore,
}: {
  output: Extract<Output, { kind: "dataset" }>;
  readOnly?: boolean;
  onSave: (data: DataRef, then: SaveFor["then"]) => void;
  onExplore: (dataset: string) => void;
}) {
  const { t } = useT();
  const inline = inlineId(output.data);
  const stored = output.data.kind === "dataset" ? output.data.dataset : undefined;
  return (
    <div className="an-nb-output an-nb-dataset" data-output="dataset">
      <div className="d-flex align-items-center gap-2 mb-1 small">
        <span className="text-secondary">
          {output.data.kind === "inline" && output.data.definition.name
            ? `${output.data.definition.name} · `
            : ""}
          {t("{shown} of {total} rows", { shown: output.rows.length, total: output.total })}
        </span>
        <span className="ms-auto" />
        {inline && !readOnly && (
          <Button size="sm" variant="outline-primary" onClick={() => onSave(output.data, "edit")}>
            <T text="Save as dataset" />
          </Button>
        )}
        {(stored || (inline && !readOnly)) && (
          <Button
            size="sm"
            variant="outline-secondary"
            onClick={() => (stored ? onExplore(stored) : onSave(output.data, "explore"))}
            title={stored ? undefined : t("It is saved as a dataset first")}
          >
            <T text="Open in explorer" />
          </Button>
        )}
      </div>
      <div className="an-nb-rows">
        <table className="table table-sm table-striped small mb-0">
          <thead>
            <tr>
              {output.columns.map((c) => (
                <th key={c}>{c}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {output.rows.map((row, i) => (
              <tr key={i}>
                {row.map((v, j) => (
                  <td key={j}>{cellText(v)}</td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}

/** A dataset output's value as text: a fraction to six significant digits. */
export function cellText(v: unknown): string {
  if (v === null || v === undefined) return "";
  if (typeof v === "number" && !Number.isInteger(v)) return String(Number(v.toPrecision(6)));
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}

/** **Save as dataset**: a non-persisted dataset stored under a name. */
function SaveDatasetDialog({
  workspace,
  what,
  onClose,
  onSaved,
}: {
  workspace: string;
  what: SaveFor;
  onClose: () => void;
  onSaved: (dataset: string) => void;
}) {
  const { t } = useT();
  const id = inlineId(what.data);
  const suggested = what.data.kind === "inline" ? what.data.definition.name : "";
  const [name, setName] = useState(suggested ?? "");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const save = async () => {
    if (!id) return;
    setBusy(true);
    setError(null);
    try {
      // The stored dataset keeps the non-persisted one's id.
      await api.saveInlineDataset(workspace, { dataset: id, name: name.trim() });
      onSaved(id);
    } catch (err) {
      setError(errorMessage(err, t("Could not save the dataset.")));
    } finally {
      setBusy(false);
    }
  };
  return (
    <Modal show onHide={onClose}>
      <Form
        onSubmit={(e) => {
          e.preventDefault();
          void save();
        }}
      >
        <Modal.Header closeButton>
          <Modal.Title>
            <T text="Save as dataset" />
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <p className="small text-secondary">
            {what.then === "explore" ? (
              <T text="The explorer opens a stored dataset: this one is saved first, and the notebook then refers to it by name." />
            ) : (
              <T text="It becomes a named dataset, which the notebook then refers to, and opens in the Dataset editor." />
            )}
          </p>
          <Form.Group controlId="nb-dataset-name">
            <Form.Label>
              <T text="Name" />
            </Form.Label>
            <Form.Control autoFocus value={name} onChange={(e) => setName(e.target.value)} />
          </Form.Group>
          {error && (
            <Alert variant="danger" className="mt-3 mb-0">
              {error}
            </Alert>
          )}
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" onClick={onClose}>
            <T text="Cancel" />
          </Button>
          <Button type="submit" disabled={busy || name.trim() === ""}>
            {busy ? <T text="Saving…" /> : <T text="Save" />}
          </Button>
        </Modal.Footer>
      </Form>
    </Modal>
  );
}
