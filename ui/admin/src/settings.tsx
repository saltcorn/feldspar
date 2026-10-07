// Rendering a `FormField[]` settings spec as a plain form.
//
// This is the shared half of the design's "settings as data" move (§6.2, §13.3).
// A framework declares its settings, a file-store backend declares its settings,
// and — post-MVP — actions, agents and model providers will too. All of them
// answer the same question in the same vocabulary, so the admin UI renders them
// with the same code and knows nothing about any particular one.
//
// It lives here rather than inside a screen because there are now two consumers
// (`ApplicationForm` and `FileStoreForm`), and a copy in each would be two places
// for the rendering of one vocabulary to drift. `ui/form-runtime` (§12) is the
// eventual home; until it exists this is the plain-form stand-in, and having a
// single stand-in is what makes replacing it a one-file change.

import Form from "react-bootstrap/Form";

import { CodeEditor } from "./CodeEditor";
import type { CodeScope } from "./codeTypes";
import { T } from "./i18n";
import type { ExtraOption } from "./newFileStore";

/** One settings field, structurally matching the API's `form_field_schema`.
 *
 * Declared here rather than imported from a specific endpoint's response type,
 * so this module does not depend on whose settings it is rendering — which is
 * the whole point of the shared vocabulary. */
export type FieldSpec = {
  name: string;
  label: string;
  type: string;
  required: boolean;
  default?: unknown | null;
  options: unknown[];
  /** Render as a text area: the value is many lines, not one. A hint the field
   * declares, so no screen has to know that some particular setting happens to
   * hold an SSH key or a certificate. */
  multiline: boolean;
  /** The value is a secret (§11.1): render a password input, and expect the
   * value handed here to be the redaction sentinel rather than the stored key.
   * Submitting the sentinel unchanged keeps what the server has, which is what
   * makes editing a provider's other settings safe. */
  secret?: boolean;
  /** The value is set when the thing is created and fixed afterwards — a git
   * store's working-copy directory names where the checkout was put, so an edit
   * would abandon it rather than move it.
   *
   * Rendered read-only on an edit (`locked` below). The server does not trust
   * that: it puts the stored value back over whatever a save submits. This is
   * the courtesy of not offering a control that does nothing, not the rule. */
  create_only?: boolean;
  /** The value is source code in this language (`"javascript"`), so it is edited
   * in a code editor rather than a text area (§12.2). A hint on the declaration
   * for the same reason `multiline` is one: no screen should know that a
   * particular action's particular setting happens to hold a program. */
  code_language?: string | null;
  /** When the setting applies: every named setting holds one of its values
   * (v1's `showIf`). Empty or absent means always. A setting that does not
   * apply is not shown, and the server does not require it. */
  show_if?: ShowIfCondition[];
};

/** One condition of a `show_if`: setting `name` must hold one of `values`. */
export type ShowIfCondition = { name: string; values: unknown[] };

/** Something the form shows only while its conditions hold (v1's `showIf`):
 * a setting ({@link FieldSpec}), or a target's operation button. */
export type Conditional = { show_if?: ShowIfCondition[] };

/** Whether `item` is shown: every condition in its `show_if` holds for the
 * form's current `values`. A setting the form has no value for yet counts as
 * its default, looked up in `spec`. Compared as text, which is how a form holds
 * every value — a checkbox is `"true"`. */
export function isShown(
  item: Conditional,
  spec: readonly FieldSpec[],
  values: Record<string, string>,
): boolean {
  return (item.show_if ?? []).every(({ name, values: allowed }) => {
    const setting = spec.find((f) => f.name === name);
    const current = values[name] ?? (setting ? asString(setting.default) : "");
    return allowed.some((v) => asString(v) === current);
  });
}

/** What the server substitutes for a secret setting's value on read, and what it
 * reads back as "unchanged" on write (`sc_types::SECRET_SENTINEL`).
 *
 * Duplicated here rather than imported because it is a *protocol* constant — it
 * travels in the JSON — and the client's copy having to match is the same
 * arrangement every other field name in this file lives under. */
export const SECRET_SENTINEL = "••••••••";

/** Read a config value as a display string (config bags arrive as `unknown`). */
export function asString(value: unknown): string {
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return "";
}

/** Read one stored config value as the text the form edits it as.
 *
 * A `json` setting — a rich type's `options`, a File kind's `mime_allow`, an
 * `insert_row` action's field→formula map — is stored as an object or an array
 * and edited as JSON text, so it is stringified rather than dropped. (`asString`
 * yields "" for those, which is right for a one-line summary and wrong for a
 * form that has to hand the value back unchanged.) */
export function configText(value: unknown): string {
  if (value !== null && typeof value === "object") return JSON.stringify(value, null, 2);
  return asString(value);
}

/** Read a stored config bag (an `unknown`) into the string map the form edits. */
export function readConfig(raw: unknown): Record<string, string> {
  const out: Record<string, string> = {};
  if (raw && typeof raw === "object") {
    for (const [key, value] of Object.entries(raw as Record<string, unknown>)) {
      out[key] = configText(value);
    }
  }
  return out;
}

