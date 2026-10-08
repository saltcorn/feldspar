// What changed, told to whatever else is on the screen (analytics TODO A4.1).
//
// With the view split, a dataset edited on one side is shown on the other —
// a model beside its dataset, an explorer beside the model being built from
// its dataset — and what the other side shows of it must follow. So a screen
// that saves a dataset or a model **announces** it, and a screen that shows
// one **listens** and reads it again. Each announcement says which side made
// it, so the side that made a change does not read its own change back over
// what it is in the middle of.
//
// In-page only: two browser tabs are two sessions, as they were before.

import type { Side } from "./router";

/** A thing that changed: a dataset (saved, cloned, deleted) or a model
 * (saved, fitted). */
export type Change = { kind: "dataset" | "model"; id: string; from: Side };

type Listener = (change: Change) => void;

const listeners = new Set<Listener>();

/** Tell every listener. */
export function announce(change: Change): void {
  for (const listener of [...listeners]) listener(change);
}

/** Listen, until the answer is called. */
export function listen(listener: Listener): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Whether a listener on `side` should act on `change`: a change from the
 * other side, of the kind it shows. */
export function concerns(change: Change, side: Side, kinds: Change["kind"][]): boolean {
  return change.from !== side && kinds.includes(change.kind);
}
