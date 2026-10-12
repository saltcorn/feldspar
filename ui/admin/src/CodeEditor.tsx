// The code editor a `code` setting is edited in: Monaco, with the sandbox's own
// types loaded.
//
// A `run_js_code` or `run_python_code` body is a program — it declares things,
// loops, branches, and reads and writes tables — and a one-line input or a bare
// text area is the wrong instrument for one. So a setting whose declaration
// names a language (`FormField::code`) gets the editor VS Code is built on, with
// the two things that make writing against `db` possible without the reference
// open in another tab: **highlighting**, and **completions** over this server's
// own tables (`codeTypes.ts` builds the declarations).
//
// **Two languages, and they get different amounts of help** (§12 of the Python
// milestone). Both get a grammar. Only JavaScript gets the completions, because
// the declarations are TypeScript and the thing that answers a question about
// them is the TypeScript worker; the Python counterpart is a generated
// `saltcorn.pyi` and a language server, and neither exists yet. So a Python body
// gets highlighting, its own indentation, and no worker started on its behalf —
// which is honest rather than degraded: what is missing is completion, and
// nothing here pretends to offer it.
//
// Three decisions worth stating:
//
// - **Plain `monaco-editor`, not the workbench.** `ui/ide` embeds VS Code itself
//   (`@codingame/monaco-vscode-api`, §12.1) because a project needs a tree, tabs
//   and a palette. A settings field needs an editor, and the editor is the small
//   half of that package. They are separate bundles, so the two do not meet.
// - **Loaded on demand.** The import is dynamic, so Monaco is a chunk the admin
//   UI fetches when someone opens a code setting, and never on the way to any
//   other screen. If that fetch fails the field falls back to a text area: an
//   admin who cannot load an editor must still be able to fix a trigger.
// - **No diagnostics.** A body is the *inside* of a function — top-level
//   `return` is what the action's docs tell an admin to write — so the parser's
//   "return outside a function" is a false report, and the sandbox is not the
//   browser, so half of what a type-checker would flag is not knowable here
//   anyway. Completions, hovers and signature help are what the types are for;
//   errors come from the Run button, which really runs it.

import { useEffect, useRef, useState } from "react";
import Form from "react-bootstrap/Form";

import { catalog, codeLibrary, moduleFunctions, type CodeScope } from "./codeTypes";

/** The subset of Monaco this file uses, named so the dynamic import has a type
 * without pulling the package into the main bundle's type graph. */
type Monaco = typeof import("monaco-editor");
type Editor = import("monaco-editor").editor.IStandaloneCodeEditor;

/** Monaco, its language contributions and its workers — one chunk, loaded the
 * first time a code setting is opened.
 *
 * The workers are Vite `?worker` imports: they are built as same-origin scripts,
 * which is what the admin CSP allows (`default-src 'self'`, no `blob:`). The
 * language contributions are imported one by one rather than through
 * `monaco-editor`'s barrel, which would bundle every language it ships. */
async function loadMonaco(): Promise<Monaco> {
  const [monaco, editorWorker, tsWorker] = await Promise.all([
    import("monaco-editor/esm/vs/editor/editor.api"),
    import("monaco-editor/esm/vs/editor/editor.worker?worker"),
    import("monaco-editor/esm/vs/language/typescript/ts.worker?worker"),
    import("monaco-editor/esm/vs/editor/editor.all.js"),
    import("monaco-editor/esm/vs/language/typescript/monaco.contribution"),
    import("monaco-editor/esm/vs/basic-languages/javascript/javascript.contribution"),
    // Python's grammar, beside JavaScript's and for the same reason: a
    // `run_python_code` body is a program. It is a *basic language* — a
    // tokenizer and the bracket and indentation rules — with no worker and no
    // analysis behind it, which is exactly the difference `editorSettings`
    // below describes.
    import("monaco-editor/esm/vs/basic-languages/python/python.contribution"),
  ]);
  self.MonacoEnvironment = {
    getWorker(_workerId: string, label: string) {
      return label === "typescript" || label === "javascript"
        ? new tsWorker.default()
        : new editorWorker.default();
    },
  };
  const ts = monaco.languages.typescript;
  ts.javascriptDefaults.setCompilerOptions({
    target: ts.ScriptTarget.ESNext,
    // The sandbox is not a browser: no `document`, no `fetch`, no timers. The
    // ES library alone is what it really has, so it is the only one loaded —
    // a completion for something that is not there is worse than no completion.
    lib: ["es2022"],
    allowJs: true,
    allowNonTsExtensions: true,
  });
  ts.javascriptDefaults.setDiagnosticsOptions({
    noSemanticValidation: true,
    noSyntaxValidation: true,
  });
  return monaco;
}

