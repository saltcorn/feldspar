// The tables and their fields, for writing a posterior back (Stan TODO §16):
// the target table's number fields are what a statistic can go into. The
// admin UI's `codeTypes.ts` reads the same thing for its code completions;
// this is the part of it the model editor needs (analytics TODO A3.6).

import { api } from "../api";

/** One field of a table. */
export type ColumnInfo = { name: string; type: string; sqlType: string };

/** One table and its fields. */
export type TableInfo = { name: string; columns: ColumnInfo[] };

/** The SQL and declared types whose values are numbers. */
const NUMERIC = new Set([
  "int",
  "integer",
  "bigint",
  "smallint",
  "float",
  "double precision",
  "real",
  "decimal",
  "numeric",
]);

/** The fields a statistic can be written into: those holding numbers. */
export function numericColumns(columns: ColumnInfo[]): string[] {
  return columns.filter((c) => NUMERIC.has(c.type) || NUMERIC.has(c.sqlType)).map((c) => c.name);
}

/** Every table with its fields — one request per table, a table whose fields
 * cannot be read kept with none. */
async function loadCatalog(): Promise<TableInfo[]> {
  const tables = await api.listTables();
  return await Promise.all(
    tables.map(async (table): Promise<TableInfo> => {
      try {
        const fields = await api.listFields(table.name);
        return {
          name: table.name,
          columns: fields.map((f) => ({ name: f.name, type: f.type, sqlType: f.sql_type })),
        };
      } catch {
        return { name: table.name, columns: [] };
      }
    }),
  );
}

let cache: Promise<TableInfo[]> | null = null;

/** [`loadCatalog`], once per page. */
export function catalog(): Promise<TableInfo[]> {
  cache ??= loadCatalog();
  return cache;
}
