// What the model editor keeps in a model's **view state** (analytics TODO
// A3.4, A3.5): which outputs are collapsed, the optional plots chosen from
// "More plots", and the fit being looked at — so the model reopens as it was
// left. The view state is a dictionary beside the model that nothing about
// fitting or prediction reads; each key is written on its own with
// `patchModelViewState`, so another screen keeping other keys (a comparison,
// a split view's other side) is never overwritten.

/** The editor's three keys in the view state. */
export const VIEW_KEYS = {
  collapsed: "editor_collapsed",
  plots: "editor_plots",
  fit: "editor_fit",
} as const;

/** What the editor reads back. */
export type EditorView = {
  /** The outputs shown folded, by name. */
  collapsed: string[];
  /** The optional plots shown, by name, in the order they were chosen. */
  plots: string[];
  /** The fit selected, or `null` for the model's own choice (the active fit,
   * else the newest fitted). */
  fit: string | null;
};

function names(raw: unknown): string[] {
  return Array.isArray(raw) ? raw.filter((v): v is string => typeof v === "string") : [];
}

/** The editor's part of a model's view state, whatever else is in it. */
export function readEditorView(raw: unknown): EditorView {
  const state = raw && typeof raw === "object" ? (raw as Record<string, unknown>) : {};
  const fit = state[VIEW_KEYS.fit];
  return {
    collapsed: names(state[VIEW_KEYS.collapsed]),
    plots: names(state[VIEW_KEYS.plots]),
    fit: typeof fit === "string" && fit !== "" ? fit : null,
  };
}

/** The patch that records `change`: each key changed, an empty list or no fit
 * removing its key rather than storing an empty value. */
export function editorPatch(change: Partial<EditorView>): Record<string, unknown> {
  const patch: Record<string, unknown> = {};
  if (change.collapsed) patch[VIEW_KEYS.collapsed] = change.collapsed.length > 0 ? change.collapsed : null;
  if (change.plots) patch[VIEW_KEYS.plots] = change.plots.length > 0 ? change.plots : null;
  if (change.fit !== undefined) patch[VIEW_KEYS.fit] = change.fit;
  return patch;
}

/** `list` with `name` added when absent and removed when present. */
export function toggled(list: string[], name: string): string[] {
  return list.includes(name) ? list.filter((n) => n !== name) : [...list, name];
}

/** Which fit to show: the one the address names, else the one the view state
 * remembers if it is still one of the model's, else the server's choice. */
export function chosenFit(
  fromRoute: string | undefined,
  remembered: string | null,
  fits: { id: string }[],
): string | null {
  if (fromRoute) return fromRoute;
  if (remembered && fits.some((f) => f.id === remembered)) return remembered;
  return null;
}