/** Start the TypeScript worker and hand it the model, without waiting for a
 * question.
 *
 * The worker is a 6 MB script that builds a program before it can answer
 * anything, and nothing starts it until something asks — so the first `.` an
 * admin types is also the request that pays for all of that, and the suggest
 * widget shows the word-based guesses in the meantime. Asking for nothing here
 * moves that wait to the moment the editor appears, which nobody is watching.
 * The declarations that arrive later are pushed to the running worker by Monaco
 * itself (`onDidExtraLibsChange`), so warming early does not mean warming it
 * without them. */
function warmLanguageService(monaco: Monaco, editor: Editor): void {
  const model = editor.getModel();
  if (!model) return;
  void monaco.languages.typescript
    .getJavaScriptWorker()
    .then((worker) => worker(model.uri))
    // A worker that will not start costs the wait, not the editor.
    .catch(() => undefined);
}

/** Monaco, loaded once per page however many code settings a form has. */
let monacoOnce: Promise<Monaco> | null = null;
function monacoModule(): Promise<Monaco> {
  monacoOnce ??= loadMonaco();
  return monacoOnce;
}

/** What a language a `code` setting declared means to the editor.
 *
 * `id` is Monaco's language id, `tabSize` the indentation the language's own
 * community writes (and, in Python, the indentation the parser reads), and
 * `typed` whether the TypeScript worker has anything to say about a body in it —
 * which decides both whether that 6 MB worker is started and whether the
 * catalog is fetched to feed it.
 *
 * A language this SPA has no grammar for is **plaintext** rather than its own
 * name: an unregistered id colours nothing either way, and this one at least
 * cannot claim highlighting it does not have. */
export function editorSettings(language: string): {
  id: string;
  tabSize: number;
  typed: boolean;
} {
  if (language === "javascript") return { id: "javascript", tabSize: 2, typed: true };
  if (language === "python") return { id: "python", tabSize: 4, typed: false };
  return { id: "plaintext", tabSize: 2, typed: false };
}

/** The colour scheme the page is in, read from the attribute `useTheme` sets. */
function monacoTheme(): string {
  return document.documentElement.getAttribute("data-bs-theme") === "dark"
    ? "vs-dark"
    : "vs";
}

/**
 * A code setting's editor.
 *
 * `value`/`onChange` are the ordinary controlled-input contract, with the one
 * qualification an editor forces: the model is the thing being typed into, so a
 * `value` that arrives from outside (the stored trigger, loaded after this
 * mounted) is written into the model, while a `value` that is merely this
 * editor's own last keystroke coming back is not — that would move the cursor to
 * the end on every character.
 */
