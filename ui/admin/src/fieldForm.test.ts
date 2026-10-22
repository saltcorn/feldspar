/**
 * The field editor's round trip: a field as the server reports it, into the
 * form, back out as a request that says the same thing.
 *
 * This is the whole of what makes an Edit button safe. `updateField` states the
 * **whole** overlay — what is left out is cleared, not kept (§13.1) — so a form
 * that failed to read one part of a field back would silently drop it the moment
 * an admin opened the field to change something else. The claims worth pinning
 * down are therefore the ones where a field's identity is not where it looks:
 *
 *   - a File's parameters live on the `kind` object, not in `attributes`;
 *   - a Key names its target on the kind object, and reports "no summary" as
 *     `null` where the form's select holds "";
 *   - a calculated field's *type* is the value's display type, and its kind is
 *     the expression;
 *   - and a plain field with a rich type keeps the type, because dropping it
 *     would take the attributes with it.
 */

import { describe, expect, it } from "vitest";

import {
  createFieldBody,
  defaultEditable,
  fieldForm,
  fieldFormError,
  keyValueNote,
  newFieldForm,
  nullableEditable,
  updateFieldBody,
  type FieldItem,
  type FieldTypeItem,
} from "./fieldForm";

/** A field as `listFields` reports it, with only what a test states set. */
function field(over: Partial<FieldItem>): FieldItem {
  return {
    name: "title",
    label: "",
    description: "",
    sql_type: "text",
    type: "text",
    nullable: true,
    required: false,
    unique: false,
    primary_key: false,
    generated: false,
    default_expression: "",
    kind: { type: "plain" },
    attributes: {},
    ...over,
  };
}

/** The field types this test picks from, as `listFieldTypes` reports them. */
const TEXT: FieldTypeItem = { name: "text", label: "Text", category: "basic", config_spec: [] };
const STRING: FieldTypeItem = {
  name: "string",
  label: "String",
  category: "rich",
  config_spec: [
    {
      name: "max_length",
      label: "Max length",
      type: "int",
      required: false,
      default: null,
      options: [],
      multiline: false,
      secret: false,
      create_only: false,
      show_if: [],
    },
  ],
};
const FILE: FieldTypeItem = {
  name: "file",
  label: "File",
  category: "kind",
  config_spec: [
    {
      name: "store",
      label: "Store",
      type: "string",
      required: true,
      default: null,
      options: [],
      multiline: false,
      secret: false,
      create_only: false,
      show_if: [],
    },
    {
      name: "folder",
      label: "Folder",
      type: "string",
      required: false,
      default: null,
      options: [],
      multiline: false,
      secret: false,
      create_only: false,
      show_if: [],
    },
    {
      name: "mime_allow",
      label: "Allowed types",
      type: "json",
      required: false,
      default: null,
      options: [],
      multiline: false,
      secret: false,
      create_only: false,
      show_if: [],
    },
  ],
};
const KEY: FieldTypeItem = { name: "key", label: "Key", category: "kind", config_spec: [] };
const SECRET: FieldTypeItem = { name: "secret", label: "Secret", category: "kind", config_spec: [] };

