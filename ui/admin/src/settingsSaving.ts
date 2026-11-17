// How the Settings screen saves: as each setting is edited, except the ones
// held for their card's Apply button.
//
// The server declares which are held (`applied` on the field,
// `sc_config::ConfigDef::applied`): the TLS and Email sections, whose settings
// it checks together, and the admin subdomain, whose save moves the admin UI.
// Everything else saves on its own, in one of two ways:
//
// - a **checkbox or a choice** at once — its every change is a finished one;
// - **typing** when the admin is done with the box (it loses focus, or Enter),
//   never on a keystroke, so a half-typed path or number is not sent to be
//   refused, and a half-typed language list is not briefly the live one.
//
// The server merges what a save sends over what it has stored, so a save sends
// only the settings it is about, and an Apply only its card's edits.

import { initialValues, type FieldSpec } from "./settings";

/** A setting as the Settings screen is sent it: a declaration, its sentence of
 * help, and whether its card's Apply button saves it. */
export type SettingSpec = FieldSpec & { help?: string; applied?: boolean };

/** When an edit to `field` is saved: `change`, as it is made; `commit`, when
 * the admin finishes typing; `apply`, with its card's Apply button. */
export type SaveMode = "change" | "commit" | "apply";

export function saveMode(field: SettingSpec): SaveMode {
  if (field.applied) return "apply";
  if (field.type === "bool" || field.options.length > 0) return "change";
  return "commit";
}

/** The settings that save as they are edited. */
export function autosavedFields(spec: SettingSpec[]): SettingSpec[] {
  return spec.filter((field) => !field.applied);
}

/** The settings held for an Apply button. */
export function appliedFields(spec: SettingSpec[]): SettingSpec[] {
  return spec.filter((field) => field.applied);
}

/** The settings in `spec` whose box differs from what is stored — compared as
 * the form holds them, so a setting nobody has set is its declared default and
 * an untouched secret is the sentinel on both sides. */
export function unsavedNames(
  spec: FieldSpec[],
  stored: Record<string, string>,
  values: Record<string, string>,
): string[] {
  const saved = initialValues(spec, stored);
  return spec
    .filter((field) => (values[field.name] ?? saved[field.name]) !== saved[field.name])
    .map((field) => field.name);
}

/** The form after a save of `names` landed: each saved box shows what the
 * server stored (a subdomain lower-cased, a secret as the sentinel), unless the
 * admin has typed into it since the save was sent — that edit is newer than
 * the answer, and is kept. */
export function afterSave(
  names: string[],
  sent: Record<string, string>,
  current: Record<string, string>,
  fresh: Record<string, string>,
): Record<string, string> {
  const next = { ...current };
  for (const name of names) {
    if (current[name] === sent[name] && name in fresh) next[name] = fresh[name];
  }
  return next;
}

/** Whether a section mixes the two kinds — the Development card's admin
 * subdomain among switches that save at once — and so needs to say, under each
 * held setting, that it waits for Apply. */
export function mixesModes(fields: SettingSpec[]): boolean {
  const held = appliedFields(fields).length;
  return held > 0 && held < fields.length;
}
