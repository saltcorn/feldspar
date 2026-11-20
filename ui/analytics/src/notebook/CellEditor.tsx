// A notebook cell's editor (analytics TODO A10.7): A3.6's Monaco, grown to its
// text's height, with the notebook's completions — `nb`'s functions, `db`'s,
// the datasets' and tables' names, and the columns of the datasets the cell
// names (`completionsAt`) — and Shift+Enter to run. The line a run stopped on
// is marked. Should Monaco not load, the cell is a text area that runs on
// Shift+Enter the same.

import { useEffect, useRef, useState } from "react";
import Form from "react-bootstrap/Form";

import { monacoModule, monacoTheme } from "../models/ProgramEditor";
import { completionsAt, type Completion, type Language } from "./state";

type Monaco = typeof import("monaco-editor");
type Editor = import("monaco-editor").editor.IStandaloneCodeEditor;

/** The names a cell's completions are drawn from. */
export type Names = { datasets: string[]; tables: string[]; columns: string[] };

/** What a cell's editor is written in: its notebook's language, or Markdown. */
export type EditorLanguage = Exclude<Language, "llm"> | "markdown" | "plaintext";

/** Each open editor's completions, by its model's address: one provider per
 * Monaco language serves every cell's. */
const sources = new Map<string, { language: Exclude<Language, "llm">; names: () => Names }>();
const registered = new Set<string>();

/** Monaco with the notebook's languages and their completions. */
async function notebookMonaco(): Promise<Monaco> {
  const monaco = await monacoModule();
  await Promise.all([
    import("monaco-editor/esm/vs/basic-languages/javascript/javascript.contribution.js"),
    import("monaco-editor/esm/vs/basic-languages/python/python.contribution.js"),
    import("monaco-editor/esm/vs/basic-languages/markdown/markdown.contribution.js"),
  ]);
  for (const id of ["javascript", "python"]) {
    if (registered.has(id)) continue;
    registered.add(id);
    monaco.languages.registerCompletionItemProvider(id, {
      triggerCharacters: [".", '"', "'"],
      provideCompletionItems(model, position) {
        const source = sources.get(model.uri.toString());
        if (!source) return { suggestions: [] };
        const before = model.getValueInRange({
          startLineNumber: position.lineNumber,
          startColumn: 1,
          endLineNumber: position.lineNumber,
          endColumn: position.column,
        });
        const word = model.getWordUntilPosition(position);
        const range = {
          startLineNumber: position.lineNumber,
          endLineNumber: position.lineNumber,
          startColumn: word.startColumn,
          endColumn: word.endColumn,
        };
        const kinds: Record<Completion["kind"], number> = {
          function: monaco.languages.CompletionItemKind.Function,
          method: monaco.languages.CompletionItemKind.Method,
          dataset: monaco.languages.CompletionItemKind.Module,
          column: monaco.languages.CompletionItemKind.Field,
        };
        return {
          suggestions: completionsAt(before, source.language, source.names()).map((c) => ({
            label: c.label,
            insertText: c.insert,
            insertTextRules: monaco.languages.CompletionItemInsertTextRule.InsertAsSnippet,
            detail: c.detail,
            kind: kinds[c.kind],
            range,
          })),
        };
      },
    });
  }
  return monaco;
}

/** The fewest and most lines a cell's editor shows before it scrolls. */
const MIN_LINES = 2;
const MAX_LINES = 40;
const LINE_PX = 19;
const SCROLLBAR_PX = 10;