export function CodeEditor({
  value,
  language,
  scope,
  onChange,
  readOnly = false,
  id,
}: {
  value: string;
  /** The language the setting declared (`"javascript"`). */
  language: string;
  /** The event the code will run in, which decides what is declared. */
  scope?: CodeScope;
  onChange: (value: string) => void;
  readOnly?: boolean;
  id?: string;
}) {
  const container = useRef<HTMLDivElement | null>(null);
  const editor = useRef<Editor | null>(null);
  // The callback the model listener calls, held in a ref so a re-render with a
  // new closure does not mean tearing the editor down and building it again.
  const change = useRef(onChange);
  change.current = onChange;
  const [failed, setFailed] = useState(false);

  // The declarations, as a string. `null` until the catalog has been read; the
  // editor opens without waiting for it, because a body is worth typing into
  // before the completions arrive.
  const [library, setLibrary] = useState<string | null>(null);
  const table = scope?.table;
  const event = scope?.event;
  const request = scope?.request;
  const bytes = scope?.bytes;
  const settings = editorSettings(language);
  const typed = settings.typed;
  useEffect(() => {
    // Nothing to declare to a language whose service is not TypeScript's, so a
    // Python body does not read the catalog at all.
    if (!typed) return;
    let cancelled = false;
    // The tables and the module functions together: both are cached per page,
    // and a body is worth typing into before either arrives.
    void Promise.all([catalog(), moduleFunctions()])
      .then(([tables, functions]) => {
        if (!cancelled) setLibrary(codeLibrary(tables, { table, event, request, bytes }, functions));
      })
      // A catalog that cannot be read costs completions, not the editor.
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [table, event, request, bytes, typed]);

  useEffect(() => {
    let disposed = false;
    let observer: MutationObserver | null = null;
    void monacoModule()
      .then((monaco) => {
        if (disposed || !container.current) return;
        const instance = monaco.editor.create(container.current, {
          value,
          language: settings.id,
          theme: monacoTheme(),
          readOnly,
          automaticLayout: true,
          minimap: { enabled: false },
          scrollBeyondLastLine: false,
          // Python's indentation is syntax, so a body written at two spaces in
          // an editor that inserted four would be a body that does not parse.
          tabSize: settings.tabSize,
          insertSpaces: true,
          fontSize: 13,
          // A settings form scrolls; an editor that swallowed the page's scroll
          // when the pointer crossed it would trap it.
          scrollbar: { alwaysConsumeMouseWheel: false },
        });
        editor.current = instance;
        if (settings.typed) warmLanguageService(monaco, instance);
        instance.onDidChangeModelContent(() => {
          change.current(instance.getValue());
        });
        // The theme toggle sets `data-bs-theme` on the root element (`useTheme`),
        // which nothing tells this editor about — so it watches for it.
        observer = new MutationObserver(() => monaco.editor.setTheme(monacoTheme()));
        observer.observe(document.documentElement, {
          attributes: true,
          attributeFilter: ["data-bs-theme"],
        });
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
    // Built once. `value` is synced by the effect below, and `language`,
    // `readOnly` and the theme are all changed in place.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // A value from outside — the stored code, arriving after the fetch resolves.
  useEffect(() => {
    const instance = editor.current;
    if (instance && instance.getValue() !== value) instance.setValue(value);
  }, [value]);

  useEffect(() => {
    editor.current?.updateOptions({ readOnly });
  }, [readOnly]);

  // The declarations, published to the language service as an ambient library.
  // Keyed by a stable path so a later catalog read replaces the earlier one
  // instead of declaring `db` twice.
  useEffect(() => {
    if (library === null) return;
    let cancelled = false;
    void monacoModule()
      .then((monaco) => {
        if (cancelled) return;
        monaco.languages.typescript.javascriptDefaults.setExtraLibs([
          { content: library, filePath: "ts:saltcorn/sandbox.d.ts" },
        ]);
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [library]);

  if (failed) {
    // No editor: the value still has to be editable, so it degrades to what a
    // multi-line setting would have been all along.
    return (
      <Form.Control
        as="textarea"
        rows={12}
        id={id}
        className="font-monospace small"
        value={value}
        readOnly={readOnly}
        onChange={(e) => onChange(e.target.value)}
      />
    );
  }

  return (
    <div
      id={id}
      ref={container}
      className="border rounded"
      style={{ height: "22rem", overflow: "hidden" }}
    />
  );
}
