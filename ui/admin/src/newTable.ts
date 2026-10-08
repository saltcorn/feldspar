// The model behind the "New table" dialog — the half of it that is not React.
//
// One dialog, three ways to make a table: an empty one (a name, and the identity
// primary key the server gives it), one deduced from a CSV file (its fields
// named and typed by the file's header and contents, and every row in it
// imported), or one served by a **table provider** a module supplies (§8.3) —
// an RSS feed, a remote PostgreSQL table — whose columns and rows are the
// module's and whose settings are the form that module declared. They are one
// dialog because they answer one question — "what table do you want?" — and
// because the *name* is asked for in all three cases, so three buttons on the
// list page would have been three places to ask it.
//
// A fourth choice does not make a table at all: a **metadata table** is one of
// Saltcorn's own `_fd_*` tables, already in the database, added to the tables
// list so its rows and settings can be edited. It is here because it ends in the
// same place — a table on the list — but it asks nothing, not even a name: the
// table already has one, and a label can be given on its settings page.
//
// The rules here are the ones a test can pin without a browser: when the Create
// button may be pressed, and what a chosen file suggests the table be called.

/** Which of the things the dialog is making. `geo` is a table from a map file:
 * GeoJSON, a zipped Shapefile or a GeoPackage (analytics TODO A5.2). */
export type NewTableSource = "blank" | "csv" | "geo" | "provider" | "metadata";

/** What the dialog holds while it is open. */
export type NewTableForm = {
  name: string;
  source: NewTableSource;
  /** The chosen file, for `source === "csv"` or `"geo"`. */
  file: File | null;
  /** For `source === "geo"`: which Shapefile of a zip, or table of a
   * GeoPackage, when the file holds several. Empty for the only one. */
  layer: string;
  /** Which database to create the table in: `primary` for Saltcorn's own, else
   * the name of a connected database connection. */
  database: string;
  /** For `source === "provider"`: which provider, as `"module\u0000provider"`.
   *
   * One string rather than two fields because it is one `<select>`, and because
   * a provider is only ever chosen as a pair — the module is what routes the
   * call and the name is what it is called there. The separator is a NUL, which
   * cannot occur in a package name or a provider name. */
  provider: string;
  /** The values typed into the provider's own settings form, keyed by setting
   * name, as `SettingsFields` holds them. */
  providerConfig: Record<string, string>;
  /** For `source === "metadata"`: which `_fd_*` table to add to the list. */
  metadataTable: string;
};

/** The `database` that means Saltcorn's own. */
export const PRIMARY_DATABASE = "primary";

/** A dialog just opened. */
export const EMPTY_NEW_TABLE_FORM: NewTableForm = {
  name: "",
  source: "blank",
  file: null,
  layer: "",
  database: PRIMARY_DATABASE,
  provider: "",
  providerConfig: {},
  metadataTable: "",
};

/** The separator inside a `NewTableForm["provider"]` — see the field. */
const PROVIDER_SEPARATOR = "\u0000";

/** A provider's `(module, provider)` pair as the one value a `<select>` holds. */
export function providerKey(module: string, provider: string): string {
  return `${module}${PROVIDER_SEPARATOR}${provider}`;
}

/** The pair back out, or `null` when nothing is chosen. */
export function splitProviderKey(key: string): { module: string; provider: string } | null {
  const at = key.indexOf(PROVIDER_SEPARATOR);
  if (at < 0) return null;
  return { module: key.slice(0, at), provider: key.slice(at + 1) };
}

/** How a provider reads in the chooser: its own name, with the package that
 * supplies it beside it — because two modules may each supply a provider called
 * `Table`, and the module is half of what the admin is choosing. */
export function providerLabel(module: string, provider: string): string {
  return `${provider} (${module})`;
}

