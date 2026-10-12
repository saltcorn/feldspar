// The Backup tab's model: what a backup includes, as data the screen renders and
// the tests can hold.
//
// The screen itself is two buttons and a dialog; what is worth pinning down
// without a browser is the *arithmetic* of the selection, because two rules in it
// are easy to write and easy to get subtly wrong:
//
// - **rows cannot be included without their table's metadata.** The server
//   narrows the payload it is sent (a client bug must not produce rows nothing
//   can read), but a dialog that lets an admin tick a combination the server will
//   quietly drop is a dialog that lies. So unticking a table takes its rows with
//   it, here, in front of them.
// - **the same dialog drives the backup and the restore.** The server describes
//   what is available in one shape whether it comes from this installation or
//   from an uploaded file's manifest, so everything below takes that shape and
//   knows nothing about which it is looking at.

import type { GetBackupOptionsResponse } from "./client";
import type { MultiChoice } from "./multiSelect";

/** Everything that could go into a backup — this server's, or an uploaded
 * file's. */
export type BackupContents = GetBackupOptionsResponse["available"];

/** What one backup or restore includes. */
export type BackupSelection = GetBackupOptionsResponse["include"];

/** One table, application or file store on offer. */
export type BackupItem = BackupContents["tables"][number];

/** An empty offering — what a screen holds before the server has answered. */
export const NO_CONTENTS: BackupContents = {
  tables: [],
  applications: [],
  file_stores: [],
  users: 0,
  modules: 0,
  db_connections: 0,
  streams: 0,
  datasets: 0,
  models: 0,
  workspaces: 0,
  fits: 0,
  llm_providers: 0,
  agents: 0,
  code_libraries: 0,
  triggers: 0,
  views: 0,
  pages: 0,
  ssl: false,
  settings: false,
};

/** Whether there is anything for the Analytics choice to carry. */
export function hasAnalytics(contents: BackupContents): boolean {
  return contents.datasets > 0 || contents.models > 0 || contents.workspaces > 0;
}

/** Everything on offer, selected. The restore dialog's starting point, and the
 * backup dialog's until the admin has narrowed it. */
export function everything(contents: BackupContents): BackupSelection {
  const names = (items: BackupItem[]) => items.map((i) => i.name);
  return {
    tables: names(contents.tables),
    // Only the tables whose rows are actually there. A count is null for a table
    // whose data a backup did not carry, and a tick box that would restore nothing
    // is worse than no tick box.
    table_data: names(contents.tables.filter((t) => t.count !== null && t.count !== undefined)),
    applications: names(contents.applications),
    file_stores: names(contents.file_stores),
    users: contents.users > 0,
    modules: contents.modules > 0,
    db_connections: contents.db_connections > 0,
    streams: contents.streams > 0,
    analytics: hasAnalytics(contents),
    fits: contents.fits > 0 && hasAnalytics(contents),
    llm_providers: contents.llm_providers > 0,
    agents: contents.agents > 0,
    code_libraries: contents.code_libraries > 0,
    triggers: contents.triggers > 0,
    views: contents.views > 0,
    pages: contents.pages > 0,
    ssl: contents.ssl,
    settings: contents.settings,
  };
}

/** How many rows or files an item holds, as the line under its name. `null` is
 * "not counted" — a live file store, which would have to be walked to answer —
 * and shows the item's own description instead of a wrong number. */
export function itemDescription(item: BackupItem, unit: string): string {
  if (item.count === null || item.count === undefined) {
    return item.label === item.name ? "" : item.label;
  }
  const plural = item.count === 1 ? unit : `${unit}s`;
  return `${item.count} ${plural}`;
}

/** The choices a multi-select is given for a list of items. */
export function choices(items: BackupItem[], unit: string): MultiChoice[] {
  return items.map((item) => ({
    value: item.name,
    label: item.name,
    description: itemDescription(item, unit),
  }));
}

/** The tables whose *rows* can be chosen: only the ones whose metadata is in.
 *
 * This is the "data needs metadata" rule as the dialog shows it — the rows
 * picker offers what the metadata picker has left. */
export function dataChoices(contents: BackupContents, selection: BackupSelection): MultiChoice[] {
  return choices(
    contents.tables.filter((t) => selection.tables.includes(t.name)),
    "row",
  );
}

/** Set which tables' metadata is included, dropping the rows of any table that
 * just left. */
export function withTables(selection: BackupSelection, tables: string[]): BackupSelection {
  return {
    ...selection,
    tables,
    table_data: selection.table_data.filter((t) => tables.includes(t)),
  };
}

/** Set which tables' rows are included, ignoring any whose metadata is not. */
export function withTableData(selection: BackupSelection, data: string[]): BackupSelection {
  return { ...selection, table_data: data.filter((t) => selection.tables.includes(t)) };
}

/** Turn the Analytics choice on or off. Fits belong to models, so turning it
 * off takes the fits with it — the "data needs metadata" rule again. */
export function withAnalytics(selection: BackupSelection, analytics: boolean): BackupSelection {
  return { ...selection, analytics, fits: analytics && selection.fits };
}

/** Whether the selection would produce an empty backup — the one state the
 * confirm button is not offered in, since a zip of nothing is not what anybody
 * pressed the button for. */
export function isEmpty(selection: BackupSelection): boolean {
  return (
    selection.tables.length === 0 &&
    selection.applications.length === 0 &&
    selection.file_stores.length === 0 &&
    !selection.users &&
    !selection.modules &&
    !selection.db_connections &&
    !selection.streams &&
    !selection.analytics &&
    !selection.llm_providers &&
    !selection.agents &&
    !selection.code_libraries &&
    !selection.triggers &&
    !selection.ssl &&
    !selection.settings
  );
}

/** What is ticked, in a sentence — shown under the button so an admin can see
 * their remembered choice without opening the dialog. */
export function summarise(selection: BackupSelection, contents: BackupContents): string {
  const parts: string[] = [];
  const count = (n: number, one: string, many: string) =>
    n === 1 ? `1 ${one}` : `${n} ${many}`;
  if (selection.tables.length > 0) {
    const all = selection.tables.length === contents.tables.length;
    const tables = count(selection.tables.length, "table", "tables");
    parts.push(all ? `all ${tables}` : tables);
    if (selection.table_data.length === 0) parts.push("no rows");
    else if (selection.table_data.length < selection.tables.length)
      parts.push(`rows of ${selection.table_data.length}`);
  }
  if (selection.applications.length > 0) {
    parts.push(count(selection.applications.length, "application", "applications"));
    // Views and pages live inside applications, so they are only worth saying
    // when some application is ticked to carry them.
    if (selection.views && contents.views > 0) parts.push("views");
    if (selection.pages && contents.pages > 0) parts.push("pages");
  }
  if (selection.file_stores.length > 0)
    parts.push(count(selection.file_stores.length, "file store", "file stores"));
  if (selection.users) parts.push("users");
  if (selection.modules) parts.push("modules");
  if (selection.db_connections) parts.push("database connections");
  if (selection.streams) parts.push("streams");
  if (selection.analytics) parts.push(selection.fits ? "analytics with fits" : "analytics");
  if (selection.llm_providers) parts.push("LLM providers");
  if (selection.agents) parts.push("agents");
  if (selection.code_libraries) parts.push("code libraries");
  if (selection.triggers) parts.push("triggers");
  if (selection.ssl) parts.push("SSL settings");
  if (selection.settings) parts.push("other settings");
  return parts.length === 0 ? "Nothing selected." : `${parts.join(", ")}.`;
}
