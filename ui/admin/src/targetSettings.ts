// A framework's build targets' own settings on the application form.
//
// A target (an Android APK) may declare settings that configure it alone:
// debug or release, a signing keystore. The server lists them in
// the framework's `config_spec` like every other setting — they are stored,
// validated and handed to the framework's generators the same way — and names
// them per target in `targets`. This module splits them out so the form shows
// each target's settings in a card of its own, and fills each file picker (a
// setting that picks a file: an app icon, a keystore) with the matching files
// from the application's file store (`storeNameOf`, `fileOptions`).
//
// A module rather than inline in the screen for the reason `newFileStore.ts` is
// one: what goes where is testable without a browser (`targetSettings.test.ts`).

import { NEW_LOCAL_FILE_STORE, type ExtraOption } from "./newFileStore";
import type { FieldSpec, ShowIfCondition } from "./settings";

/** Something the module does for a target on request: a button under the
 * target's settings, shown while its `show_if` holds. */
export type TargetOperationDecl = {
  name: string;
  label: string;
  description: string;
  show_if: ShowIfCondition[];
};

/** A target as `listFrameworks` describes it. */
export type TargetDecl = {
  name: string;
  label: string;
  options: string[];
  operations?: TargetOperationDecl[];
};

/** One target's settings, in the order the framework declared them, and its
 * operations. */
export type TargetSettings = {
  name: string;
  label: string;
  spec: FieldSpec[];
  operations: TargetOperationDecl[];
};

/** The framework's own settings, and each target's, from one `config_spec`. A
 * target that declares no settings gets no card. */
export function splitTargetSettings(
  spec: readonly FieldSpec[],
  targets: readonly TargetDecl[],
): { general: FieldSpec[]; targets: TargetSettings[] } {
  const owned = new Set(targets.flatMap((t) => t.options));
  return {
    general: spec.filter((f) => !owned.has(f.name)),
    targets: targets
      .map((t) => ({
        name: t.name,
        label: t.label,
        spec: spec.filter((f) => t.options.includes(f.name)),
        operations: t.operations ?? [],
      }))
      .filter((t) => t.spec.length > 0),
  };
}

/** A file picker: a setting whose value is a file in the application's store
 * (an app icon, a keystore), with the extensions it accepts, as
 * `listFrameworks` describes it (`file_settings`). */
export type FileSetting = { name: string; extensions: string[] };

/** The name of the application's file store, which the file pickers list
 * files from: the value of its file store setting (e.g. `"withcard"`). `null` while
 * that is blank, or set to a store still to be created, which has no files yet. */
export function storeNameOf(
  fileStoreSettings: readonly string[],
  config: Record<string, string>,
): string | null {
  for (const setting of fileStoreSettings) {
    const store = (config[setting] ?? "").trim();
    if (store && store !== NEW_LOCAL_FILE_STORE) return store;
  }
  return null;
}

/** The dropdown choices for each file picker (an icon, a keystore).
 *
 * A picker's choices are the matching files found in the store (`found`).
 * If the picker already holds a file that is no longer there, that file is
 * added as a choice too. Otherwise the dropdown could not show it, and
 * opening and saving the form would quietly clear the setting.
 *
 * Example: `found = { app_icon: ["a.png", "b.png"] }`, and the icon is set to
 * `"old.png"`, which has since been deleted. The icon's dropdown offers
 * `a.png`, `b.png` and `old.png`, with `old.png` still selected. */
export function fileOptions(
  filePickers: readonly FileSetting[],
  found: Record<string, readonly string[]>,
  config: Record<string, string>,
): Record<string, ExtraOption[]> {
  return Object.fromEntries(
    filePickers.map(({ name }) => {
      const files = found[name] ?? [];
      const current = (config[name] ?? "").trim();
      const paths = current && !files.includes(current) ? [...files, current] : files;
      return [name, paths.map((path) => ({ value: path, label: path }))];
    }),
  );
}