describe("reading a field into the form", () => {
  it("keeps a rich type and its attributes", () => {
    const form = fieldForm(
      field({ type: "string", label: "Book title", attributes: { max_length: 200 } }),
    );
    expect(form.typeName).toBe("string");
    expect(form.label).toBe("Book title");
    expect(form.attrs).toEqual({ max_length: "200" });
    expect(form.calculated).toBe(false);
  });

  it("names a File by its kind, and reads its parameters as the attribute form's", () => {
    const form = fieldForm(
      field({
        name: "cover",
        kind: { type: "file", store: "uploads", folder: "covers", mime_allow: ["image/png"] },
      }),
    );
    // The picker says "file" even though the column is text — that is what the
    // field *is*, and what the admin chose when it was made.
    expect(form.typeName).toBe("file");
    // The discriminator is not a parameter, so it is not offered as one.
    expect(form.attrs).toEqual({
      store: "uploads",
      folder: "covers",
      mime_allow: '[\n  "image/png"\n]',
    });
  });

  it("reads a Key's target, and its absent summary as the select's empty value", () => {
    const form = fieldForm(
      field({
        name: "author",
        type: "int8",
        kind: {
          type: "key",
          target_table: "people",
          target_field: "id",
          summary_field: null,
        },
      }),
    );
    expect(form.typeName).toBe("key");
    expect(form.key).toEqual({
      target_table: "people",
      target_field: "id",
      summary_field: "",
    });
  });

  it("shows a calculated field as its value type, with the formula", () => {
    const form = fieldForm(
      field({ name: "area", type: "float", kind: { type: "calc", expression: "w * h" } }),
    );
    expect(form.calculated).toBe(true);
    expect(form.typeName).toBe("float");
    expect(form.expression).toBe("w * h");
  });

  it("carries a NOT NULL column through as not nullable", () => {
    expect(fieldForm(field({ nullable: false, required: true })).nullable).toBe(false);
  });
});

describe("saving an edited field", () => {
  it("re-states a rich type, so editing an attribute does not drop the type", () => {
    const form = fieldForm(field({ type: "string", attributes: { max_length: 200 } }));
    expect(updateFieldBody({ ...form, attrs: { max_length: "100" } }, STRING)).toEqual({
      label: "",
      description: "",
      primary_key: false,
      type: "string",
      attributes: { max_length: 100 },
    });
  });

  it("round-trips a File unchanged", () => {
    const stored = field({
      name: "cover",
      label: "Cover",
      kind: { type: "file", store: "uploads", folder: "covers", mime_allow: ["image/png"] },
    });
    expect(updateFieldBody(fieldForm(stored), FILE)).toEqual({
      label: "Cover",
      description: "",
      primary_key: false,
      type: "text",
      kind: {
        type: "file",
        store: "uploads",
        folder: "covers",
        mime_allow: ["image/png"],
      },
    });
  });

  it("round-trips a File's delete and conflict options", () => {
    const choice = (name: string, options: string[], def: string) => ({
      ...FILE.config_spec[0],
      name,
      label: name,
      required: false,
      default: def,
      options,
    });
    const withOptions: FieldTypeItem = {
      ...FILE,
      config_spec: [
        ...FILE.config_spec,
        choice("on_delete", ["keep", "remove_file"], "keep"),
        choice("on_conflict", ["overwrite", "suffix"], "overwrite"),
      ],
    };
    const stored = field({
      name: "cover",
      kind: {
        type: "file",
        store: "uploads",
        folder: null,
        mime_allow: [],
        on_delete: "remove_file",
        on_conflict: "suffix",
      },
    });
    const form = fieldForm(stored);
    expect(form.attrs.on_delete).toBe("remove_file");
    expect(form.attrs.on_conflict).toBe("suffix");
    const body = updateFieldBody(form, withOptions);
    expect(body.kind).toMatchObject({ on_delete: "remove_file", on_conflict: "suffix" });
  });

  it("round-trips a Key unchanged, and still omits an empty summary", () => {
    const stored = field({
      name: "author",
      type: "int8",
      kind: { type: "key", target_table: "people", target_field: "id", summary_field: null },
    });
    // No `type`: a key is stored as whatever its target is, which is the
    // server's to derive rather than this form's to re-assert.
    expect(updateFieldBody(fieldForm(stored), KEY)).toEqual({
      label: "",
      description: "",
      primary_key: false,
      kind: { type: "key", target_table: "people", target_field: "id" },
    });
  });

  it("names a basic type rather than clearing it, and sends no attributes", () => {
    const form = fieldForm(field({ label: "Title", description: "The book's title" }));
    expect(updateFieldBody(form, TEXT)).toEqual({
      label: "Title",
      description: "The book's title",
      primary_key: false,
      type: "text",
    });
  });

  it("never sends the name, which an edit cannot change", () => {
    const body = updateFieldBody(fieldForm(field({ nullable: false })), TEXT);
    expect(body).not.toHaveProperty("name");
  });
});

