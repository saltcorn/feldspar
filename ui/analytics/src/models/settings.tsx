// A provider's settings, rendered from its `config_spec` (analytics TODO A3.5):
// the admin UI's `settings.tsx`, cut to what a model's configuration uses.
//
// The admin UI renders every "settings as data" declaration with one form —
// file stores, actions, applications — and the model screens were one of its
// consumers until they moved here. A copy rather than a shared import, because
// the two bundles share no source but the stylesheet; what both must agree on
// is the wire (`form_field_schema`), and that is the generated client's.
//
// One difference: a setting declaring a `code_language` is a monospace text
// area here rather than the admin's Monaco editor with the sandbox's types. No
// model provider declares one; the Stan program has its own editor
// (`ProgramEditor.tsx`).

import Form from "react-bootstrap/Form";

import { T } from "../i18n";

/** One settings field, structurally matching the API's `form_field_schema`. */
export type FieldSpec = {
  name: string;
  label: string;
  type: string;
  required: boolean;
  default?: unknown | null;
  options: unknown[];
  /** Render as a text area: the value is many lines, not one. */
  multiline: boolean;
  /** The value is a secret: a password input. */
  secret?: boolean;
  create_only?: boolean;
  code_language?: string | null;
};

/** Read a config value as a display string (config bags arrive as `unknown`). */
export function asString(value: unknown): string {
  if (typeof value === "string") return value;
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  return "";
}

/** One stored config value as the text the form edits it as: an object or an
 * array (a `json` setting) as JSON text, anything else as [`asString`]. */
export function configText(value: unknown): string {
  if (value !== null && typeof value === "object") return JSON.stringify(value, null, 2);
  return asString(value);
}

/** A stored config bag (an `unknown`) as the string map the form edits. */
export function readConfig(raw: unknown): Record<string, string> {
  const out: Record<string, string> = {};
  if (raw && typeof raw === "object") {
    for (const [key, value] of Object.entries(raw as Record<string, unknown>)) {
      out[key] = configText(value);
    }
  }
  return out;
}

/** The string form values coerced back to the types the spec declares, empty
 * optional settings dropped. A required setting left empty is passed through:
 * the server validates against the same spec and names it. */
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
      // Invalid JSON is passed through, for the server to name the mistake.
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

/** One setting as a plain control: a select when it restricts options, a
 * checkbox, number or text otherwise. */
export function SettingField({
  field,
  value,
  onChange,
  idPrefix = "cfg",
}: {
  field: FieldSpec;
  value: string;
  onChange: (value: string) => void;
  idPrefix?: string;
}) {
  const controlId = `${idPrefix}-${field.name}`;
  const label = (
    <Form.Label>
      {field.label}
      {field.required && <span className="text-danger"> *</span>}
    </Form.Label>
  );
  if (field.options.length > 0) {
    return (
      <Form.Group className="mb-3" controlId={controlId}>
        {label}
        <Form.Select value={value} onChange={(e) => onChange(e.target.value)}>
          <option value="">—</option>
          {field.options.map((opt) => {
            const s = asString(opt);
            return (
              <option key={s} value={s}>
                {s}
              </option>
            );
          })}
        </Form.Select>
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
          onChange={(e) => onChange(e.target.checked ? "true" : "false")}
        />
      </Form.Group>
    );
  }
  if (field.type === "json" || field.multiline || field.code_language) {
    return (
      <Form.Group className="mb-3" controlId={controlId}>
        {label}
        <Form.Control
          as="textarea"
          rows={field.type === "json" ? 4 : 3}
          className="font-monospace small"
          value={value}
          required={field.required}
          onChange={(e) => onChange(e.target.value)}
        />
        {field.type === "json" && (
          <Form.Text muted>
            <T text="JSON." />
          </Form.Text>
        )}
      </Form.Group>
    );
  }
  return (
    <Form.Group className="mb-3" controlId={controlId}>
      {label}
      <Form.Control
        type={field.secret ? "password" : field.type === "int" || field.type === "float" ? "number" : "text"}
        step={field.type === "float" ? "any" : undefined}
        value={value}
        required={field.required}
        onChange={(e) => onChange(e.target.value)}
      />
    </Form.Group>
  );
}

/** A whole settings spec as a block of controls. */
export function SettingsFields({
  spec,
  values,
  onChange,
  idPrefix,
}: {
  spec: FieldSpec[];
  values: Record<string, string>;
  onChange: (name: string, value: string) => void;
  idPrefix?: string;
}) {
  return (
    <>
      {spec.map((field) => (
        <SettingField
          key={field.name}
          field={field}
          value={values[field.name] ?? asString(field.default)}
          onChange={(v) => onChange(field.name, v)}
          idPrefix={idPrefix}
        />
      ))}
    </>
  );
}