export function CellEditor({
  value,
  onChange,
  onRun,
  language,
  names,
  errorLine,
  label,
  autoFocus,
  readOnly,
}: {
  value: string;
  onChange: (source: string) => void;
  /** Shift+Enter. */
  onRun?: () => void;
  language: EditorLanguage;
  names?: Names;
  /** The line the last run stopped on, from 1. */
  errorLine?: number;
  label: string;
  autoFocus?: boolean;
  readOnly?: boolean;
}) {
  const container = useRef<HTMLDivElement>(null);
  const editor = useRef<Editor | null>(null);
  const monacoRef = useRef<Monaco | null>(null);
  const decorations = useRef<import("monaco-editor").editor.IEditorDecorationsCollection | null>(null);
  const [failed, setFailed] = useState(false);
  // What the handlers read, kept current without rebuilding the editor.
  const latest = useRef({ onChange, onRun, names });
  latest.current = { onChange, onRun, names };

  useEffect(() => {
    if (failed) return undefined;
    let disposed = false;
    let observer: MutationObserver | null = null;
    void notebookMonaco()
      .then((monaco) => {
        if (disposed || !container.current) return;
        monacoRef.current = monaco;
        const instance = monaco.editor.create(container.current, {
          value,
          language,
          theme: monacoTheme(),
          automaticLayout: true,
          minimap: { enabled: false },
          scrollBeyondLastLine: false,
          lineNumbers: language === "markdown" || language === "plaintext" ? "off" : "on",
          wordWrap: language === "markdown" || language === "plaintext" ? "on" : "off",
          wordBasedSuggestions: "off",
          tabSize: language === "python" ? 4 : 2,
          insertSpaces: true,
          fontSize: 13,
          readOnly,
          renderLineHighlight: "none",
          overviewRulerLanes: 0,
          scrollbar: { alwaysConsumeMouseWheel: false, vertical: "auto", horizontalScrollbarSize: SCROLLBAR_PX },
          ariaLabel: label,
        });
        editor.current = instance;
        const model = instance.getModel();
        if (model && (language === "javascript" || language === "python")) {
          sources.set(model.uri.toString(), {
            language,
            names: () => latest.current.names ?? { datasets: [], tables: [], columns: [] },
          });
        }
        instance.onDidChangeModelContent(() => latest.current.onChange(instance.getValue()));
        instance.addCommand(monaco.KeyMod.Shift | monaco.KeyCode.Enter, () => latest.current.onRun?.());
        const fit = () => {
          if (!container.current) return;
          const lines = Math.min(MAX_LINES, Math.max(MIN_LINES, instance.getModel()?.getLineCount() ?? 1));
          // Room below the last line for the horizontal scrollbar a long line brings.
          const height = Math.max(lines * LINE_PX, Math.min(instance.getContentHeight(), MAX_LINES * LINE_PX)) + SCROLLBAR_PX + 4;
          container.current.style.height = `${height}px`;
        };
        instance.onDidContentSizeChange(fit);
        fit();
        decorations.current = instance.createDecorationsCollection();
        if (autoFocus) instance.focus();
        observer = new MutationObserver(() => monaco.editor.setTheme(monacoTheme()));
        observer.observe(document.documentElement, { attributes: true, attributeFilter: ["data-bs-theme"] });
      })
      .catch(() => {
        if (!disposed) setFailed(true);
      });
    return () => {
      disposed = true;
      observer?.disconnect();
      const model = editor.current?.getModel();
      if (model) sources.delete(model.uri.toString());
      model?.dispose();
      editor.current?.dispose();
      editor.current = null;
    };
    // Built once per language; typing changes `value`, which must not rebuild it.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [failed, language]);

  // A change from elsewhere — a Save as dataset, a reload — reaches the editor.
  useEffect(() => {
    const instance = editor.current;
    if (instance && instance.getValue() !== value) instance.setValue(value);
  }, [value]);

  useEffect(() => {
    editor.current?.updateOptions({ readOnly });
  }, [readOnly]);

  useEffect(() => {
    const monaco = monacoRef.current;
    const collection = decorations.current;
    if (!monaco || !collection) return;
    collection.set(
      errorLine
        ? [
            {
              range: new monaco.Range(errorLine, 1, errorLine, 1),
              options: { isWholeLine: true, className: "an-nb-error-line", linesDecorationsClassName: "an-nb-error-gutter" },
            },
          ]
        : [],
    );
  }, [errorLine]);

  if (failed) {
    return (
      <Form.Control
        as="textarea"
        rows={Math.min(MAX_LINES, Math.max(MIN_LINES, value.split("\n").length))}
        className="font-monospace small"
        aria-label={label}
        value={value}
        readOnly={readOnly}
        autoFocus={autoFocus}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && e.shiftKey) {
            e.preventDefault();
            onRun?.();
          }
        }}
      />
    );
  }
  return <div ref={container} className="an-nb-editor" aria-label={label} data-cell-editor />;
}
