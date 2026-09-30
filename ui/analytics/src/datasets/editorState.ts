// What a Dataset editor workspace keeps (analytics TODO A1.16, A1.17): which
// dataset is open — none while the list of datasets is showing — and which
// operation's result is selected: an operation's id, `""` for the base, and
// `null` for the last stage.

export type EditorState = { dataset: string | null; operation: string | null };

/** A workspace's state as a Dataset editor reads it; anything unexpected is
 * the list. */
export function readEditorState(state: Record<string, unknown>): EditorState {
  return {
    dataset: typeof state.dataset === "string" && state.dataset !== "" ? state.dataset : null,
    operation: typeof state.operation === "string" ? state.operation : null,
  };
}

/** Open `dataset`, on its last stage. */
export function openDataset(state: Record<string, unknown>, dataset: string) {
  return { ...state, dataset, operation: null };
}

/** Select the stage after `operation` (`""` for the base, `null` for the last). */
export function selectOperation(state: Record<string, unknown>, operation: string | null) {
  return { ...state, operation };
}

/** Back to the list of datasets. */
export function backToList(state: Record<string, unknown>) {
  return { ...state, dataset: null, operation: null };
}
