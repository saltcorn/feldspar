// The model behind the field editor (design §3.4) — the half of it that is not
// React.
//
// The Fields card on the table page has one modal and does two things with it:
// **add** a field, and **edit** one that exists. They are one form because they
// ask one question — what is this column, and what does it mean? — and because
// two forms would be two places for "a File's parameters live on the kind
// object, not in `attributes`" to be got right or wrong independently.
//
// What the two do *not* share is what they may change. Adding a field writes a
// column: its name and its storage type are decided then. Editing one writes the
// `_fd_fields` overlay (§3.2) — the label, the rich type, the kind and the
// attributes — plus the two column properties an edit may change: whether it is
// in the primary key and whether it accepts nulls. Retyping or renaming a column
// is a migration and a migration framework is out of scope (§3.3), so the form
// disables what an edit cannot carry rather than offering it and losing it.
//
// Everything here is a plain function over plain values, which is what makes the
// round trip — a field as `listFields` reports it, into the form, back out as a
// request that means the same thing — assertable without a browser.

import type {
  CreateFieldRequest,
  ListFieldsResponse,
  ListFieldTypesResponse,
  UpdateFieldRequest,
} from "./client";
import { EMPTY_KEY, keyIsComplete, keyKindRequest, type KeyKind } from "./keyField";
import { buildConfig, readConfig } from "./settings";

/** One field as `listFields` reports it. */
export type FieldItem = ListFieldsResponse[number];

/** One entry of `listFieldTypes`: a basic type, a rich type or a kind. */
export type FieldTypeItem = ListFieldTypesResponse[number];

/** A field's `kind`, narrowed from the `unknown` the API types it as. */
export type FieldKind = {
  type?: string;
  expression?: string;
  target_table?: string;
  target_field?: string;
  summary_field?: string | null;
  store?: string;
  folder?: string | null;
} | null;

/**
 * What the field modal holds while it is open.
 *
 * `typeName` is the chosen entry of `listFieldTypes`, which is one list of three
 * things — basic types, rich types and the Key/File kinds — because that is one
 * question to the admin ("what is this field?") even though the three land in
 * different places on the wire.
 */
export type FieldForm = {
  name: string;
  label: string;
  description: string;
  /** The chosen `listFieldTypes` entry's name. */
  typeName: string;
  nullable: boolean;
  /**
   * Whether the column accepted nulls when the form opened, so an edit sends
   * `required` only when the admin changed it — an edit that did not touch the
   * box must not be refused because of rows it was not about.
   */
  wasNullable: boolean;
  /** Part of the table's primary key — a field like any other (GOALS). */
  primaryKey: boolean;
  /**
   * Whether the column already fills itself in — read back off the column, not
   * inferred from its type, so a key that came from a hand-written
   * `CREATE TABLE` says what it really does.
   */
  generated: boolean;
  /**
   * The key the field had when the form opened, which is what tells ticking the
   * box ("this will start numbering itself") apart from describing a key that is
   * already there ("this one does not, and never will unless it is recreated").
   */
  wasPrimaryKey: boolean;
  /** Computed on read, with no stored column (§7.3). */
  calculated: boolean;
  expression: string;
  /**
   * The JavaScript expression an insert that leaves a NOT NULL field out takes
   * its value from, with `user` and the row's other fields in scope. Empty for
   * none; only sent for a field that is not nullable.
   */
  defaultExpression: string;
  /** The attribute form's values, keyed by spec-field name. */
  attrs: Record<string, string>;
  /** A Key's parameters, which the generic attribute form cannot render. */
  key: KeyKind;
};

/** A form with nothing entered. */
export const EMPTY_FIELD_FORM: FieldForm = {
  name: "",
  label: "",
  description: "",
  typeName: "",
  nullable: true,
  wasNullable: true,
  primaryKey: false,
  generated: false,
  wasPrimaryKey: false,
  calculated: false,
  expression: "",
  defaultExpression: "",
  attrs: {},
  key: EMPTY_KEY,
};

/** The form an "Add field" opens on: empty, on the first type offered. */
export function newFieldForm(typeName: string): FieldForm {
  return { ...EMPTY_FIELD_FORM, typeName };
}

