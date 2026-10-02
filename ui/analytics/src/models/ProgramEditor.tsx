// A model's program, edited in place (analytics TODO A3.6): the `.stan` file
// the model names, read from its file store into an editor beside the
// bindings, and written back with **Save program** — after which the program's
// interface is read again, so the binding table follows what was just typed.
//
// The program still lives in the file store and nowhere else (Stan TODO §18):
// versioned with the store, opened in the IDE for anything bigger than an edit
// (the "Open in IDE" button beside this), and copied into each fit that runs
// it. This is a window onto that file, not a second copy.
//
// The editor is Monaco, loaded on demand as the admin UI's `CodeEditor` loads
// it — a chunk fetched when a program is first shown, its one worker a
// same-origin script the strict CSP allows — with a small Stan grammar for
// highlighting. Should the chunk not load, the program is a text area.

import { useEffect, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";

import { api, errorMessage } from "../api";
import { T, useT } from "../i18n";

type Monaco = typeof import("monaco-editor");
type Editor = import("monaco-editor").editor.IStandaloneCodeEditor;

/** Stan's block names, types and statements, for highlighting. */
export const STAN_KEYWORDS = [
  "functions",
  "data",
  "transformed",
  "parameters",
  "model",
  "generated",
  "quantities",
  "int",
  "real",
  "complex",
  "vector",
  "row_vector",
  "matrix",
  "array",
  "tuple",
  "simplex",
  "ordered",
  "positive_ordered",
  "unit_vector",
  "sum_to_zero_vector",
  "cholesky_factor_corr",
  "cholesky_factor_cov",
  "corr_matrix",
  "cov_matrix",
  "lower",
  "upper",
  "offset",
  "multiplier",
  "for",
  "in",
  "while",
  "if",
  "else",
  "return",
  "break",
  "continue",
  "target",
  "print",
  "reject",
  "fatal_error",
  "void",
];

/** Monaco with the Stan grammar registered — one chunk, loaded once. */
async function loadMonaco(): Promise<Monaco> {
  const [monaco, editorWorker] = await Promise.all([
    import("monaco-editor/esm/vs/editor/editor.api"),
    import("monaco-editor/esm/vs/editor/editor.worker?worker"),
    import("monaco-editor/esm/vs/editor/editor.all.js"),
  ]);
  self.MonacoEnvironment = {
    getWorker() {
      return new editorWorker.default();
    },
  };
  if (!monaco.languages.getLanguages().some((l) => l.id === "stan")) {
    monaco.languages.register({ id: "stan", extensions: [".stan"] });
    monaco.languages.setMonarchTokensProvider("stan", {
      keywords: STAN_KEYWORDS,
      tokenizer: {
        root: [
          [/\/\/.*$/, "comment"],
          [/#.*$/, "comment"],
          [/\/\*/, "comment", "@comment"],
          [/"[^"]*"/, "string"],
          [/\d+(\.\d*)?([eE][-+]?\d+)?/, "number"],
          [/~/, "operator"],
          [/[a-zA-Z_]\w*/, { cases: { "@keywords": "keyword", "@default": "identifier" } }],
        ],
        comment: [
          [/[^/*]+/, "comment"],
          [/\*\//, "comment", "@pop"],
          [/[/*]/, "comment"],
        ],
      },
    });
    monaco.languages.setLanguageConfiguration("stan", {
      comments: { lineComment: "//", blockComment: ["/*", "*/"] },
      brackets: [
        ["{", "}"],
        ["[", "]"],
        ["(", ")"],
      ],
      autoClosingPairs: [
        { open: "{", close: "}" },
        { open: "[", close: "]" },
        { open: "(", close: ")" },
      ],
    });
  }
  return monaco;
}

let monacoPromise: Promise<Monaco> | null = null;
function monacoModule(): Promise<Monaco> {
  monacoPromise ??= loadMonaco();
  return monacoPromise;
}

/** Monaco's theme for the colour scheme in force. */
function monacoTheme(): string {
  return document.documentElement.getAttribute("data-bs-theme") === "dark" ? "vs-dark" : "vs";
}

export function ProgramEditor({
  store,
  path,
  onSaved,
}: {
  /** The file store the program is in. */
  store: string;
  /** Its path in the store. */
  path: string;
  /** The program was written: read its interface again. */
  onSaved: () => void;
}) {
  const { t } = useT();
  const container = useRef<HTMLDivElement>(null);
  const editor = useRef<Editor | null>(null);
  const [text, setText] = useState<string | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const [missing, setMissing] = useState(false);
  const [failed, setFailed] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // The file, again whenever the store or the path changes.
  useEffect(() => {
    let cancelled = false;
    setError(null);
    setMissing(false);
    setText(null);
    void api
      .readFile(store, { path })
      .then((file) => {
        if (cancelled) return;
        const body = file.text ?? "";
        setText(body);
        setSaved(body);
      })
      .catch(() => {
        if (cancelled) return;
        // Not there yet: a new program, written from here.
        setMissing(true);
        setText("");
        setSaved(null);
      });
    return () => {
      cancelled = true;
    };
  }, [store, path]);

  const ready = text !== null;
  useEffect(() => {
    if (!ready || failed) return undefined;
    let disposed = false;
    let observer: MutationObserver | null = null;
    void monacoModule()
      .then((monaco) => {
        if (disposed || !container.current) return;
        const instance = monaco.editor.create(container.current, {
          value: text ?? "",
          language: "stan",
          theme: monacoTheme(),
          automaticLayout: true,
          minimap: { enabled: false },
          scrollBeyondLastLine: false,
          tabSize: 2,
          insertSpaces: true,
          fontSize: 13,
          scrollbar: { alwaysConsumeMouseWheel: false },
        });
        editor.current = instance;
        instance.onDidChangeModelContent(() => setText(instance.getValue()));
        observer = new MutationObserver(() => monaco.editor.setTheme(monacoTheme()));
        observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-bs-theme"] });
      })
      .catch(() => {
        if (!disposed) setFailed(true);
      });
    return () => {
      disposed = true;
      observer?.disconnect();
      editor.current?.getModel()?.dispose();
      editor.current?.dispose();
      editor.current = null;
    };
    // Built once per file; typing changes `text`, which must not rebuild it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ready, failed, store, path]);

  const save = async () => {
    if (text === null) return;
    setBusy(true);
    setError(null);
    try {
      await api.writeFile(store, { path, text });
      setSaved(text);
      setMissing(false);
      onSaved();
    } catch (err) {
      setError(errorMessage(err, t("Could not save the program.")));
    } finally {
      setBusy(false);
    }
  };

  const changed = text !== null && text !== saved;
  return (
    <div className="mt-3" data-program-editor>
      {missing && (
        <p className="text-secondary small">
          <T text="There is no {path} in {store} yet: what is written here is saved as it." args={{ path, store }} />
        </p>
      )}
      {failed ? (
        <Form.Control
          as="textarea"
          rows={16}
          className="font-monospace small"
          aria-label={t("Program")}
          value={text ?? ""}
          onChange={(e) => setText(e.target.value)}
        />
      ) : (
        <div ref={container} className="an-program-editor" aria-label={t("Program")} />
      )}
      <div className="d-flex align-items-center gap-2 mt-2">
        <Button size="sm" variant={changed ? "primary" : "outline-secondary"} disabled={!changed || busy} onClick={() => void save()}>
          {busy ? <T text="Saving…" /> : <T text="Save program" />}
        </Button>
        <span className="text-secondary small">
          {changed ? (
            <T text="Unsaved: a fit runs the program as it is in the file store." />
          ) : (
            <T text="Saved in the file store." />
          )}
        </span>
      </div>
      {error && (
        <Alert variant="danger" className="mt-2 mb-0">
          {error}
        </Alert>
      )}
    </div>
  );
}