/** Coerce the string form values back to the types the spec declares, dropping
 * empty optional settings.
 *
 * A *required* setting left empty is deliberately passed through rather than
 * defaulted or blocked here: the server validates against the same spec and its
 * message names the offending setting, which is a better error than anything the
 * form could invent, and it keeps one authority for what "valid" means. */
export function buildConfig(
  spec: FieldSpec[],
  values: Record<string, string>,
): Record<string, unknown> {
  const config: Record<string, unknown> = {};
  for (const field of spec) {
    const raw = values[field.name] ?? asString(field.default);
    if (field.type === "bool") {
      config[field.name] = raw === "true";
      continue;
    }
    if (raw.trim() === "") continue;
    if (field.type === "json") {
      // A `json` setting (a rich type's `options`, a File kind's `mime_allow`)
      // is entered as JSON text and parsed here, so it reaches the server as the
      // array/object it declares rather than a string. Invalid JSON is passed
      // through unchanged, letting the server's validation name the mistake
      // rather than this form inventing one.
      try {
        config[field.name] = JSON.parse(raw);
      } catch {
        config[field.name] = raw;
      }
      continue;
    }
    config[field.name] = field.type === "int" || field.type === "float" ? Number(raw) : raw;
  }
  return config;
}

/** The initial form values for a spec: whatever is stored, else each field's
 * declared default. */
export function initialValues(
  spec: FieldSpec[],
  stored: Record<string, string>,
): Record<string, string> {
  const values: Record<string, string> = {};
  for (const field of spec) {
    values[field.name] = stored[field.name] ?? asString(field.default);
  }
  return values;
}

/** One setting rendered as a plain control: a select when it restricts options,
 * a checkbox/number/text otherwise.
 *
 * `locked` means "the thing being configured already exists", which is what
 * turns a `create_only` field read-only. It is passed in rather than read from
 * the field because the field cannot know: the same spec renders the create form
 * and the edit form. */
