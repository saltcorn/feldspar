// The model behind the constraint editor (design §5) — the half of it that is
// not React.
//
// A table's constraints are five different things wearing one shape: a
// jointly-unique key, an index, a full-text index, a vector index and a row
// constraint. They
// share a card and a modal because an admin asks one question of them ("what
// rules does this table keep?"), and they share one request body because the
// endpoint does — the `type` decides which of its fields are read.
//
// What they do not share is what the form asks. Ticking boxes for the fields
// that are unique together, picking one field to index, choosing a language,
// writing a formula: four questions, and the form shows exactly the one that
// belongs to the chosen kind rather than a superset with most of it greyed out.
//
// Everything here is a plain function over plain values, so the round trip — a
// kind and some answers, into a request body, and a listed constraint back into
// a sentence — is assertable without a browser.

import type { CreateConstraintRequest, ListConstraintsResponse } from "./client";

/** One constraint as `listConstraints` reports it. */
export type ConstraintItem = ListConstraintsResponse[number];

/** The five kinds an admin can add. */
export type ConstraintType = "unique" | "index" | "full_text_search" | "vector_index" | "formula";

/** What the constraint modal holds while it is open. */
export type ConstraintForm = {
  type: ConstraintType;
  /** The fields ticked for a unique constraint, or the one field to index
   * (the one vector field, for a vector index). */
  fields: string[];
  /** The text-search configuration for a full-text index. */
  language: string;
  /** The row constraint's formula, and the short name it is known by. */
  formula: string;
  name: string;
  /** Shown to whoever breaks the rule, in the admin's own words. */
  errorMessage: string;
};

/**
 * The text-search configurations offered for a full-text index.
 *
 * A short list of Postgres's own built-in ones rather than a query for
 * `pg_ts_config`: an installation with a custom configuration is rare, and the
 * admin who has one can say so — the field accepts anything the database
 * accepts, and the database is what refuses a name it does not know.
 */
export const TEXT_SEARCH_LANGUAGES = [
  "simple",
  "english",
  "danish",
  "dutch",
  "finnish",
  "french",
  "german",
  "hungarian",
  "italian",
  "norwegian",
  "portuguese",
  "romanian",
  "russian",
  "spanish",
  "swedish",
  "turkish",
];

/** A fresh form for adding a constraint of `type`. */
export function newConstraintForm(type: ConstraintType): ConstraintForm {
  return {
    type,
    fields: [],
    language: "english",
    formula: "",
    name: "",
    errorMessage: "",
  };
}

/**
 * Why this form cannot be submitted yet, or `null` when it can.
 *
 * The server checks all of this again and says so better (it knows the table);
 * this is only so the button is disabled with a reason rather than enabled onto
 * a refusal.
 */
export function constraintFormError(form: ConstraintForm): string | null {
  if (form.type === "unique" && form.fields.length === 0) {
    return "Tick the fields that should be unique together.";
  }
  if (form.type === "index" && form.fields.length !== 1) {
    return "Choose the field to index.";
  }
  if (form.type === "vector_index" && form.fields.length !== 1) {
    return "Choose the vector field to index.";
  }
  if (form.type === "full_text_search" && !form.language.trim()) {
    return "Choose a language for the search index.";
  }
  if (form.type === "formula") {
    if (!form.name.trim()) {
      return "Give the constraint a short name.";
    }
    if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(form.name.trim())) {
      return "The name must start with a letter and hold only letters, digits and underscores.";
    }
    if (!form.formula.trim()) {
      return "Write the formula every row must satisfy.";
    }
  }
  return null;
}

/**
 * The request body this form means.
 *
 * Only what the chosen kind uses is sent: a `language` on a unique constraint
 * would be a value the server reads for no kind and stores for none, which is
 * the sort of thing that later looks like a feature.
 */
export function createConstraintBody(form: ConstraintForm): CreateConstraintRequest {
  const body: CreateConstraintRequest = { type: form.type };
  if (form.type === "unique" || form.type === "index" || form.type === "vector_index") {
    body.fields = form.fields;
  }
  if (form.type === "full_text_search") {
    body.language = form.language.trim();
  }
  if (form.type === "formula") {
    body.formula = form.formula.trim();
    body.name = form.name.trim();
  }
  // A message belongs to whichever kind can be *violated* by a row. An index
  // cannot be, so the form does not ask and the body does not carry one.
  if (
    form.type !== "index" &&
    form.type !== "full_text_search" &&
    form.type !== "vector_index" &&
    form.errorMessage.trim()
  ) {
    body.error_message = form.errorMessage.trim();
  }
  return body;
}

/** The human name of a constraint kind, for the list and the picker. */
export function constraintTypeLabel(type: string): string {
  if (type === "unique") return "Jointly unique";
  if (type === "index") return "Index";
  if (type === "full_text_search") return "Full-text search";
  if (type === "vector_index") return "Vector index";
  if (type === "formula") return "Row constraint";
  return type;
}

/**
 * What a listed constraint *is*, in one line — the "What" column of the list.
 *
 * One function rather than a branch in the table, because the same sentence is
 * wanted in the delete confirmation, and two spellings of "what this constraint
 * is" would eventually describe different things.
 */
export function constraintSummary(constraint: ConstraintItem): string {
  switch (constraint.type) {
    case "unique":
      return constraint.fields.join(" + ");
    case "index":
      // An index written by hand may be over an expression or over several
      // columns; it is still an index, and showing it as it is beats hiding it.
      return constraint.expression ?? constraint.fields.join(", ");
    case "full_text_search":
      return `every text field (${constraint.language ?? "unknown"})`;
    case "vector_index":
      return `${constraint.fields[0] ?? ""} (nearest by cosine distance)`;
    case "formula":
      return constraint.formula ?? "";
    default:
      return "";
  }
}