/**
 * The databases a new table may be created in: Saltcorn's own, then every
 * **connected** connection, in the order the list gives them.
 *
 * Connections that are not connected are left out, and that is the difference
 * between this and the Connections screen's list. There, a connection that
 * cannot be dialled must be shown, because editing it is the repair. Here it
 * would be a choice that can only fail — the server has no driver to send the
 * `CREATE TABLE` to — and a chooser whose entries are not all choosable is worse
 * than one with fewer entries.
 */
export function creatableDatabases(
  connections: Array<{ name: string; connected: boolean }>,
): string[] {
  return [PRIMARY_DATABASE, ...connections.filter((c) => c.connected).map((c) => c.name)];
}

/** How a database reads in the chooser. */
export function databaseLabel(name: string): string {
  return name === PRIMARY_DATABASE ? "Saltcorn's own database" : name;
}

/**
 * Why this form cannot be submitted yet, or `null` when it can.
 *
 * A message rather than a boolean: the same answer disables the button and says
 * what is missing, and "Create is greyed out and I cannot tell why" is the
 * failure mode of every dialog that only returns the boolean.
 */
export function newTableError(form: NewTableForm): string | null {
  // A metadata table already has its name and is in Saltcorn's own database,
  // so the one question is which.
  if (form.source === "metadata")
    return form.metadataTable ? null : "Choose the metadata table to add.";
  if (!form.name.trim()) return "The table needs a name.";
  if (form.source === "csv" && !form.file) return "Choose a CSV file to create the table from.";
  if (form.source === "geo" && !form.file)
    return "Choose a GeoJSON, zipped Shapefile or GeoPackage file to create the table from.";
  if (form.source === "provider" && !splitProviderKey(form.provider))
    return "Choose the table provider that will serve this table's rows.";
  // The database question does not apply to a provided table — its rows are not
  // in one — so it is not asked and not checked.
  if (form.source !== "provider" && !form.database.trim())
    return "Choose which database to create the table in.";
  return null;
}

/**
 * The table name a chosen file suggests: its base name, as an identifier.
 *
 * Offered only into an *empty* name box (see `Tables.tsx`), so it is a
 * suggestion and never a correction — an admin who has already typed a name
 * keeps it. The transformation is the server's own field-name rule (`sc-api`'s
 * `label_to_name`): lower case, spaces and dashes as underscores, punctuation
 * dropped, and a leading digit pushed behind an underscore, because none of
 * those can be a SQL identifier.
 */
export function tableNameFromFile(fileName: string): string {
  const dot = fileName.lastIndexOf(".");
  const base = dot > 0 ? fileName.slice(0, dot) : fileName;
  let name = "";
  for (const c of base.trim()) {
    if (c === " " || c === "-" || c === "_") name += "_";
    else if (/[A-Za-z0-9]/.test(c)) name += c.toLowerCase();
  }
  if (/^[0-9]/.test(name)) name = `_${name}`;
  return name;
}

/**
 * What to say after a table was made from a file.
 *
 * Creating from a CSV is all-or-nothing on the server — a row it will not take
 * drops the table rather than leaving a half-filled one (§13.1) — so there is
 * one number to report here, not the two an import into an existing table has.
 */
export function importedMessage(table: string, inserted: number): string {
  return `${inserted} row${inserted === 1 ? "" : "s"} imported into ${table}.`;
}

/** The file types the map-file choice accepts, for the file input. */
export const GEO_FILE_ACCEPT = ".geojson,.json,.zip,.gpkg,application/geo+json,application/zip";

/**
 * Bytes as base64, which is how a binary file crosses the JSON endpoint
 * (`createTableFromGeoFile`). In slices, because `String.fromCharCode` takes its
 * characters as arguments and a large file would overflow the call stack.
 */
export function toBase64(bytes: Uint8Array): string {
  let binary = "";
  const slice = 0x8000;
  for (let i = 0; i < bytes.length; i += slice) {
    binary += String.fromCharCode(...bytes.subarray(i, i + slice));
  }
  return btoa(binary);
}