export function SettingField({
  field,
  value,
  onChange,
  idPrefix = "cfg",
  locked = false,
  codeScope,
  extraOptions = [],
  pickerHint,
  pinned,
}: {
  field: FieldSpec;
  value: string;
  onChange: (value: string) => void;
  idPrefix?: string;
  locked?: boolean;
  /** Why this setting cannot be edited here at all — a TLS key this host's
   * `feldspar.toml` pins. Read-only whatever `locked` says, with this sentence
   * under it in place of the create-only one. */
  pinned?: string;
  /** Makes the field a drop-down even while it has no choices, with this
   * sentence under it saying why there are none and what to do — an app icon
   * picked from a store that holds no images yet. A text box there would
   * invite typing a path the screen could have offered. */
  pickerHint?: string;
  /** Choices the *screen* adds after the field's own, each with its own label —
   * "Create a new local file store" at the end of a new application's store
   * picker. Passed in because the field only knows the values it may hold; what
   * else the screen can do with the answer is the screen's to offer. */
  extraOptions?: ExtraOption[];
  /** What a *code* setting's editor should declare in scope — the event a
   * trigger's body will run in. Passed in because the screen knows the event and
   * the field knows it is code, and neither knows both. */
  codeScope?: CodeScope;
}) {
  const controlId = `${idPrefix}-${field.name}`;
  const fixed = pinned !== undefined || (locked && Boolean(field.create_only));
  // Why the control cannot be edited, said once, wherever it is rendered.
  const fixedHint =
    pinned !== undefined ? (
      <Form.Text muted>{pinned}</Form.Text>
    ) : fixed ? (
      <Form.Text muted><T text="Chosen when this was created; it cannot be changed." /></Form.Text>
    ) : null;
  const choices = field.options.length + extraOptions.length;
  if (choices > 0 || pickerHint !== undefined) {
    return (
      <Form.Group className="mb-3" controlId={controlId}>
        <Form.Label>
          {field.label}
          {field.required && <span className="text-danger"> *</span>}
        </Form.Label>
        <Form.Select
          value={value}
          disabled={fixed}
          onChange={(e) => onChange(e.target.value)}
        >
          <option value="">—</option>
          {field.options.map((opt) => {
            const s = asString(opt);
            return (
              <option key={s} value={s}>
                {s}
              </option>
            );
          })}
          {extraOptions.map((opt) => (
            <option key={opt.value} value={opt.value}>
              {opt.label}
            </option>
          ))}
        </Form.Select>
        {choices === 0 && pickerHint && <Form.Text muted>{pickerHint}</Form.Text>}
        {fixedHint}
      </Form.Group>
    );
  }
  if (field.type === "bool") {
    return (
      <Form.Group className="mb-3" controlId={controlId}>
        <Form.Check
          type="checkbox"
          label={field.label}
          checked={value === "true"}
          disabled={fixed}
          onChange={(e) => onChange(e.target.checked ? "true" : "false")}
        />
        {fixedHint}
      </Form.Group>
    );
  }
  if (field.code_language) {
    // Source code: an editor, with the sandbox's types loaded (see
    // `CodeEditor.tsx`). A required code setting carries no `required`
    // attribute — there is no form control to hang one on — and it does not need
    // one: the action refuses a blank body on save, with a message naming it.
    return (
      <Form.Group className="mb-3">
        <Form.Label htmlFor={controlId}>
          {field.label}
          {field.required && <span className="text-danger"> *</span>}
        </Form.Label>
        <CodeEditor
          id={controlId}
          value={value}
          language={field.code_language}
          scope={codeScope}
          readOnly={fixed}
          onChange={onChange}
        />
        {fixedHint}
      </Form.Group>
    );
  }
  if (field.type === "json") {
    // JSON text, so it gets room and a monospace face: these are objects and
    // arrays (an action's field→formula map, a MIME allow-list), not words.
    return (
      <Form.Group className="mb-3" controlId={controlId}>
        <Form.Label>
          {field.label}
          {field.required && <span className="text-danger"> *</span>}
        </Form.Label>
        <Form.Control
          as="textarea"
          rows={4}
          className="font-monospace"
          value={value}
          required={field.required}
          readOnly={fixed}
          onChange={(e) => onChange(e.target.value)}
        />
        {fixedHint ?? <Form.Text muted>JSON.</Form.Text>}
      </Form.Group>
    );
  }
  return (
    <Form.Group className="mb-3" controlId={controlId}>
      <Form.Label>
        {field.label}
        {field.required && <span className="text-danger"> *</span>}
      </Form.Label>
      {field.multiline ? (
        // Monospace and selectable-on-focus, because a multi-line setting is
        // something copied in or out (a key, a certificate) far more often than
        // it is typed.
        <Form.Control
          as="textarea"
          rows={3}
          className="font-monospace small"
          value={value}
          required={field.required}
          readOnly={fixed}
          onChange={(e) => onChange(e.target.value)}
          onFocus={(e) => e.currentTarget.select()}
        />
      ) : (
        <Form.Control
          type={
            field.secret
              ? "password"
              : field.type === "int" || field.type === "float"
                ? "number"
                : "text"
          }
          step={field.type === "float" ? "any" : undefined}
          value={value}
          required={field.required}
          readOnly={fixed}
          onChange={(e) => onChange(e.target.value)}
          // A secret arrives as the sentinel, and the sentinel is what the
          // server reads as "unchanged". Clearing it on focus is what makes
          // *replacing* a key possible: typing into the mask would otherwise
          // produce "••••••••sk-new", which is neither the old key nor the new
          // one. Blurring without typing puts it back, so opening a form and
          // tabbing through it does not silently unset a key.
          onFocus={field.secret ? () => value === SECRET_SENTINEL && onChange("") : undefined}
          onBlur={field.secret ? () => value === "" && onChange(SECRET_SENTINEL) : undefined}
        />
      )}
      {fixedHint}
      {field.secret && value === SECRET_SENTINEL && (
        <Form.Text muted><T text="Stored. Type to replace it." /></Form.Text>
      )}
    </Form.Group>
  );
}

/** A whole settings spec rendered as a block of controls. */
export function SettingsFields({
  spec,
  values,
  onChange,
  idPrefix,
  locked = false,
  codeScope,
  extraOptions = {},
  pickerHints = {},
  conditionSpec,
}: {
  spec: FieldSpec[];
  values: Record<string, string>;
  onChange: (name: string, value: string) => void;
  idPrefix?: string;
  /** The thing being configured already exists, so its `create_only` settings
   * are shown but not editable. */
  locked?: boolean;
  /** What a code setting's editor declares in scope (see [`SettingField`]).
   * Ignored by every spec that declares no code setting. */
  codeScope?: CodeScope;
  /** Screen-supplied choices appended to a setting's own, by setting name (see
   * [`SettingField`]). */
  extraOptions?: Record<string, ExtraOption[]>;
  /** Settings that are always drop-downs, by name, each with the sentence shown
   * while it has no choices (see [`SettingField`]). */
  pickerHints?: Record<string, string>;
  /** The settings a field's `show_if` may name, when that is more than `spec`
   * — a form split into cards, whose conditions reach across them. */
  conditionSpec?: readonly FieldSpec[];
}) {
  return (
    <>
      {spec
        .filter((field) => isShown(field, conditionSpec ?? spec, values))
        .map((field) => (
        <SettingField
          key={field.name}
          field={field}
          value={values[field.name] ?? asString(field.default)}
          onChange={(v) => onChange(field.name, v)}
          idPrefix={idPrefix}
          locked={locked}
          codeScope={codeScope}
          extraOptions={extraOptions[field.name]}
          pickerHint={pickerHints[field.name]}
        />
      ))}
    </>
  );
}