describe("whether a field accepts nulls, which an edit may change", () => {
  it("sends `required` only when the box was changed", () => {
    // An edit about the label must not be refused because of rows holding
    // nulls, so an untouched box says nothing and the server leaves it alone.
    const form = fieldForm(field({ nullable: true }));
    expect(updateFieldBody(form, TEXT)).not.toHaveProperty("required");
    expect(updateFieldBody({ ...form, nullable: false }, TEXT).required).toBe(true);

    const required = fieldForm(field({ nullable: false, required: true }));
    expect(updateFieldBody(required, TEXT)).not.toHaveProperty("required");
    expect(updateFieldBody({ ...required, nullable: true }, TEXT).required).toBe(false);
  });

  it("is editable on any stored field except a key or a calculated one", () => {
    expect(nullableEditable(fieldForm(field({})))).toBe(true);
    const key = fieldForm(field({ name: "id", primary_key: true, nullable: false }));
    expect(nullableEditable(key)).toBe(false);
    // Unticking the key box frees it again, in the same edit.
    expect(nullableEditable({ ...key, primaryKey: false })).toBe(true);
    const calc = fieldForm(field({ kind: { type: "calc", expression: "1" } }));
    expect(nullableEditable(calc)).toBe(false);
  });

  it("never sends it for a key, whose column is NOT NULL whatever the box says", () => {
    const key = fieldForm(field({ name: "id", primary_key: true, nullable: false }));
    expect(updateFieldBody({ ...key, nullable: true }, TEXT)).not.toHaveProperty("required");
  });
});

describe("adding a field", () => {
  it("sends the name, and required as the opposite of nullable", () => {
    const form = { ...newFieldForm("text"), name: "  title  ", nullable: false };
    expect(createFieldBody(form, TEXT)).toEqual({
      name: "title",
      label: "",
      description: "",
      required: true,
      primary_key: false,
      type: "text",
    });
  });

  it("never makes a calculated field required — there is no column to constrain", () => {
    const form = {
      ...newFieldForm("float"),
      name: "area",
      nullable: false,
      calculated: true,
      expression: " w * h ",
    };
    expect(createFieldBody(form, { ...TEXT, name: "float" })).toEqual({
      name: "area",
      label: "",
      description: "",
      required: false,
      primary_key: false,
      type: "float",
      kind: { type: "calc", expression: "w * h" },
    });
  });
});

describe("the primary key, which is a field like any other", () => {
  it("round-trips the key flag, so opening a key field and saving keeps it", () => {
    // The whole-object contract makes this load-bearing: `updateField` states
    // the key on every save, so a form that failed to read it back would drop
    // the table's key the next time anyone edited its label.
    const form = fieldForm(field({ name: "id", type: "int8", primary_key: true }));
    expect(form.primaryKey).toBe(true);
    expect(updateFieldBody(form, TEXT).primary_key).toBe(true);
    expect(updateFieldBody({ ...form, primaryKey: false }, TEXT).primary_key).toBe(false);
  });

  it("sends the key on a new field, which is the only way a table gets one", () => {
    const form = { ...newFieldForm("int"), name: "id", primaryKey: true, nullable: false };
    expect(createFieldBody(form, { ...TEXT, name: "int" }).primary_key).toBe(true);
  });

  it("refuses a calculated key, which has no column to key on", () => {
    const form = {
      ...newFieldForm("float"),
      name: "area",
      calculated: true,
      expression: "w * h",
      primaryKey: true,
    };
    expect(fieldFormError(form, TEXT)).toMatch(/calculated/);
  });
});

