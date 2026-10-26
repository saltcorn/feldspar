/**
 * The constraint editor's model: four kinds through one form and one body.
 *
 * What is worth pinning down is exactly where the four differ, because that is
 * where a shared shape goes wrong:
 *
 *   - each kind sends **only** the fields it uses, so a value the admin typed
 *     for another kind cannot ride along and be stored for nothing;
 *   - a message belongs to the kinds a *row* can violate, and an index is not
 *     one of them;
 *   - a row constraint is the one kind that needs a name, because it is the one
 *     whose identity cannot be derived from its fields;
 *   - and the summary a listed constraint renders as says what it *is*,
 *     including for a constraint Saltcorn did not create.
 */

import { describe, expect, it } from "vitest";

import {
  constraintFormError,
  constraintSummary,
  constraintTypeLabel,
  createConstraintBody,
  newConstraintForm,
  type ConstraintItem,
} from "./constraintForm";

/** A constraint as `listConstraints` reports it, with only what a test states. */
function listed(over: Partial<ConstraintItem>): ConstraintItem {
  return {
    name: "sc_uq_book_title",
    type: "unique",
    fields: ["title"],
    expression: null,
    method: null,
    language: null,
    formula: null,
    error_message: null,
    managed: true,
    ...over,
  };
}

describe("createConstraintBody", () => {
  it("sends the fields that are unique together, and the message", () => {
    const body = createConstraintBody({
      ...newConstraintForm("unique"),
      fields: ["author", "title"],
      errorMessage: "that author already has a book by that name",
    });
    expect(body).toEqual({
      type: "unique",
      fields: ["author", "title"],
      error_message: "that author already has a book by that name",
    });
  });

  it("sends one field for an index, and no message — a row cannot violate one", () => {
    const body = createConstraintBody({
      ...newConstraintForm("index"),
      fields: ["author"],
      // Typed while the admin was on another kind, and not carried across.
      errorMessage: "ignored",
      formula: "pages > 0",
    });
    expect(body).toEqual({ type: "index", fields: ["author"] });
  });

  it("sends the language for a full-text index and nothing else", () => {
    const body = createConstraintBody({
      ...newConstraintForm("full_text_search"),
      language: "german",
      fields: ["title"],
    });
    expect(body).toEqual({ type: "full_text_search", language: "german" });
  });

  it("sends the one vector field for a vector index, and no message", () => {
    const body = createConstraintBody({
      ...newConstraintForm("vector_index"),
      fields: ["embedding"],
      errorMessage: "ignored",
    });
    expect(body).toEqual({ type: "vector_index", fields: ["embedding"] });
    expect(constraintFormError(newConstraintForm("vector_index"))).toBe(
      "Choose the vector field to index.",
    );
    expect(constraintTypeLabel("vector_index")).toBe("Vector index");
  });

  it("sends the formula, its name and its message, trimmed", () => {
    const body = createConstraintBody({
      ...newConstraintForm("formula"),
      name: "  paid  ",
      formula: "  salary > 0  ",
      errorMessage: "  we don't work for free  ",
    });
    expect(body).toEqual({
      type: "formula",
      formula: "salary > 0",
      name: "paid",
      error_message: "we don't work for free",
    });
  });
});

describe("constraintFormError", () => {
  it("names what is missing, per kind", () => {
    expect(constraintFormError(newConstraintForm("unique"))).toMatch(/unique together/);
    expect(constraintFormError(newConstraintForm("index"))).toMatch(/field to index/);
    expect(constraintFormError(newConstraintForm("formula"))).toMatch(/short name/);
    expect(
      constraintFormError({ ...newConstraintForm("formula"), name: "paid" }),
    ).toMatch(/formula/);
    // A name that is not an identifier: the trigger is named after it, and
    // Postgres would refuse the DDL rather than the form.
    expect(
      constraintFormError({ ...newConstraintForm("formula"), name: "not a name", formula: "x" }),
    ).toMatch(/letters, digits/);
  });

  it("passes a form that is ready", () => {
    expect(
      constraintFormError({ ...newConstraintForm("unique"), fields: ["a", "b"] }),
    ).toBeNull();
    expect(constraintFormError({ ...newConstraintForm("index"), fields: ["a"] })).toBeNull();
    expect(constraintFormError(newConstraintForm("full_text_search"))).toBeNull();
    expect(
      constraintFormError({
        ...newConstraintForm("formula"),
        name: "paid",
        formula: "salary > 0",
      }),
    ).toBeNull();
  });

  it("refuses two fields for an index — that is a constraint we do not create", () => {
    expect(
      constraintFormError({ ...newConstraintForm("index"), fields: ["a", "b"] }),
    ).toMatch(/field to index/);
  });
});

describe("constraintSummary", () => {
  it("says what each kind is", () => {
    expect(constraintSummary(listed({ fields: ["author", "title"] }))).toBe("author + title");
    expect(
      constraintSummary(listed({ type: "index", fields: ["author"], method: "btree" })),
    ).toBe("author");
    expect(
      constraintSummary(listed({ type: "full_text_search", fields: [], language: "english" })),
    ).toBe("every text field (english)");
    expect(
      constraintSummary(listed({ type: "formula", fields: [], formula: "pages > 0" })),
    ).toBe("pages > 0");
  });

  it("shows an index somebody wrote by hand as what it is", () => {
    // Not every index in a database was made here (§9): one over an expression
    // is listed with the expression rather than with an empty column list.
    expect(
      constraintSummary(
        listed({
          name: "book_lower_title",
          type: "index",
          fields: [],
          expression: "lower(title)",
          managed: false,
        }),
      ),
    ).toBe("lower(title)");
  });
});

describe("constraintTypeLabel", () => {
  it("names the four kinds and passes anything else through", () => {
    expect(constraintTypeLabel("unique")).toBe("Jointly unique");
    expect(constraintTypeLabel("index")).toBe("Index");
    expect(constraintTypeLabel("full_text_search")).toBe("Full-text search");
    expect(constraintTypeLabel("formula")).toBe("Row constraint");
    expect(constraintTypeLabel("exclusion")).toBe("exclusion");
  });
});