/**
 * The form for a field that exists — the round trip that makes editing possible.
 *
 * The chosen type comes from wherever the field's identity actually lives, which
 * is not one place: a Key or a File is named by its **kind**, a calculated field
 * is named by its kind but shown as its value type, and everything else is named
 * by its type. Their parameters differ the same way — a kind's are on the kind
 * object, a rich type's are in `attributes` — and both are edited through the
 * one spec-driven attribute form, so they are read into the one `attrs` map here.
 */
export function fieldForm(field: FieldItem): FieldForm {
  const kind = (field.kind ?? null) as FieldKind;
  const base: FieldForm = {
    ...EMPTY_FIELD_FORM,
    name: field.name,
    label: field.label,
    description: field.description,
    typeName: field.type,
    nullable: field.nullable,
    wasNullable: field.nullable,
    primaryKey: field.primary_key,
    generated: field.generated,
    wasPrimaryKey: field.primary_key,
    defaultExpression: field.default_expression,
    attrs: readConfig(field.attributes),
  };
  if (kind?.type === "calc") {
    // The type picker names the value's *display* type, which is what the merged
    // field already reports; the expression is the field itself.
    return { ...base, calculated: true, expression: kind.expression ?? "", attrs: {} };
  }
  if (kind?.type === "key") {
    return {
      ...base,
      typeName: "key",
      attrs: {},
      key: {
        target_table: kind.target_table ?? "",
        target_field: kind.target_field ?? "",
        // "" is the shape the summary select's placeholder option produces,
        // and the server reports "no summary" as null.
        summary_field: kind.summary_field ?? "",
      },
    };
  }
  if (kind && kind.type && kind.type !== "plain") {
    // Any other kind (today, File) is a text column plus parameters, so the
    // picker names the kind and the parameters are the attribute form's values.
    return { ...base, typeName: kind.type, attrs: kindConfig(kind) };
  }
  return base;
}

/**
 * What the primary-key tick box has to say about the key's *value*, which is the
 * question ticking it immediately raises: who puts a number in this column?
 *
 * Three answers, and which one is true is not a property of the type alone. An
 * `int` key numbers itself and a `uuid` key generates itself — but only from the
 * moment the box is ticked, because that is when the column is given its
 * generator. So a key that is *already* there is described by what the column
 * actually does (`generated`, read back by introspection), and a key about to be
 * made is described by what ticking the box will do to it. The two differ for
 * exactly the table Saltcorn is built to pick up as it finds it: one created
 * outside the admin UI, whose `id` is an ordinary `bigint` nobody fills in.
 */
export function keyValueNote(form: FieldForm): string {
  const willGenerate = form.typeName === "int" || form.typeName === "uuid";
  const generates = form.generated || (!form.wasPrimaryKey && willGenerate);
  if (!generates) {
    return "Identifies the row. Nothing fills this key in, so whoever writes the row supplies it.";
  }
  const how =
    form.typeName === "uuid"
      ? "generates a UUID for itself"
      : form.typeName === "int"
        ? "numbers itself"
        : "fills itself in";
  return `Identifies the row. This key ${how} — leave it blank and the database assigns one.`;
}

/** A kind's parameters as the attribute form edits them: everything on the kind
 * object except the discriminator that said which kind it is. */
function kindConfig(kind: object): Record<string, string> {
  const values = readConfig(kind);
  delete values.type;
  return values;
}

/**
 * What is wrong with the form, or `null` if the server should decide.
 *
 * Only the two things the form itself can be sure of. Everything else — an
 * attribute the type refuses, a rich type that does not fit the column, a
 * reference the database will not enforce — is left to the server, whose message
 * names what to fix and is the only authority for what "valid" means.
 */
export function fieldFormError(form: FieldForm, selected: FieldTypeItem | null): string | null {
  if (!selected) return "Choose a type for the field.";
  if (form.calculated) {
    // Three of these, now: a calculated field has no column, so there is nothing
    // for a key to be made of, and the server says the same thing.
    if (form.primaryKey) return "A calculated field cannot be part of the primary key.";
    return form.expression.trim() === "" ? "A calculated field needs a formula." : null;
  }
  if (selected.name === "key" && !keyIsComplete(form.key)) {
    return "A key needs a table and a field to point at.";
  }
  return null;
}