describe("what the form itself refuses", () => {
  it("wants a formula for a calculated field and a target for a key", () => {
    const calc = { ...newFieldForm("text"), name: "area", calculated: true };
    expect(fieldFormError(calc, TEXT)).toMatch(/formula/);
    expect(fieldFormError({ ...calc, expression: "w * h" }, TEXT)).toBeNull();

    const key = { ...newFieldForm("key"), name: "author" };
    expect(fieldFormError(key, KEY)).toMatch(/table and a field/);
    expect(
      fieldFormError(
        { ...key, key: { target_table: "people", target_field: "id", summary_field: "" } },
        KEY,
      ),
    ).toBeNull();
  });

  it("passes everything else to the server, which is the authority on it", () => {
    // A max_length of "nonsense" is a mistake, and the server's message names
    // the attribute — this form's job is only to send what was typed.
    const form = { ...newFieldForm("string"), name: "title", attrs: { max_length: "nonsense" } };
    expect(fieldFormError(form, STRING)).toBeNull();
  });
});

describe("what the key tick box says about filling the key in", () => {
  it("promises generation for the types that get it, and nothing for the rest", () => {
    const int = { ...newFieldForm("int"), name: "id", primaryKey: true };
    expect(keyValueNote(int)).toMatch(/numbers itself/);
    expect(keyValueNote({ ...int, typeName: "uuid" })).toMatch(/generates a UUID/);
    // No sensible value to invent for a text key, so it says so rather than
    // implying the database will produce one.
    expect(keyValueNote({ ...int, typeName: "text" })).toMatch(/whoever writes the row/);
  });

  it("describes a key that already exists by what its column does, not its type", () => {
    // The case introspection-first makes real: a table created outside the
    // admin UI whose `id` is an ordinary bigint nobody fills in. Its type says
    // "int", but ticking the box will not change it — it is already the key.
    const found = fieldForm(
      field({ name: "id", type: "int", sql_type: "int8", primary_key: true, required: true }),
    );
    expect(keyValueNote(found)).toMatch(/whoever writes the row/);

    const identity = fieldForm(
      field({
        name: "id",
        type: "int",
        sql_type: "int8",
        primary_key: true,
        required: true,
        generated: true,
      }),
    );
    expect(keyValueNote(identity)).toMatch(/numbers itself/);
  });

  it("promises generation to a column that is becoming the key", () => {
    // The same bigint column, not yet a key: ticking the box is what gives it
    // its identity, so here the type does predict what happens.
    const notYet = fieldForm(field({ name: "code", type: "int", sql_type: "int8" }));
    expect(keyValueNote({ ...notYet, primaryKey: true })).toMatch(/numbers itself/);
  });
});

describe("a default value", () => {
  it("is offered on every stored field, nullable or not, and never on a calculated one", () => {
    const base = newFieldForm("text");
    expect(defaultEditable({ ...base, nullable: true })).toBe(true);
    expect(defaultEditable({ ...base, nullable: false })).toBe(true);
    expect(defaultEditable({ ...base, nullable: true, primaryKey: true })).toBe(true);
    expect(defaultEditable({ ...base, nullable: false, calculated: true })).toBe(false);
  });

  it("is sent trimmed when a new required field has one", () => {
    const form = { ...newFieldForm("uuid"), name: "owner", nullable: false, defaultExpression: " user.id " };
    expect(createFieldBody(form, { ...TEXT, name: "uuid" })).toMatchObject({
      required: true,
      default_expression: "user.id",
    });
  });

  it("is kept when the field becomes nullable, and cleared by emptying the box", () => {
    const form = {
      ...fieldForm(field({ nullable: false, required: true, default_expression: "user && user.id" })),
      nullable: true,
    };
    expect(updateFieldBody(form, TEXT)).toMatchObject({ default_expression: "user && user.id" });
    expect(updateFieldBody({ ...form, defaultExpression: " " }, TEXT)).not.toHaveProperty("default_expression");
  });
});

describe("a secret field (TODO-imap M2)", () => {
  it("is named by its kind, and created and saved as that kind with no parameters", () => {
    const stored = field({ name: "imap_password", kind: { type: "secret" } });
    const form = fieldForm(stored);
    expect(form.typeName).toBe("secret");
    expect(updateFieldBody(form, SECRET)).toMatchObject({ kind: { type: "secret" } });

    const created = createFieldBody({ ...newFieldForm("secret"), name: "smtp_password" }, SECRET);
    expect(created).toMatchObject({ name: "smtp_password", kind: { type: "secret" } });
  });
});
