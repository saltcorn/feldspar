// The application form's **static directories**: a mount, the store the files
// live in, and the subdirectory within it (design §13.2).
//
// A module rather than state inside the screen for the reason `apiRows.ts` is
// one: the part that has to be right is not a control but a *decision* — which
// stores this row may name — and that is testable without a browser
// (`staticDirs.test.ts`).
//
// The decision itself: a static directory's store is one of the **application's
// own** declared file stores, not one of the server's. The declared subset is
// the whole truth about which stores an application touches, and the server
// refuses a save outside it (`validate_static_dirs`), so offering the full list
// here would be offering choices that cannot be saved. It is also why this takes
// the form's live subset rather than the endpoint's list: ticking a store above
// changes what this offers, immediately, without a round trip.

/** One row of the static directories list, as the form edits it. */
export type StaticRow = { mount: string; store: string; path: string };

/** An empty row — what "Add" produces. */
export function blankStaticRow(): StaticRow {
  return { mount: "", store: "", path: "" };
}

/** One entry of the store drop-down: the store's name, and whether the
 * application actually declares it. */
export type StoreOption = { value: string; declared: boolean };

/** The stores a static directory row may name: the application's declared
 * subset, in the order the form holds it.
 *
 * `current` — what the row already has — is kept and offered even when the
 * subset no longer contains it, marked as undeclared. A form must never
 * silently discard what it was given to edit: a store un-ticked above is a
 * mistake the admin can see and undo, whereas a row that quietly reset itself
 * to the first store in the list is a different directory saved under the same
 * mount. It is the pattern the framework picker on this same screen already
 * uses for a framework this server does not register.
 */
export function storeOptions(fileStores: string[], current: string): StoreOption[] {
  const options = fileStores.map((value) => ({ value, declared: true }));
  if (current && !fileStores.includes(current)) {
    options.push({ value: current, declared: false });
  }
  return options;
}

/** The rows as the application record stores them: the blank rows an admin
 * added and did not fill in are dropped, and everything else goes through
 * **untouched** — a store the subset no longer offers included, because saving
 * is not the moment to quietly rewrite a field the admin did not edit. The
 * server has the last word on it either way (`validate_static_dirs`), and being
 * told is better than being corrected. */
export function staticDirsToRequest(rows: StaticRow[]): StaticRow[] {
  return rows.filter((d) => d.mount.trim() || d.path.trim());
}