/** The type half of a create or update request: what the type picker, the
 * calculated switch, the Key form and the attribute form mean on the wire. */
function fieldTypeBody(
  form: FieldForm,
  selected: FieldTypeItem,
): { type?: string; kind?: unknown; attributes?: unknown } {
  if (form.calculated) {
    // Virtual field: no column, so no storage kind. The chosen type stays as the
    // value's display type; the expression is the field.
    return { type: selected.name, kind: { type: "calc", expression: form.expression.trim() } };
  }
  if (selected.name === "key") {
    // No `type`: a key is stored as whatever its target is stored as, and the
    // server derives that from the field it points at rather than trusting an
    // answer this form would have to guess.
    return { kind: keyKindRequest(form.key) };
  }
  if (selected.category === "kind") {
    // A File is a path, so it is stored as text; its parameters come from the
    // kind's declared spec like any other settings form.
    return {
      type: "text",
      kind: { type: selected.name, ...buildConfig(selected.config_spec, form.attrs) },
    };
  }
  if (selected.category === "rich") {
    return { type: selected.name, attributes: buildConfig(selected.config_spec, form.attrs) };
  }
  return { type: selected.name };
}

/**
 * Whether the form offers a default value: every stored field does, and a
 * calculated one, which is never written, does not. On a nullable field a
 * default that gives nothing (`user && user.id` for a guest) leaves it null.
 */
export function defaultEditable(form: FieldForm): boolean {
  return !form.calculated;
}

/**
 * The default expression to send: the box's, or nothing where none applies.
 * Both endpoints read an omitted one as "none", so on an edit leaving it out is
 * how it is cleared.
 */
function defaultBody(form: FieldForm): { default_expression?: string } {
  const expression = defaultEditable(form) ? form.defaultExpression.trim() : "";
  return expression ? { default_expression: expression } : {};
}

/** The `createField` request the form describes. */
export function createFieldBody(form: FieldForm, selected: FieldTypeItem): CreateFieldRequest {
  return {
    name: form.name.trim(),
    label: form.label.trim(),
    description: form.description.trim(),
    // A calculated field is virtual: there is no column to make NOT NULL.
    required: !form.calculated && !form.nullable,
    // Nothing invents a key, so this is the only way a table gets one: the box
    // on the field being added (GOALS).
    primary_key: form.primaryKey,
    ...defaultBody(form),
    ...fieldTypeBody(form, selected),
  };
}

/**
 * Whether the admin may tick or untick "Nullable" on this form.
 *
 * A key column is NOT NULL whatever the box says, and a calculated field has no
 * column to constrain. Everything else can go either way — making a field
 * required is refused by the server while a row has no value in it, and the
 * message says so.
 */
export function nullableEditable(form: FieldForm): boolean {
  return !form.primaryKey && !form.calculated;
}

/**
 * The `updateField` request the form describes: the whole overlay, plus the
 * column properties the admin changed.
 *
 * Whole-object like `updateTable` (§13.1) — what is left out is cleared, not
 * kept — so the label and description are always stated even when the admin only
 * came to change an attribute. The name and the column's storage are absent
 * because the endpoint has nowhere to put them: they are the database's, and
 * changing one is a migration (§3.3).
 */
export function updateFieldBody(form: FieldForm, selected: FieldTypeItem): UpdateFieldRequest {
  // Stated only when changed, where the endpoint reads "omitted" as "leave it".
  const nullableChanged = nullableEditable(form) && form.nullable !== form.wasNullable;
  return {
    label: form.label.trim(),
    description: form.description.trim(),
    // A table with no key could otherwise only get one by being recreated — see
    // the endpoint.
    primary_key: form.primaryKey,
    ...(nullableChanged ? { required: !form.nullable } : {}),
    // Whole-object: an omitted one clears it.
    ...defaultBody(form),
    ...fieldTypeBody(form, selected),
  };
}
